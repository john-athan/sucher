// Directory browser: a fast, two-pane file navigator. Left pane is the entry
// list for the current directory; right pane previews the selection (child
// listing for folders, head of the file for text, dimensions for images,
// metadata otherwise). Enter opens a file in its viewer and returns here.

use crate::config::{IconMode, Layout};
use crate::format::Format;
use crate::git::{self, GitStatus};
use crate::media::{self, ImagePane};
use crate::{fileop, highlight, icons, query, theme, typeahead};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use image::DynamicImage;
// The ratatui layout builder is aliased to `RtLayout` so `Layout` can name the
// browser's own pane-layout mode (auto/miller/double) from `config`.
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout as RtLayout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget,
};
use ratatui::{DefaultTerminal, Frame};
use std::cmp::Ordering;
use std::fs;
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

struct Entry {
    name: String,
    path: PathBuf,
    kind: Format,
    size: u64,
    modified: Option<SystemTime>,
}

enum Mode {
    Browse,
    Filter,
    /// Recursive, streaming, content-aware search (ADR 0007). A DISTINCT mode from
    /// the local `/` filter (D1): its own key (`S`), its own text buffer, its own
    /// background tree walk. Present only while `App.search` is `Some`.
    Search,
    /// A file-operation overlay owns the keyboard (ADR 0017 D5). ONE arm carries
    /// both overlays rather than two sibling arms, because what they share is the
    /// part `Mode` is actually deciding: they are modal popups drawn over the
    /// browse layout, they swallow every key and every click, and search never
    /// runs underneath either of them (D9). Every `match self.mode` site therefore
    /// gains exactly one arm and asks "is an operation overlay up?" once, not two
    /// arms that could drift apart as the remaining operations land. What differs
    /// between them is what the popup is *for*, which is what [`OpView`] carries.
    Op(OpView),
    /// An inline prompt for a typed name: rename, or create (ADR 0017 D4).
    ///
    /// A text-input surface like [`Mode::Filter`], so typeahead never applies and
    /// every printable key spells the name. What it does NOT share with the
    /// filter is an append-only buffer: a rename opens pre-filled with an
    /// existing name and the edit the user came to make is usually in the middle
    /// of it, so this one carries a real cursor (see [`crate::lineedit`]).
    ///
    /// Deliberately not an [`OpView`]: those two are modal popups drawn over the
    /// listing that swallow every key and every click, while this is one line at
    /// the bottom of an otherwise ordinary browse frame. It also never reaches
    /// the confirm overlay at all, because the name the user just typed IS the
    /// authorisation (ADR 0017 D5).
    Input(Prompt),
}

/// The inline rename/create prompt: what is being asked, and the buffer it is
/// being typed into.
struct Prompt {
    ask: Ask,
    edit: crate::lineedit::LineEdit,
}

/// Which question an inline prompt is asking.
///
/// Both arms carry the path they resolved when the prompt OPENED rather than
/// re-reading it on submit. The browse layout stays live underneath, so a click
/// in the listing can still move the cursor while a name is being typed, and a
/// rename that quietly retargeted itself to whatever row the cursor ended on
/// would be the worst kind of surprise this ADR exists to prevent.
#[derive(Clone)]
enum Ask {
    /// Rename this exact path (ADR 0017 D3: exactly one target).
    Rename { path: PathBuf },
    /// Create one entry in this directory. A trailing '/' on the typed name
    /// makes it a directory (ADR 0017 D4).
    Create { parent: PathBuf },
}

impl Ask {
    /// The label the status line leads with, in the same shape as the filter's
    /// `/` prefix. The two must not look alike: `r` and `a` sit one key apart on
    /// the keyboard and this label is the only thing on screen that says which
    /// of the two prompts is open.
    fn prefix(&self) -> &'static str {
        match self {
            Ask::Rename { .. } => "rename: ",
            Ask::Create { .. } => "new: ",
        }
    }

    /// The rules and keys the prompt teaches while it is open.
    ///
    /// Create names the trailing-slash rule because the prompt is the ONLY place
    /// it can be discovered: nothing about an empty field suggests that typing
    /// `notes/` makes a folder, and a rule nobody can find is a rule nobody has.
    /// Rename has no such hidden rule to teach, so it names only its two keys.
    fn hint(&self) -> &'static str {
        match self {
            Ask::Rename { .. } => "[Enter] rename  [Esc] cancel",
            Ask::Create { .. } => "end with / for a folder    [Enter] create  [Esc] cancel",
        }
    }
}

/// Which file-operation overlay is on screen.
enum OpView {
    /// A fully resolved plan waiting to be authorised. Shown BEFORE anything
    /// happens, which is the whole of ADR 0017 D5: the user sees the outcome,
    /// including every collision-dodging rename, before a byte moves.
    Confirm(Pending),
    /// A finished run that had something to say beyond its totals: steps that
    /// failed, notes that are not failures, or both. A partial result is reported,
    /// never swallowed (ADR 0009), so this is deliberately an overlay and not a
    /// status line: a one-line summary of four failures would be exactly the
    /// silent truncation that doctrine exists to forbid.
    ///
    /// Named for the whole report rather than for its failures, because an undo
    /// reaches here with an empty failure list and one note (ADR 0017 D8): the
    /// paths only the system trash can bring back. A variant called `Failures`
    /// would have made that arrival look like an error.
    Report(fileop::Report),
}

/// A plan on the confirm overlay, plus what it would take to plan it again.
///
/// The overwrite toggle (`o`, ADR 0017 D5) re-plans the whole batch under the
/// other conflict policy, and it has to do so as a PURE recomputation: `collect`
/// walks the entire source tree, and the user pressing `o` is toggling a display
/// of a decision, not asking for a fresh look at the disk. So the planner's
/// inputs are kept here from the moment the overlay opens, and `o` calls
/// [`fileop::plan`] again with nothing else changed.
struct Pending {
    /// The plan currently on screen: what the user is being asked to authorise.
    plan: fileop::Plan,
    /// `None` for an operation with no destination directory, which is trash
    /// (ADR 0017 D7): nothing collides, so there is nothing for `o` to toggle.
    ///
    /// Boxed because `Mode` is stored inline in `App` and is otherwise a handful
    /// of bytes: an enumerated batch's worth of paths sitting behind a pointer
    /// costs one allocation while an overlay is up, and keeps every `Mode::Browse`
    /// from carrying that width around for the life of the browser.
    inputs: Option<Box<Replan>>,
}

/// The two operations that arrive at the confirm overlay with a destination, and
/// therefore the only two whose collisions the overwrite toggle can speak about.
///
/// A dedicated pair rather than storing a [`fileop::Kind`] and matching on it,
/// because `Kind` also names rename, create and trash, and reconstructing an
/// [`fileop::Op`] from it would need an arm for three cases that cannot occur.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transfer {
    Copy,
    Move,
}

impl Transfer {
    /// Which transfer a clipboard load asks for: a cut moves, a yank copies.
    fn of(cut: bool) -> Self {
        if cut {
            Transfer::Move
        } else {
            Transfer::Copy
        }
    }

    fn op(self, sources: Vec<fileop::Source>, dest: PathBuf) -> fileop::Op {
        match self {
            Transfer::Copy => fileop::Op::Copy { sources, dest },
            Transfer::Move => fileop::Op::Move { sources, dest },
        }
    }
}

/// Everything [`fileop::plan`] was handed the first time, kept so the overwrite
/// toggle can hand it over again unchanged except for the policy.
///
/// `cwd` is carried rather than re-read from `App` at toggle time so that
/// re-planning is a function of this struct alone and can be tested without a
/// browser. It cannot go stale while the overlay is up in any case: the overlay
/// is modal, so no key that changes directory can be pressed underneath it.
struct Replan {
    transfer: Transfer,
    sources: Vec<fileop::Source>,
    dest: PathBuf,
    /// The UNFILTERED destination listing read at plan time (ADR 0017 D5); see
    /// [`dest_listing`] for why nothing the browser already holds will do.
    dest_listing: Vec<String>,
    missing: Vec<PathBuf>,
    cwd: PathBuf,
}

impl Replan {
    /// Resolve the same batch under `policy`. Pure: no filesystem, no `collect`,
    /// no clock.
    ///
    /// The sources are cloned because [`fileop::Op`] takes them by value and this
    /// struct has to survive for the next toggle. That is a copy of an already
    /// enumerated tree once per keypress, which is cheap next to the walk that
    /// produced it and is the reason `o` never touches the disk.
    fn plan(&self, policy: fileop::Conflict) -> Result<fileop::Plan, fileop::Refusal> {
        fileop::plan(
            self.transfer.op(self.sources.clone(), self.dest.clone()),
            &fileop::PlanCtx {
                dest_listing: &self.dest_listing,
                cwd: &self.cwd,
                missing: &self.missing,
                policy,
            },
        )
    }
}

/// What `y` and `X` put aside for `p` (ADR 0017 D4).
///
/// Like the mark set, and for the same reason (D3), this survives a directory
/// change: yanking here and pasting there is the entire point of having a
/// clipboard at all. The paths are absolute and are re-collected at paste time,
/// so entries that vanished in between are pruned and reported rather than
/// silently dropped.
struct Clip {
    /// A cut pastes as a move; a yank pastes as a copy.
    cut: bool,
    paths: Vec<PathBuf>,
}

/// The browser's half of the one operation in flight (ADR 0017 D2).
///
/// It is created the moment a plan is authorised and dropped when the run's
/// `Done` arrives, so it lives and dies with `App::op`. Besides the streamed
/// counters it carries the paths the run consumes, captured from the plan while
/// the plan is still owned here: the report that comes back names what happened,
/// not which marks asked for it, and the marks have to be dropped on completion
/// (see [`App::finish_op`]).
struct InFlight {
    /// The plan's own one-line summary, captured at authorisation time so the
    /// status line names the operation without re-deriving its verb.
    label: String,
    /// Items the plan resolved: the denominator the streamed count climbs to.
    total: usize,
    /// Every path this run acts on, plus the marks that had already vanished.
    /// Both are consumed by the operation and both are unmarked when it ends.
    targets: Vec<PathBuf>,
    /// The name the operation set out to leave behind, when it names exactly one
    /// (see [`landing_name`]). Decided at authorisation time, while the plan is
    /// still here to be asked, because the report that comes back names what
    /// happened and not what the cursor should do about it.
    landing: Option<String>,
    /// Cumulative counters as last reported by `Msg::Progress`.
    items: usize,
    bytes: u64,
    /// The path the worker was on when it last reported.
    current: PathBuf,
}

impl InFlight {
    /// The four things a run decides for itself, with the three streamed counters
    /// at the values they hold before a single `Msg::Progress` has arrived.
    ///
    /// A constructor rather than two literals, because a forward operation reads
    /// its four from a [`fileop::Plan`] and an undo reads them from a
    /// [`fileop::Journal`] (ADR 0017 D8), and the only thing the two shapes have in
    /// common is what has NOT happened yet. Keeping that zero state in one place is
    /// what stops the undo path from quietly starting its progress line at a
    /// different origin than the paste path.
    fn new(label: String, total: usize, targets: Vec<PathBuf>, landing: Option<String>) -> Self {
        InFlight {
            label,
            total,
            targets,
            landing,
            items: 0,
            bytes: 0,
            current: PathBuf::new(),
        }
    }
}

/// Which trailing metadata column the entry list draws (ADR 0005, D2). The
/// current pane toggles between `Size` and `Modified` with the `t` key; the
/// parent context pane is always `None` (no column, reclaiming the width). One
/// enum replaces the old `show_size: bool` so the size / relative-mtime / absent
/// choice is a single, exhaustive decision rather than two overlapping booleans.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MetaCol {
    /// Right-aligned human byte size (the default; directories show blank).
    Size,
    /// Right-aligned compact relative modified age (`3h`, `2d`, …); directories
    /// show their mtime too.
    Modified,
    /// No trailing column at all — the parent pane, which reclaims the width.
    None,
}

impl MetaCol {
    /// The `t` toggle for the current pane: flip between `Size` and `Modified`.
    /// `None` is a parent-only state and never user-toggled, but maps to
    /// `Modified` for totality.
    fn toggle(self) -> Self {
        match self {
            MetaCol::Modified => MetaCol::Size,
            MetaCol::Size | MetaCol::None => MetaCol::Modified,
        }
    }
}

/// The key the entry listing is ordered by (the browser's analogue of yazi's
/// sort modes). Directories are ALWAYS grouped first regardless of key — that
/// invariant predates this feature and is preserved by [`sort_cmp`]; the key
/// only decides the order *within* each group. `Name` is the default and, with
/// `reverse: false`, reproduces the old fixed ordering byte-for-byte.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    /// Case-insensitive by file name (the default).
    Name,
    /// By byte size, ascending; directories (size 0) sort among themselves by name.
    Size,
    /// By modified time, oldest first (reverse for newest first). Missing mtimes
    /// sort as oldest.
    Modified,
    /// By file extension (lower-cased), then name — groups like files together.
    Ext,
}

impl SortKey {
    /// The `o` cycle: Name → Size → Modified → Ext → Name.
    fn cycle(self) -> Self {
        match self {
            SortKey::Name => SortKey::Size,
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Ext,
            SortKey::Ext => SortKey::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Ext => "ext",
        }
    }
}

/// The full sort spec: which [`SortKey`] and whether it's reversed. `Copy` so the
/// comparator and the panes can read it freely. Default (`Name`, not reversed) is
/// the pre-feature ordering.
#[derive(Clone, Copy)]
struct Sort {
    key: SortKey,
    reverse: bool,
}

impl Sort {
    fn default() -> Self {
        Sort {
            key: SortKey::Name,
            reverse: false,
        }
    }

    /// A short status blurb, e.g. `sort: size ↓` (↑ ascending, ↓ reversed).
    fn label(self) -> String {
        let arrow = if self.reverse { "↓" } else { "↑" };
        format!("sort: {} {arrow}", self.key.label())
    }
}

/// The raw extension slice of a file name (the part after the last dot), or `""`
/// when it has none. NOT lower-cased — the caller compares it through
/// [`cmp_name_ci`], so folding here would just allocate. Pure — unit-tested. Used
/// only by [`SortKey::Ext`]; kept a free fn so the comparator and its tests share
/// one definition.
fn name_ext(name: &str) -> &str {
    match name.rsplit_once('.') {
        // A leading-dot name (`.gitignore`) is all "stem", no extension.
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => "",
    }
}

/// Case-insensitive comparison of two strings WITHOUT allocating. Folds each side
/// to lowercase lazily, char by char, through `char::to_lowercase()` (which can
/// expand one char to several — the flattened iterators handle full Unicode case
/// folding), and compares the two streams lexicographically. When one side runs
/// out of chars first it sorts first, exactly like `str::cmp`. Pure — unit-tested.
/// This is the allocation-free replacement for `a.to_lowercase().cmp(&b.to_lowercase())`,
/// which [`sort_cmp`] calls ~2·N·log N times per sort.
fn cmp_name_ci(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().flat_map(char::to_lowercase);
    let mut bi = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ai.next(), bi.next()) {
            (Some(x), Some(y)) => match x.cmp(&y) {
                Ordering::Equal => continue,
                ord => return ord,
            },
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => return Ordering::Equal,
        }
    }
}

/// The fields [`sort_cmp`] orders by, abstracted over the two things the browser
/// sorts: a directory [`Entry`] and a recursive-search [`crate::search::Hit`]. One
/// comparator then serves both surfaces, so browse listings and search results
/// order identically under the same [`Sort`] (the search results inherit whatever
/// sort the browser is set to). The only per-type wrinkle is the sort name: an
/// entry sorts by its bare name, a hit by its relative path so the flat result
/// list still groups by folder.
trait Sortable {
    /// The string the `Name`/`Ext` keys order and tie-break by.
    fn sort_name(&self) -> &str;
    fn sort_is_dir(&self) -> bool;
    fn sort_size(&self) -> u64;
    fn sort_modified(&self) -> Option<SystemTime>;
}

impl Sortable for Entry {
    fn sort_name(&self) -> &str {
        &self.name
    }
    fn sort_is_dir(&self) -> bool {
        self.kind == Format::Directory
    }
    fn sort_size(&self) -> u64 {
        self.size
    }
    fn sort_modified(&self) -> Option<SystemTime> {
        self.modified
    }
}

impl Sortable for crate::search::Hit {
    /// Sort by the relative path (not the bare file name) so the flat result list
    /// reads in a folder-grouped order — `sub/a.rs` sorts beside its siblings, not
    /// scattered among every other `a.*` in the tree.
    fn sort_name(&self) -> &str {
        &self.rel
    }
    fn sort_is_dir(&self) -> bool {
        self.kind == Format::Directory
    }
    fn sort_size(&self) -> u64 {
        self.size
    }
    fn sort_modified(&self) -> Option<SystemTime> {
        self.modified
    }
}

/// Total order over anything [`Sortable`] for a given [`Sort`]. Pure — unit-tested
/// without any filesystem. Directories always sort before files (the pre-feature
/// invariant); the `Sort` only orders within each group, and every key breaks ties
/// by case-insensitive [`Sortable::sort_name`] so the order is deterministic.
/// `reverse` flips the within-group order (directories stay first — reversing name
/// gives Z→A, not files-before-dirs), matching how file managers reverse.
fn sort_cmp<T: Sortable>(a: &T, b: &T, sort: Sort) -> Ordering {
    let dirs_first = b.sort_is_dir().cmp(&a.sort_is_dir());
    if dirs_first != Ordering::Equal {
        return dirs_first; // group boundary — never affected by key or reverse
    }
    let by_name = || cmp_name_ci(a.sort_name(), b.sort_name());
    let ord = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Size => a.sort_size().cmp(&b.sort_size()).then_with(by_name),
        SortKey::Modified => a.sort_modified().cmp(&b.sort_modified()).then_with(by_name),
        SortKey::Ext => {
            cmp_name_ci(name_ext(a.sort_name()), name_ext(b.sort_name())).then_with(by_name)
        }
    };
    if sort.reverse {
        ord.reverse()
    } else {
        ord
    }
}

/// What the preview pane is currently showing.
enum Pv {
    Text,    // styled lines in `preview`
    Loading, // async raster in flight; caption in `caption`
    Image,   // pixels in `pane`, caption in `caption`
}

/// What the async raster worker ships back over the channel (ADR 0005 D1). A
/// still is a single decoded image (cached in `img_cache`); an animated GIF is a
/// frame set the pane loops (never cached — frame sets are large; reselecting
/// re-decodes off-thread). Widening this from a bare `Option<DynamicImage>` is
/// what lets one worker feed both the still and the animated install paths while
/// still never touching `pane`/`img_cache` itself.
enum Rastered {
    Still(DynamicImage),
    Animated(Vec<media::Frame>),
}

/// The direction a folder-navigation slide travels (ADR 0006 D3). Names the
/// motion by where the NEW listing enters from: entering a child pushes the old
/// listing left and brings the new one in from the right; going to the parent
/// reverses it. One `Copy` enum so `render` and the offset maths can read it
/// without borrowing the owned snapshot buffer beside it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SlideDir {
    /// Entered a child: the new listing enters from the right (old exits left).
    FromRight,
    /// Went to the parent: the new listing enters from the left (old exits right).
    FromLeft,
}

/// One in-flight folder slide (ADR 0006 D3): the time-based `Anim`, the direction,
/// an owned snapshot of the OLD pane's inner content, and a frame counter for the
/// `SUCHER_ANIM_STATS` proof. Snapshotted at navigation time (before the new
/// listing loads) and driven/cleared in `main_loop` exactly like the colour
/// `fade`, which runs in lockstep so the incoming listing slides AND resolves its
/// colours together.
///
/// *Cell-granularity ceiling (ADR 0006 D3):* a terminal can only translate content
/// in whole character cells, so the slide has at most `inner_width` (~40) distinct
/// positions; beyond ~250 fps extra frames repeat a position. The slide's
/// smoothness is bounded by column width, not refresh rate — the continuous part
/// is the colour fade layered on top.
struct Slide {
    anim: crate::anim::Anim,
    dir: SlideDir,
    old: Buffer,
    frames: u32,
}

/// The live state of the recursive-search mode (ADR 0007). Present on `App.search`
/// (as `Some`) only while `Mode::Search` is active — `None` in browse/filter, so
/// the search paths are strictly additive and cost nothing off-mode. Distinct from
/// the browse filter in every field (own text, own selection, own walk): search and
/// the local `/` filter are two operations, not a hybrid (D1).
struct SearchState {
    /// The raw query text being typed (shown as `⌕ …`). Parsed fresh on every edit
    /// by [`App::restart_search`] into a `query::Query`. Its OWN buffer, never the
    /// browse `filter`, so the local filter path (D1) is byte-for-byte untouched.
    query: String,
    /// The running background tree walk, or `None` when the query is empty — a
    /// blank query must not walk the whole tree (D3 / [`query::Query::is_empty`]).
    /// Dropping it cancels the walk, so replacing it (a query edit) or clearing it
    /// (leaving search) stops the superseded walk promptly (D3).
    engine: Option<crate::search::Search>,
    /// Hits received so far, kept sorted by the active [`Sort`] — the walk streams
    /// them live and [`App::pump_search`] re-sorts on each drain (the walker itself
    /// surfaces them in nondeterministic arrival order).
    results: Vec<crate::search::Hit>,
    /// Selection + scroll state for the results list: the search analogue of the
    /// browse `App.state`, so [`App::cur_sel`] can source the hit under the cursor
    /// and render it through the shared preview pipeline (D5).
    state: ListState,
    /// Whether the walk has sent its terminal `Msg::Done` (streaming finished).
    /// Drives the `searching…` vs `N results` status text and the fast-poll gate.
    done: bool,
    /// Whether the walk stopped at the result cap. Surfaced in the status line —
    /// never a silent truncation (D3).
    capped: bool,
}

impl SearchState {
    /// A fresh search: empty query, no walk yet (started on the first keystroke).
    fn new() -> Self {
        SearchState {
            query: String::new(),
            engine: None,
            results: Vec::new(),
            state: ListState::default(),
            done: false,
            capped: false,
        }
    }

    /// The hit under the cursor, if any.
    fn selected(&self) -> Option<&crate::search::Hit> {
        self.state.selected().and_then(|i| self.results.get(i))
    }

    /// Move the results selection by `delta`, clamped to the result set (the search
    /// analogue of [`App::move_sel`]); a no-op when there are no results.
    fn move_sel(&mut self, delta: isize) {
        let next = search_sel(self.state.selected(), delta, self.results.len());
        self.state.select(next);
    }
}

struct App {
    cwd: PathBuf,
    // The resolved icon mode (ADR 0003, D5). The browser keys its glyph column
    // and per-entry tint off it in `render_entry_list`.
    icons: IconMode,
    // The effective pane layout (ADR 0004, D1). Starts at the config value; the
    // `M` key cycles it at runtime. `render` reduces it to 2 or 3 columns via
    // `effective_columns`, collapsing Miller to double when the frame is too
    // narrow or there is no parent.
    layout: Layout,
    // Whether the git gutter is enabled at all (config `git`, ADR 0004 D2). When
    // false, `git` below stays `None` everywhere and no `git` subprocess runs.
    git_enabled: bool,
    // The current directory's git status map (name → state), recomputed on every
    // `load`. `None` when git is disabled, git is absent, or `cwd` isn't a repo —
    // in which case the gutter is not drawn and the layout is the pre-git render.
    git: Option<std::collections::HashMap<String, GitStatus>>,
    // The repo HEAD (branch / detached oid / ahead-behind) shown right-aligned on
    // the breadcrumb row. Refreshed with `git` on every `load`; `None` outside a
    // repo (or git disabled/absent), which keeps the crumb row pre-git identical.
    head: Option<git::RepoHead>,
    all: Vec<Entry>,
    view: Vec<usize>, // indices into `all` matching the filter
    // The parent directory's entries, already ordered by `sort` — the cache
    // behind the Miller parent pane (perf). Recomputed once per directory change
    // in `load` (a single `read_dir` + sort) and re-sorted in place by `resort`,
    // so `render_parent` reads this slice instead of re-listing the parent on
    // every render frame — which, on a remote S3/GCS mount, was a network LIST
    // per keystroke and per animation frame. Empty when `cwd` has no parent.
    parent: Vec<Entry>,
    state: ListState,
    filter: String,
    mode: Mode,
    show_hidden: bool,
    viewport_h: u16,
    status: Option<String>,
    // Type-to-select session (ADR 0002): the name buffer and the `Instant` of
    // its last keystroke. The session is active only while that keystroke is
    // within `typeahead::TIMEOUT`; after it lapses the vim motions win again.
    typeahead: String,
    typeahead_at: Option<Instant>,
    preview: Vec<Line<'static>>,
    preview_for: Option<PathBuf>,
    pv: Pv,
    caption: String,
    pane: Option<ImagePane>,
    img_cache: Vec<(PathBuf, DynamicImage)>,
    // Async rasteriser (image/PDF/video posters). The worker never touches
    // `img_cache` or `pane`; it decodes on a thread and ships the finished
    // `Rastered` (Send) — a still image or an animated GIF's frames — back over
    // the channel to the main thread, which installs it.
    raster_tx: Sender<(PathBuf, Option<Rastered>)>,
    raster_rx: Receiver<(PathBuf, Option<Rastered>)>,
    raster_pending: Option<PathBuf>, // path in the ONE live worker
    raster_want: Option<(PathBuf, Format)>, // latest selection awaiting a raster
    // Whether the current `Pv::Image` preview is an animated GIF that must be
    // ticked (ADR 0005 D1). Set only when an `Animated` raster installs; cleared
    // by `build_preview` on any new selection and by installing a still. `main_loop`
    // gates its per-frame tick on this AND `pv == Image`, so a still (or nothing)
    // being previewed never ticks — no idle churn off the animated path.
    preview_animated: bool,
    // Which trailing metadata column the current pane draws (ADR 0005 D2).
    // Starts at `Size` (byte-for-byte the pre-feature look); the `t` key cycles
    // it with `Modified`. The parent pane always renders `MetaCol::None`.
    meta: MetaCol,
    // Clickable breadcrumb hit-targets, rebuilt every `render_crumb` (ADR 0005
    // D2): each entry is a column span in the breadcrumb row and the absolute
    // directory a click there navigates to. Recorded unconditionally (harmless
    // when mouse capture is off); consumed by `crumb_hit` on a left-click.
    crumb_hits: Vec<(Range<u16>, PathBuf)>,
    // The CURRENT entry-list pane's on-screen rectangle, recorded every `render`
    // (ADR 0005 D2). This is the click-hit-test surface for the file list —
    // analogous to `crumb_hits` for the breadcrumb: written unconditionally
    // (harmless with mouse off) and read only on a left-click, where
    // `row_to_index` maps a clicked row inside it to a `view` index. It is
    // `cols[0]` in the two-column split and `cols[1]` (the middle pane) in Miller.
    list_area: Rect,
    // The Miller PARENT pane's rectangle, or `None` outside Miller (ADR 0005 D2).
    // A left-click inside it navigates up (`go_parent`). `Some(cols[0])` only in
    // the three-column branch; `None` in the two-column split, so a parent click
    // is simply impossible there.
    parent_area: Option<Rect>,
    // Braille-spinner frame counter for the `Loading` preview (ADR 0004 D3). It
    // advances ONLY while a raster is live (pending or wanted) — never on an idle
    // redraw — so the spinner animates during real work without the fully-idle
    // browser ever churning the CPU. See the tick in `main_loop`.
    spin: usize,
    // Whether navigation animations run (config `animate`, ADR 0006 D4). Snapshot
    // of `anim::enabled()` at construction. When false, `fade` is never armed and
    // the browser is byte-for-byte the pre-animation build.
    animate: bool,
    // The in-flight current-pane fade-in after a directory change (ADR 0006 D3),
    // or `None` when no fade is live. A time-based `Anim`, so its duration is
    // identical whatever FPS the loop sustains. Armed at the end of `enter_dir`
    // (only when `animate`); driven and cleared in `main_loop`.
    fade: Option<crate::anim::Anim>,
    // Frames drawn during the current fade, for the `SUCHER_ANIM_STATS` proof.
    // Reset when a fade is armed; reported to `anim::record` when it completes.
    fade_frames: u32,
    // The in-flight current-pane directional slide after a directory change (ADR
    // 0006 D3), or `None` when none is live. Holds an owned snapshot of the OLD
    // listing's inner content, taken at navigation time before the new listing
    // loads. Armed alongside `fade` (only when `animate` AND a real pane rect
    // exists), and driven/cleared beside it in `main_loop`; a keypress clears it
    // so the next render is the settled state. Only the current pane slides — in
    // Miller the parent/preview panes stay static (D3).
    slide: Option<Slide>,
    // The recursive-search mode's live state (ADR 0007), or `None` in browse/filter.
    // `Some` exactly while `Mode::Search` is active; every search path is guarded on
    // it, so browse/filter/typeahead are strictly unaffected (D1).
    search: Option<SearchState>,
    // The results-list pane rect, recorded every `render_search` (ADR 0007 §9). The
    // click-hit-test surface for search rows — the search analogue of `list_area` —
    // read on a left-click by `row_to_index` against the search results. Inert
    // outside search mode (no search mouse events are routed there).
    search_area: Rect,
    // The active sort for the entry listing (feature: yazi-style sort modes). The
    // current and parent panes both order through `sort_cmp` with this; `o`
    // cycles the key and `O` toggles reverse. Starts at the default (name,
    // ascending) — byte-for-byte the pre-feature ordering.
    sort: Sort,
    // Whether the which-key help overlay is up. Toggled by `?` in browse mode and
    // dismissed by the next keypress (which-key convention). Only ever true in
    // browse mode — every mode change dismisses it first (see `handle_key`).
    help: bool,
    // The multi-select set a later file operation will act on (ADR 0017 D2/D3).
    // Keyed by absolute path and deliberately NOT cleared on a directory change:
    // gathering three files here and two in a sibling folder, then acting once, is
    // the entire reason multi-select beats operating on the cursor. Empty is the
    // ordinary state, and while it is empty the browser is byte-for-byte the
    // pre-feature build: the mark gutter reserves no width and the status line
    // keeps its key hint, both appearing only once something is actually marked.
    marks: crate::marks::Marks,
    // What `y` / `X` loaded for `p`, or `None` when nothing is on the clipboard
    // (ADR 0017 D4). Deliberately NOT cleared on a directory change, exactly as
    // `marks` is not and for the same reason (D3): yanking in one folder and
    // pasting in another is the whole workflow the key pair exists to serve.
    clip: Option<Clip>,
    // The ONE file operation in flight, or `None` (ADR 0017 D2). Exactly one at a
    // time, mirroring how `raster_pending` allows a single live raster worker: a
    // second request while this is `Some` is refused with an honest "busy" status
    // and never queued, because queued mutation cannot be reasoned about while the
    // user is still navigating. Dropping the `Run` cancels its worker, so an
    // operation can never outlive the browser that asked for it.
    op: Option<fileop::Run>,
    // The browser-side state of that same run: the progress the status line draws
    // and the marks the run will consume. `Some` exactly while `op` is `Some`.
    op_progress: Option<InFlight>,
    // The undo stack (ADR 0017 D8): the journals of completed operations, oldest
    // first, bounded at `UNDO_DEPTH` by `push_journal`. `U` pops the newest and
    // hands it to the executor; an undo's own journal is empty by construction and
    // `finish_op` skips pushing an empty one, so undos never stack on undos.
    journal: Vec<fileop::Journal>,
}

enum Action {
    Quit,
    Open(PathBuf),
}

/// A browse action bound to a single character. The one place the browser's
/// char bindings are named, so typeahead's "is this char bound?" test and the
/// real key handler share a single source of truth (ADR 0002 D2).
enum CharAction {
    Down,
    Up,
    HalfDown,
    HalfUp,
    Top,
    Bottom,
    Open,
    /// Hand the selected entry to the OS default application ("open in native
    /// app"). Bound to `x`. Works on any entry — including ones sucher has no
    /// in-app viewer for (legacy .doc, audio) and directories (opens the file
    /// manager).
    OpenExternal,
    Parent,
    Filter,
    /// Enter recursive-search mode (ADR 0007). Bound to `S`.
    Search,
    ToggleHidden,
    ToggleLayout,
    ToggleMeta,
    /// Cycle the sort key (name → size → modified → ext). Bound to `o`.
    CycleSort,
    /// Toggle sort direction. Bound to `O` (shift-o).
    ReverseSort,
    /// Toggle the which-key help overlay. Bound to `?`.
    Help,
    /// Toggle the mark on the entry under the cursor, then step the selection one
    /// row down so holding the key marks a run (ADR 0017 D4). Bound to `Space`.
    ToggleMark,
    /// Toggle every entry in the CURRENT filtered view, in listing order (ADR 0017
    /// D4). Bound to `V`. Marks held in other directories are outside the listing,
    /// so a global set survives an invert here untouched (D3).
    InvertMarks,
    /// Send the selection to the OS trash, behind the confirm overlay (ADR 0017
    /// D4/D7). Bound to `D`. There is no permanent-delete binding at all, here or
    /// anywhere: trash is the only way something leaves a path in sucher.
    Trash,
    /// Load the selection onto the clipboard to be copied (ADR 0017 D4). Bound to
    /// `y`.
    Yank,
    /// Load the selection onto the clipboard to be moved (ADR 0017 D4). Bound to
    /// `X`, leaving the lowercase `x` "open in the native app" motion alone.
    Cut,
    /// Paste the clipboard into the CURRENT directory, behind the confirm overlay
    /// (ADR 0017 D4/D5). Bound to `p`.
    Paste,
    /// Rename the one selected entry through the inline prompt (ADR 0017 D4).
    /// Bound to `r`. No confirm overlay follows: the typed name is the
    /// authorisation (D5).
    Rename,
    /// Create an entry in the current directory through the inline prompt, as a
    /// directory when the typed name ends in `/` (ADR 0017 D4). Bound to `a`.
    Create,
    /// Undo the most recent completed operation (ADR 0017 D4/D8). Bound to `U`.
    /// It pops the newest journal and replays its inverses through the SAME
    /// executor every forward operation uses, so it is an ordinary run and the
    /// one-in-flight rule, the progress line and the `Esc` ladder all cover it.
    Undo,
    /// Hand the absolute path(s) of the selection to the terminal for the system
    /// clipboard, over OSC 52 (ADR 0017 D4). Bound to `Y`, leaving the lowercase
    /// `y` clipboard-for-paste key alone. It mutates nothing, so unlike every
    /// other key in this group it is neither planned, confirmed, journalled, nor
    /// refused while an operation is running.
    YankPath,
    Quit,
}

/// Map a character to its browse action, or `None` if it's unbound. Both the
/// key handler (via [`App::run_char_action`]) and typeahead's `key_is_bound`
/// read from here; add a binding once and both stay correct.
fn browse_char(c: char) -> Option<CharAction> {
    Some(match c {
        'j' => CharAction::Down,
        'k' => CharAction::Up,
        'd' => CharAction::HalfDown,
        'u' => CharAction::HalfUp,
        'g' => CharAction::Top,
        'G' => CharAction::Bottom,
        'l' => CharAction::Open,
        // Open the selection in the OS default app. Bound here (not left to
        // typeahead) so `x` is a motion, never the start of a name search.
        'x' => CharAction::OpenExternal,
        'h' => CharAction::Parent,
        '/' => CharAction::Filter,
        // Enter recursive search (ADR 0007). Capital `S`; binding it here also
        // keeps typeahead correct — a bound char never starts a name search.
        'S' => CharAction::Search,
        '.' => CharAction::ToggleHidden,
        // Cycle the pane layout (auto→miller→double→auto). Binding `M` here also
        // keeps typeahead correct: a bound char never starts a name search.
        'M' => CharAction::ToggleLayout,
        // Cycle the trailing metadata column (size ↔ modified). Binding `t` here
        // also keeps typeahead correct: a bound char never starts a name search.
        't' => CharAction::ToggleMeta,
        // Sort controls: `o` cycles the key, `O` toggles direction. Bound here so
        // typeahead treats them as motions, never as the start of a name search.
        'o' => CharAction::CycleSort,
        'O' => CharAction::ReverseSort,
        // Multi-select (ADR 0017 D4). Registering `Space` and `V` here is what
        // keeps typeahead correct for them (ADR 0002 D2): a bound char is never
        // the start of a name search, so a space can never open a name buffer and
        // `V` stays a mark key rather than a jump to `Videos/`.
        ' ' => CharAction::ToggleMark,
        'V' => CharAction::InvertMarks,
        // Delete to trash (ADR 0017 D4). Capital `D`, so the lowercase `d`
        // half-page motion is untouched; registering it here is what keeps
        // typeahead correct (ADR 0002 D2), since a bound char never starts a name
        // search and `D` would otherwise jump to `Downloads/`.
        'D' => CharAction::Trash,
        // The clipboard (ADR 0017 D4). `y` and `p` are the two yazi/vim keys the
        // ADR knowingly spends out of the typeahead alphabet, and `X` is capital
        // so the lowercase `x` native-open motion survives. Registering all three
        // here is what keeps typeahead correct (ADR 0002 D2): a bound char never
        // starts a name search, so `p` cannot become a jump to `Pictures/`.
        'y' => CharAction::Yank,
        'X' => CharAction::Cut,
        'p' => CharAction::Paste,
        // The two typed-name operations (ADR 0017 D4), the other pair of
        // lowercase letters the ADR knowingly spends out of the typeahead
        // alphabet. Registering them here is what keeps typeahead correct (ADR
        // 0002 D2): a bound char never starts a name search, so `r` cannot
        // become a jump to `README.md`. `Ctrl-a` still marks everything in the
        // view, and reaches its own arm in `handle_key` before this map is
        // consulted, so the bare `a` and the modified one stay distinct.
        'r' => CharAction::Rename,
        'a' => CharAction::Create,
        // Undo, and the yank of a path to the system clipboard (ADR 0017 D4).
        // Both are capitals so the lowercase `u` half-page motion and the `y`
        // clipboard key survive untouched, and registering them here is what keeps
        // typeahead correct (ADR 0002 D2): a bound char never starts a name search,
        // so `U` cannot become a jump to `Users/` nor `Y` one to `Yesterday.md`.
        'U' => CharAction::Undo,
        'Y' => CharAction::YankPath,
        // Toggle the help overlay. Bound (not left to typeahead) so `?` never
        // starts a name search; the overlay is dismissed by the next key.
        '?' => CharAction::Help,
        'q' => CharAction::Quit,
        _ => return None,
    })
}

/// Enables crossterm mouse capture on construction (when `on`) and guarantees
/// its teardown on drop (ADR 0005 D2). Wrapping the mode in an RAII guard makes
/// the "the shell must never be left in capture mode" invariant structural: the
/// guard is created right after `ratatui::init()` and explicitly dropped right
/// before `ratatui::restore()`, so capture is off on every exit — quit, the
/// open-and-return round trip, an error return, or a panic (drop still runs
/// while unwinding). A disabled guard (`on == false`) is inert both ways.
struct MouseGuard(bool);

impl MouseGuard {
    fn enable(on: bool) -> Self {
        if on {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture);
        }
        MouseGuard(on)
    }
}

impl Drop for MouseGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        }
    }
}

pub fn run(
    start: String,
    icons: IconMode,
    layout: Layout,
    git_enabled: bool,
    mouse: bool,
) -> io::Result<()> {
    let cwd = fs::canonicalize(&start).unwrap_or_else(|_| PathBuf::from(&start));
    // Probe the graphics protocol once, before any alternate screen. If the
    // terminal can't do pixels, previews fall back to text/metadata.
    let pane = ImagePane::new().ok();
    let (raster_tx, raster_rx) = std::sync::mpsc::channel();
    let mut app = App {
        cwd,
        icons,
        layout,
        git_enabled,
        git: None,
        head: None,
        all: Vec::new(),
        view: Vec::new(),
        parent: Vec::new(),
        state: ListState::default(),
        filter: String::new(),
        mode: Mode::Browse,
        show_hidden: false,
        viewport_h: 0,
        status: None,
        typeahead: String::new(),
        typeahead_at: None,
        preview: Vec::new(),
        preview_for: None,
        pv: Pv::Text,
        caption: String::new(),
        pane,
        img_cache: Vec::new(),
        raster_tx,
        raster_rx,
        raster_pending: None,
        raster_want: None,
        preview_animated: false,
        meta: MetaCol::Size,
        crumb_hits: Vec::new(),
        list_area: Rect::default(),
        parent_area: None,
        spin: 0,
        // Read the animate toggle once from the process global (installed in
        // `main` beside the palette), consistent with how the browser reads the
        // theme — no new parameter threaded through `run`.
        animate: crate::anim::enabled(),
        fade: None,
        fade_frames: 0,
        slide: None,
        search: None,
        search_area: Rect::default(),
        sort: Sort::default(),
        help: false,
        // No marks at startup, which is also the state in which the browser
        // renders byte-for-byte as it did before ADR 0017 (no gutter, no status
        // line). The set is filled only from the browse listing, by `Space`, `V`
        // and `Ctrl-a`, and survives every later directory change (D3).
        marks: crate::marks::Marks::new(),
        // Nothing on the clipboard until `y` or `X` fills it.
        clip: None,
        // No operation at startup, and none is created until a plan is authorised
        // in the confirm overlay, so a browser that is only ever browsed pays
        // nothing for this feature: `pump_fileop` returns immediately and the
        // fast-poll tier is never entered.
        op: None,
        op_progress: None,
        journal: Vec::new(),
    };
    app.load();

    loop {
        let mut term = ratatui::init();
        // Enable mouse capture right after entering the alternate screen and tear
        // it down right before leaving it, on every path. The explicit `drop`
        // below disables capture before `restore` (and before any opened viewer,
        // which runs its own screen); the guard's Drop is the backstop that also
        // covers a panic during `main_loop`.
        let guard = MouseGuard::enable(mouse);
        let action = app.main_loop(&mut term);
        drop(guard);
        ratatui::restore();
        match action {
            Ok(Action::Quit) => return Ok(()),
            Ok(Action::Open(path)) => {
                crate::open_interactive(&path.to_string_lossy());
                app.preview_for = None; // force a redraw-time recompute
            }
            Err(e) => return Err(e),
        }
    }
}

/// An owned snapshot of the entry currently under the cursor, from whichever
/// surface is active: the browsed listing, or (in search mode) the selected hit.
/// [`App::build_preview`] and the preview-change check source from here so a
/// search hit renders through the exact same preview pipeline as a browsed file
/// (ADR 0007 D5) — the whole point of the feature. Owned (not a borrow) so the
/// caller is free to mutate `self` while building the preview.
struct Sel {
    name: String,
    path: PathBuf,
    kind: Format,
    size: u64,
    modified: Option<SystemTime>,
}

impl App {
    /// The entry under the cursor as an owned [`Sel`], from the active surface: in
    /// search mode the selected hit (its file name derived from the hit path, or
    /// its `rel` when the path has no final component), otherwise the browsed entry
    /// (ADR 0007 D5). `None` when nothing is selected (empty listing, or a search
    /// with no results yet). Browse/filter behave exactly as `selected()` — this is
    /// purely additive; `selected()` itself is unchanged.
    fn cur_sel(&self) -> Option<Sel> {
        if let Some(search) = self.search.as_ref() {
            let hit = search.selected()?;
            let name = hit
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| hit.rel.clone());
            Some(Sel {
                name,
                path: hit.path.clone(),
                kind: hit.kind,
                size: hit.size,
                modified: hit.modified,
            })
        } else {
            let e = self.selected()?;
            Some(Sel {
                name: e.name.clone(),
                path: e.path.clone(),
                kind: e.kind,
                size: e.size,
                modified: e.modified,
            })
        }
    }

    /// Read the current directory into `all`, then apply the filter.
    fn load(&mut self) {
        self.all = read_entries(&self.cwd, self.sort);
        // Cache the parent listing once for the whole time we're in this directory
        // (perf): `render_parent` reads this slice every frame instead of re-listing
        // the parent, so the Miller parent pane costs one `read_dir` per navigation
        // rather than one per render — critical on remote mounts. Empty when there
        // is no parent (the parent pane is a no-op then anyway).
        self.parent = match self.cwd.parent() {
            Some(p) => read_entries(p, self.sort),
            None => Vec::new(),
        };
        // Refresh the git gutter for the new directory (cheap, correct after a
        // dir change — D2). Disabled, git-absent, or non-repo dirs yield `None`,
        // which the pane renderer treats as "no gutter" (byte-for-byte pre-git).
        self.git = if self.git_enabled {
            git::status_map(&self.cwd)
        } else {
            None
        };
        // HEAD identity for the breadcrumb row. Only fetched when `status_map`
        // found a repo (`git` is `Some`), so non-repo dirs pay zero extra
        // subprocess cost; `--untracked-files=no` keeps the call ms-cheap.
        self.head = if self.git.is_some() {
            git::head_info(&self.cwd)
        } else {
            None
        };
        self.refilter();
    }

    /// Rebuild `view` from `all` honoring hidden + the smart-query filter.
    fn refilter(&mut self) {
        let q = query::parse(&self.filter);
        self.view = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_hidden || !e.name.starts_with('.'))
            .filter(|(_, e)| q.matches(&e.name, e.kind, e.size, e.modified))
            .map(|(i, _)| i)
            .collect();
        let sel = if self.view.is_empty() {
            None
        } else {
            Some(self.state.selected().unwrap_or(0).min(self.view.len() - 1))
        };
        self.state.select(sel);
        self.preview_for = None;
    }

    /// Re-order the loaded entries in place after a sort change, then rebuild the
    /// filtered view. Uses the SAME comparator as `read_entries`, so an in-place
    /// re-sort and a fresh directory read can never disagree. Cheaper than a full
    /// `load` (no `read_dir`, no git subprocess) since the entries themselves are
    /// unchanged — only their order is. Selection is re-clamped by `refilter`.
    fn resort(&mut self) {
        let sort = self.sort;
        self.all.sort_by(|a, b| sort_cmp(a, b, sort));
        // Re-order the cached parent listing with the SAME comparator so the parent
        // pane reflects the new sort without a re-read (the cwd didn't change, so
        // the parent's entries are unchanged — only their order is).
        self.parent.sort_by(|a, b| sort_cmp(a, b, sort));
        self.refilter();
        self.status = Some(self.sort.label());
    }

    fn selected(&self) -> Option<&Entry> {
        let i = self.state.selected()?;
        self.all.get(*self.view.get(i)?)
    }

    /// The current filtered view as the `(path, size, is_dir)` rows
    /// [`crate::marks::Marks`] consumes, in listing order (ADR 0017 D3). Owned
    /// rather than an iterator over `self`, because every caller feeds them
    /// straight into `&mut self.marks`, and materialising the listing first keeps
    /// that mutation independent of the `all`/`view` borrows instead of relying on
    /// disjoint-field capture rules. It costs one small allocation per keystroke,
    /// never per frame.
    fn view_rows(&self) -> Vec<(PathBuf, u64, bool)> {
        self.view
            .iter()
            .map(|&i| {
                let e = &self.all[i];
                (e.path.clone(), e.size, e.kind == Format::Directory)
            })
            .collect()
    }

    fn move_sel(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let n = self.view.len() as isize;
        let cur = self.state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, n - 1);
        self.state.select(Some(next as usize));
    }

    fn enter_dir(&mut self, path: PathBuf, dir: SlideDir) {
        // Snapshot the OUTGOING listing's inner content BEFORE any state mutates,
        // so the slide's "old" layer is exactly what was on screen (ADR 0006 D3).
        // Only when animating and a real pane rect already exists (a prior render
        // set `list_area`); the first navigation, or `animate = false`, yields
        // `None` and no slide is armed.
        let old = if self.animate {
            self.snapshot_current_inner()
        } else {
            None
        };
        self.cwd = path;
        self.filter.clear();
        self.mode = Mode::Browse;
        self.state.select(Some(0));
        self.status = None;
        self.typeahead.clear();
        self.typeahead_at = None;
        self.load();
        // Arm the current-pane fade-in AND the directional slide AFTER the new
        // listing loads, so the fresh entries are what resolve from the background
        // and slide into place (ADR 0006 D3). Both are time-based `Anim`s started
        // at the same instant with the same duration, so they run in lockstep: the
        // incoming listing slides in while its colours fade up. Only when
        // animations are enabled — otherwise the transition stays instant and no
        // anim state is ever created. The slide is additionally gated on a valid
        // old snapshot (skipped on the first navigation, before any render).
        if self.animate {
            let now = Instant::now();
            self.fade = Some(crate::anim::Anim::new(now, NAV_ANIM));
            self.fade_frames = 0;
            self.slide = old.map(|old| Slide {
                anim: crate::anim::Anim::new(now, NAV_ANIM),
                dir,
                old,
                frames: 0,
            });
        }
    }

    fn go_parent(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let from = self
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            // Going up: the new (parent) listing enters from the left.
            self.enter_dir(parent, SlideDir::FromLeft);
            // Land on the directory we came out of.
            if let Some(name) = from {
                if let Some(pos) = self.view.iter().position(|&i| self.all[i].name == name) {
                    self.state.select(Some(pos));
                }
            }
        }
    }

    /// Snapshot the CURRENT pane's inner content into an owned [`Buffer`] for the
    /// folder slide's "old" layer (ADR 0006 D3). Built from the live
    /// `all`/`view`/`state` at FULL colour (`fade_t: None`) through the exact same
    /// item path as the normal render, so the outgoing layer looks identical to
    /// what was on screen. Returns `None` when there's no real pane rect yet — the
    /// first navigation happens before any render sets `list_area`, and snapshotting
    /// a zero-sized region would be garbage — in which case the caller skips the
    /// slide and the transition is instant (the fade still resolves the new colours).
    fn snapshot_current_inner(&self) -> Option<Buffer> {
        let area = self.list_area;
        let inner = entry_inner(area);
        if inner.width == 0 || inner.height == 0 {
            return None;
        }
        // The outgoing listing: same fields the current pane renders with, minus
        // the title (only the inner items are snapshotted; the border/title are
        // static and drawn fresh each frame) and with no fade (full colour).
        let view = EntryListView {
            entries: &self.all,
            order: &self.view,
            selected: self.state.selected(),
            title: String::new(),
            git: self.git.as_ref(),
            meta: self.meta,
            fade_t: None,
            // The outgoing layer of a folder slide must look exactly like what was
            // on screen, so it carries the same mark gutter the live pane had.
            marks: mark_gutter(&self.marks),
        };
        // Snapshot only the visible window (perf: list virtualisation), sized from
        // the live scroll offset so the "old" layer matches what was on screen.
        let (_, window) = visible_window(
            self.state.offset(),
            view.selected,
            view.order.len(),
            inner.height as usize,
        );
        let items = entry_items(area, &view, self.icons, window.clone());
        let list = entry_list(items, None);
        let mut buf = Buffer::empty(inner);
        // A local state renders the window at offset 0 without disturbing the real
        // one, with the selection rebased into the window.
        let mut state = ListState::default();
        state.select(window_selection(view.selected, &window));
        render_items_into(&mut buf, inner, list, &mut state);
        Some(buf)
    }

    fn activate(&mut self) -> Option<Action> {
        let e = self.selected()?;
        if e.kind == Format::Directory {
            let p = e.path.clone();
            // Entering a child: the new listing enters from the right.
            self.enter_dir(p, SlideDir::FromRight);
            None
        } else if e.kind.opens() {
            Some(Action::Open(e.path.clone()))
        } else {
            // Recognized but unopenable (office docs, audio, archives, binary):
            // stay in the browser and say so rather than mis-opening.
            self.status = Some(format!("no viewer for {}", e.kind.label()));
            None
        }
    }

    /// Leave search mode and return to the browse listing (ADR 0007 §3). Dropping
    /// the `SearchState` cancels any in-flight walk (`Search`'s `Drop` → `cancel`).
    /// `preview_for = None` forces the browse selection's preview to rebuild, since
    /// `cur_sel` now sources the browsed entry again.
    fn exit_search(&mut self) {
        self.search = None;
        self.mode = Mode::Browse;
        self.status = None;
        self.preview_for = None;
    }

    /// Restart the background walk after a query edit (ADR 0007 §4). Parses the raw
    /// text: a blank query drops the engine and shows the empty prompt state (D3 —
    /// never walk the whole tree for nothing); otherwise a fresh walk is started
    /// from `cwd`. Assigning the new engine (or `None`) drops the OLD one first,
    /// which cancels the superseded walk (D3) before the next begins. Either way the
    /// accumulated results/selection/flags are cleared to the fresh-query state.
    fn restart_search(&mut self) {
        // Read the disjoint fields the walk needs before taking `&mut self.search`.
        let cwd = self.cwd.clone();
        let show_hidden = self.show_hidden;
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let q = query::parse(&search.query);
        search.results.clear();
        search.state.select(None);
        search.done = false;
        search.capped = false;
        search.engine = if q.is_empty() {
            None
        } else {
            Some(crate::search::start(cwd, q, show_hidden))
        };
    }

    /// Drain the search channel into `results`, mirroring [`App::pump_raster`] (ADR
    /// 0007 §5). Appends every streamed `Hit`; on `Done` records completion + the
    /// cap flag and drops the engine (the walk is over). Keeps the growing list
    /// **sorted** by the active [`Sort`] (via [`sort_cmp`]) so results present in a
    /// deterministic, folder-grouped order rather than nondeterministic walk-arrival
    /// order — the parallel walker surfaces hits in whatever order its worker threads
    /// finish, which two runs need not agree on. Returns whether anything changed
    /// (→ redraw).
    fn pump_search(&mut self) -> bool {
        // Read the app-wide sort before borrowing `self.search` (search results
        // inherit whatever sort the browser is set to — one sort preference).
        let sort = self.sort;
        let Some(search) = self.search.as_mut() else {
            return false;
        };
        let Some(engine) = search.engine.as_ref() else {
            return false;
        };
        let msgs = engine.drain();
        if msgs.is_empty() {
            return false;
        }
        // Remember which hit is under the cursor (by path) BEFORE appending/sorting,
        // so re-ordering the list as new hits stream in doesn't shift the selection
        // off the row the user is looking at.
        let anchor = search.selected().map(|h| h.path.clone());
        for msg in msgs {
            match msg {
                crate::search::Msg::Hit(h) => search.results.push(h),
                crate::search::Msg::Done { capped } => {
                    search.done = true;
                    search.capped = capped;
                    search.engine = None; // the walk finished; nothing left to drain
                }
            }
        }
        // Re-sort the whole (possibly grown) list each drain. Trivially cheap even
        // at the 5000-hit cap, and it lets a late-arriving hit slot into its correct
        // position rather than tacking onto the end. Cheap for two reasons: after
        // Fix 1 `sort_cmp` allocates nothing per comparison, and Rust's stable sort
        // is adaptive — the vec is already sorted from the previous drain with only
        // a short appended run, which it merges in near-linear time. So a full
        // re-sort each drain is correct and performant; no hand-rolled merge needed.
        search.results.sort_by(|a, b| sort_cmp(a, b, sort));
        // Re-anchor the cursor to the same hit after the re-order; if nothing was
        // selected yet (the first hits just arrived), land on the first row so the
        // preview pipeline (D5) has something to render immediately.
        let restored = anchor.and_then(|p| search.results.iter().position(|h| h.path == p));
        search
            .state
            .select(restored.or((!search.results.is_empty()).then_some(0)));
        true
    }

    /// Drain the file-operation channel, mirroring [`App::pump_search`] (ADR 0017
    /// D2). Folds every `Msg::Progress` into the status-line counters and finishes
    /// the run on `Msg::Done`. Returns whether anything changed (→ redraw), and is
    /// an immediate `false` when no operation is in flight, so a browser that is
    /// only being browsed pays one `Option` test per loop iteration.
    fn pump_fileop(&mut self) -> bool {
        // The receiver is borrowed only for the drain itself, so the fold below is
        // free to mutate `self` (the same shape `pump_raster` uses).
        let msgs = match self.op.as_ref() {
            Some(run) => run.drain(),
            None => return false,
        };
        if msgs.is_empty() {
            return false;
        }
        // The executor sends exactly one `Done`, always last, always preceded by a
        // final unthrottled `Progress`. Holding it until the whole batch is folded
        // keeps that ordering true even when both arrive in one drain.
        let mut done = None;
        for msg in msgs {
            match msg {
                fileop::Msg::Progress {
                    items,
                    bytes,
                    current,
                } => {
                    if let Some(flight) = self.op_progress.as_mut() {
                        // Cumulative, not incremental: the worker throttles its
                        // sends, so each message is the whole truth so far.
                        flight.items = items;
                        flight.bytes = bytes;
                        flight.current = current;
                    }
                }
                fileop::Msg::Done(report) => done = Some(report),
            }
        }
        if let Some(report) = done {
            self.finish_op(report);
        }
        true
    }

    /// Retire a finished operation: reload, unmark, journal, report.
    ///
    /// The order matters. The marks go before the reload so the refreshed listing
    /// is drawn with the gutter already correct, and the outcome is decided last,
    /// once there is nothing left that could still fail.
    fn finish_op(&mut self, report: fileop::Report) {
        self.op = None;
        let flight = self.op_progress.take();

        // Drop the marks this run consumed. A selection has served its purpose the
        // moment it is acted on, and marks left pointing at paths that are now in
        // the trash would be a lie about what is selected: the gutter would show
        // rows that no longer exist and the next operation would collect them into
        // its `missing` list for no reason. Only the run's own targets go, so marks
        // gathered in other directories survive, which is the whole point of a
        // global set (ADR 0017 D3).
        if let Some(flight) = &flight {
            for path in &flight.targets {
                self.marks.remove(path);
            }
        }

        // A cut that has landed leaves a clipboard pointing at paths that are no
        // longer where it says they are, so it goes; a copy's sources are all
        // still there and pasting them into three folders in turn is a real
        // workflow, so it stays. See [`clip_survives`] for the whole rule.
        if !clip_survives(report.kind) {
            self.clip = None;
        }

        // Reload so the listing reflects what happened; `load` also refreshes the
        // git gutter and the parent cache, both of which a mutation can invalidate.
        //
        // What the cursor should land on is what the operation set out to leave
        // behind, when it named exactly one thing: a rename's new name, a
        // create's new entry. Asking for the name that was under the cursor
        // would be asking for a name a rename has just abolished, and `reselect`
        // would fall back to the row, leaving the cursor beside the file the user
        // just renamed rather than on it. Everything else keeps the old rule,
        // because a multi-step paste has no single answer to give (see
        // [`landing_name`]).
        let wanted = flight
            .as_ref()
            .and_then(|flight| flight.landing.clone())
            .or_else(|| self.selected().map(|e| e.name.clone()));
        let prev = self.state.selected();
        self.load();
        let names: Vec<&str> = self
            .view
            .iter()
            .map(|&i| self.all[i].name.as_str())
            .collect();
        let next = reselect(&names, wanted.as_deref(), prev);
        self.state.select(next);

        // ADR 0017 D8: the journal records what actually happened, so an operation
        // that failed every step has nothing to undo. Pushing that empty journal
        // would spend a slot on the bounded stack and evict a real one, so it is
        // skipped rather than stored.
        if !report.journal.steps.is_empty() {
            push_journal(&mut self.journal, report.journal.clone(), UNDO_DEPTH);
        }

        // The run's own totals go to the status line either way: a clean run has
        // nothing more to say, and a partly failed one leaves the summary behind
        // once its overlay is dismissed, so the outcome does not vanish with the
        // popup that reported it.
        self.status = Some(op_done_status(
            report.kind,
            report.direction,
            report.items,
            report.bytes,
        ));
        if !report_speaks(&report) {
            return;
        }
        // A partial result is reported, never swallowed (ADR 0009), so failures get
        // an overlay rather than a status line. Search is left first when it is up:
        // a background walk costs one key to restart, while a failure that never
        // reached the user is gone for good, so the walk is the cheaper thing to
        // lose. A live filter is simply left in place under the popup.
        if matches!(self.mode, Mode::Search) {
            self.exit_search();
        }
        self.show_op(OpView::Report(report));
    }

    /// Put an operation overlay on screen. The which-key help is dismissed first,
    /// because two popups drawn over each other would leave the user answering the
    /// one they cannot see.
    ///
    /// The status line is deliberately left alone: while an overlay is up the
    /// status line renders from the mode itself, and what was there before is
    /// what the user should see again once the overlay closes.
    fn show_op(&mut self, view: OpView) {
        self.help = false;
        self.mode = Mode::Op(view);
    }

    /// The `D` binding: resolve a trash operation and show it for authorisation
    /// (ADR 0017 D4/D7). Nothing is mutated here; this only decides.
    fn request_trash(&mut self) {
        if self.op.is_some() {
            // ADR 0017 D2: exactly one operation in flight, refused rather than
            // queued, and said out loud rather than ignored.
            self.status = Some("busy: an operation is already running".to_string());
            return;
        }
        let paths = targets(&self.marks, self.selected().map(|e| e.path.as_path()));
        if paths.is_empty() {
            self.status = Some(fileop::Refusal::NothingSelected.to_string());
            return;
        }
        // The one filesystem question the operation asks before it runs: what is
        // actually there, and what has already vanished (ADR 0017 D2).
        let collected = match fileop::collect(&paths) {
            Ok(collected) => collected,
            Err(refusal) => {
                // A refusal is a complete, honest answer rather than an error to
                // hide, so it goes to the status line verbatim and nothing changes.
                self.status = Some(refusal.to_string());
                return;
            }
        };
        if collected.sources.is_empty() {
            // Everything selected had already vanished. The planner would refuse
            // this as "nothing selected", which is true but hides the half worth
            // knowing, so it is named here instead. The stale marks go with it:
            // leaving them would keep a gutter pointing at paths that are gone
            // (ADR 0017 D3).
            let gone = mark_count(collected.missing.len());
            for path in &collected.missing {
                self.marks.remove(path);
            }
            self.status = Some(format!("{gone} already gone, so there is nothing to trash"));
            return;
        }
        // Owned so the `PlanCtx` borrow below does not pin `self` across the match.
        let cwd = self.cwd.clone();
        let resolved = fileop::plan(
            fileop::Op::Trash {
                sources: collected.sources,
            },
            &fileop::PlanCtx {
                // Trash has no destination directory, so there are no names to
                // collide with and an empty listing is the whole truth.
                //
                // EVERY operation that DOES have a destination (copy, move,
                // rename, create) must pass the UNFILTERED listing of that
                // directory, hidden entries included, read fresh from the
                // filesystem at plan time. Neither `self.view` (which hides
                // dotfiles unless `.` is toggled) nor `self.all` (a snapshot from
                // the last `load`) will do: planning against either would let an
                // operation land on top of a `.env` the user cannot see, which is
                // exactly the hole ADR 0017 D5 closes.
                dest_listing: &[],
                cwd: &cwd,
                // Marks that had already vanished ride into the plan so the
                // overlay can show them; never silently dropped (D3).
                missing: &collected.missing,
                policy: fileop::Conflict::Rename,
            },
        );
        match resolved {
            Ok(plan) => self.show_op(OpView::Confirm(Pending {
                plan,
                // Trash has no destination, so there is nothing for the overwrite
                // toggle to re-plan and `o` stays inert on this overlay.
                inputs: None,
            })),
            Err(refusal) => self.status = Some(refusal.to_string()),
        }
    }

    /// The `y` and `X` bindings: load the selection onto the clipboard (ADR 0017
    /// D4). Nothing is read, moved or copied here; this only remembers.
    ///
    /// The mark set is deliberately LEFT ALONE, and the opposite choice is the
    /// tempting one. Marks are the visible record of what a paste is going to act
    /// on, so clearing them here would leave the user holding a clipboard with no
    /// gutter to show for it. They clean themselves up at the right moment
    /// instead: [`App::finish_op`] drops exactly the marks the operation consumed,
    /// once it has actually happened.
    fn load_clip(&mut self, cut: bool) {
        let paths = targets(&self.marks, self.selected().map(|e| e.path.as_path()));
        if paths.is_empty() {
            self.status = Some(fileop::Refusal::NothingSelected.to_string());
            return;
        }
        self.status = Some(clip_status(cut, paths.len()));
        self.clip = Some(Clip { cut, paths });
    }

    /// The `p` binding: resolve a paste of the clipboard into the CURRENT
    /// directory and show it for authorisation (ADR 0017 D4/D5). Nothing is
    /// mutated here; this only decides.
    fn request_paste(&mut self) {
        if self.op.is_some() {
            // ADR 0017 D2: exactly one operation in flight, refused rather than
            // queued, and said out loud rather than ignored.
            self.status = Some("busy: an operation is already running".to_string());
            return;
        }
        let Some(clip) = self.clip.as_ref() else {
            self.status = Some("nothing on the clipboard: [y] copies, [X] cuts".to_string());
            return;
        };
        // Owned so the borrow of `self.clip` ends here and the rest of the method
        // is free to mutate `self`.
        let cut = clip.cut;
        let paths = clip.paths.clone();
        // The one filesystem question the operation asks before it runs: what is
        // actually there, and what has already vanished (ADR 0017 D2).
        let collected = match fileop::collect(&paths) {
            Ok(collected) => collected,
            Err(refusal) => {
                // A refusal is a complete, honest answer rather than an error to
                // hide, so it goes to the status line verbatim and nothing changes.
                self.status = Some(refusal.to_string());
                return;
            }
        };
        if collected.sources.is_empty() {
            // Every path on the clipboard had already vanished. The planner would
            // refuse this as "nothing selected", which is true but hides the half
            // worth knowing, so it is named here instead. The clipboard goes with
            // it: one that points only at paths which no longer exist would offer
            // the user a paste that can never do anything.
            self.clip = None;
            let gone = mark_count(collected.missing.len());
            self.status = Some(format!("{gone} already gone, so there is nothing to paste"));
            return;
        }
        let cwd = self.cwd.clone();
        let inputs = Replan {
            transfer: Transfer::of(cut),
            sources: collected.sources,
            // A paste always lands in the directory the user is standing in, which
            // is what makes "yank there, walk here, paste" the whole gesture.
            dest: cwd.clone(),
            dest_listing: dest_listing(&cwd),
            // Clipboard entries that had already vanished ride into the plan so the
            // overlay can show them; never silently dropped (D3).
            missing: collected.missing,
            cwd,
        };
        // Suffixing is always the policy a plan is first shown under: overwriting
        // is reachable only through the explicit `o` toggle on the overlay (D5).
        match inputs.plan(fileop::Conflict::Rename) {
            Ok(plan) => self.show_op(OpView::Confirm(Pending {
                plan,
                inputs: Some(Box::new(inputs)),
            })),
            Err(refusal) => self.status = Some(refusal.to_string()),
        }
    }

    /// The `r` binding: open the inline rename prompt on the one selected entry
    /// (ADR 0017 D4). Nothing is mutated here; this only asks.
    ///
    /// The field opens pre-filled with the current name and the cursor parked at
    /// the end of the stem, so the first character typed replaces the name while
    /// keeping the extension (see [`stem_end`]). That is what every file manager
    /// does and it is most of what makes the key worth pressing: a rename that
    /// opened empty would make the user retype `.tar.gz` every time, and one that
    /// selected the whole name would make keeping the extension the hard case.
    fn request_rename(&mut self) {
        if self.op.is_some() {
            // ADR 0017 D2: exactly one operation in flight, refused rather than
            // queued, and said out loud rather than ignored.
            self.status = Some("busy: an operation is already running".to_string());
            return;
        }
        // Rename needs the KIND as well as the path, because the stem rule turns
        // on it, and both are already known without touching the disk: a mark
        // records what it was marked as, and the listing knows what it is
        // showing. This is [`targets`]' rule with its one documented exception
        // (ADR 0017 D3): several marks are named rather than guessed between,
        // since a bulk rename is a different feature and picking one of five
        // would be picking for the user.
        let one = if self.marks.is_empty() {
            self.selected()
                .map(|e| (e.path.clone(), e.kind == Format::Directory))
        } else if self.marks.len() == 1 {
            self.marks
                .marks()
                .first()
                .map(|m| (m.path.clone(), m.is_dir))
        } else {
            self.status = Some(format!(
                "{}: rename acts on one entry, so clear them with [Esc] first",
                mark_count(self.marks.len())
            ));
            return;
        };
        let Some((path, is_dir)) = one else {
            self.status = Some(fileop::Refusal::NothingSelected.to_string());
            return;
        };
        // A path with no final component is the filesystem root, which has no
        // name to edit; the planner would refuse it anyway, and refusing before
        // opening a field is the honest order.
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            self.status = Some(fileop::Refusal::FilesystemRoot.to_string());
            return;
        };
        let cursor = stem_end(&name, is_dir);
        self.status = None; // drop any stale "type: …" hint
        self.mode = Mode::Input(Prompt {
            ask: Ask::Rename { path },
            edit: crate::lineedit::LineEdit::with_text(&name, cursor),
        });
    }

    /// The `a` binding: open the inline create prompt for the current directory
    /// (ADR 0017 D4). Nothing is mutated here; this only asks.
    ///
    /// It opens EMPTY, and deliberately so: there is no existing name to start
    /// from, and any placeholder would have to be deleted before it could be
    /// useful. The trailing-slash rule is carried by the hint instead, which is
    /// the only place it can be learned.
    fn request_create(&mut self) {
        if self.op.is_some() {
            // ADR 0017 D2, exactly as above: refused rather than queued.
            self.status = Some("busy: an operation is already running".to_string());
            return;
        }
        self.status = None; // drop any stale "type: …" hint
        self.mode = Mode::Input(Prompt {
            ask: Ask::Create {
                parent: self.cwd.clone(),
            },
            edit: crate::lineedit::LineEdit::new(),
        });
    }

    /// Keys while the inline prompt is up (ADR 0017 D4).
    ///
    /// A text-input surface, so typeahead never applies and every printable key
    /// spells the name, exactly as in filter and search mode. `Esc` belongs to
    /// this layer and is answered here before the [`escape`] ladder underneath
    /// ever sees it, which is the same rule the filter and the operation overlay
    /// already follow: a modal layer backs itself out first.
    fn handle_input_key(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            // The typed name is the authorisation, so `Enter` runs it outright
            // (ADR 0017 D5).
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.status = Some("cancelled".to_string());
            }
            _ => {
                let Mode::Input(prompt) = &mut self.mode else {
                    return None;
                };
                match code {
                    KeyCode::Char(c) => prompt.edit.insert(c),
                    KeyCode::Backspace => prompt.edit.backspace(),
                    KeyCode::Delete => prompt.edit.delete(),
                    KeyCode::Left => prompt.edit.left(),
                    KeyCode::Right => prompt.edit.right(),
                    KeyCode::Home => prompt.edit.home(),
                    KeyCode::End => prompt.edit.end(),
                    _ => {}
                }
            }
        }
        None
    }

    /// `Enter` on the inline prompt: resolve the typed name and, if it resolves,
    /// run it (ADR 0017 D5).
    ///
    /// There is no confirm overlay, and that follows from what the user has
    /// already said. Paste and trash act on a set assembled earlier, possibly
    /// across several folders and possibly minutes ago, so the overlay is where
    /// they find out what that set has become. A rename or a create is typed in
    /// the moment and the typed name IS the authorisation, so a second "are you
    /// sure" would be ceremony rather than information.
    ///
    /// A refusal therefore surfaces in the status line, and the prompt STAYS OPEN
    /// with its text and cursor untouched. A refusal here is feedback on the name
    /// that was just typed, which is precisely when leaving the field is the
    /// wrong response: answering "that name is taken" by taking the name away
    /// would make the user retype the whole thing to change one character of it.
    fn submit_prompt(&mut self) {
        // Lift the question and the answer out first, so the planning below is
        // free to borrow the rest of `self`.
        let Mode::Input(prompt) = &self.mode else {
            return;
        };
        let ask = prompt.ask.clone();
        let name = prompt.edit.text().to_string();
        let cwd = self.cwd.clone();
        let resolved = match &ask {
            Ask::Rename { path } => rename_plan(path, &name, &cwd),
            Ask::Create { parent } => create_plan(parent, &name, &cwd),
        };
        match resolved {
            Ok(plan) => {
                // The prompt has served its purpose the moment the name resolves;
                // the browser returns to the listing at once so the reload the run
                // finishes with lands on an ordinary browse frame.
                self.mode = Mode::Browse;
                self.start_op(plan);
            }
            Err(refusal) => self.status = Some(refusal.to_string()),
        }
    }

    /// The `o` binding on the confirm overlay: re-plan the same batch under the
    /// other conflict policy and show what that would do (ADR 0017 D5).
    ///
    /// A toggle rather than a one-way switch, so a user who pressed it to see what
    /// overwriting would cost gets the safe suffixing default back with the same
    /// key. The recomputation is pure: the sources were enumerated once when the
    /// overlay opened and are not walked again here.
    fn toggle_overwrite(&mut self) {
        let outcome = match &self.mode {
            Mode::Op(OpView::Confirm(pending)) => pending
                .inputs
                .as_ref()
                // Trash has nothing to overwrite, so `o` is inert there rather than
                // doing something invisible.
                .map(|inputs| inputs.plan(flip_policy(pending.plan.policy))),
            _ => None,
        };
        match outcome {
            Some(Ok(plan)) => {
                if let Mode::Op(OpView::Confirm(pending)) = &mut self.mode {
                    // Only the plan changes: the inputs stay so the toggle can be
                    // pressed again, and again, without re-reading anything.
                    pending.plan = plan;
                }
            }
            // A batch that plans one way can still be refused the other way (a
            // suffix search that runs out of names), so the refusal is reported and
            // the overlay keeps showing the plan the user already has.
            Some(Err(refusal)) => self.status = Some(refusal.to_string()),
            None => {}
        }
    }

    /// Authorise a plan: hand it to the executor and remember what it consumes.
    fn start_op(&mut self, plan: fileop::Plan) {
        // Read everything the status line and the completion path need while the
        // plan is still here; `fileop::start` takes it by value.
        let label = plan.summary();
        let total = plan.items();
        let mut targets: Vec<PathBuf> = plan
            .steps
            .iter()
            .map(|s| s.src.clone())
            // A create has no source, and an empty path is nobody's mark.
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        targets.extend(plan.missing.iter().cloned());
        let landing = landing_name(&plan.steps);
        let flight = InFlight::new(label, total, targets, landing);
        self.begin_run(fileop::start(plan), flight);
    }

    /// The `U` binding: replay the newest journal's inverses (ADR 0017 D4/D8).
    ///
    /// There is no plan, no confirm overlay and no collect stage, because the
    /// journal already names every step that actually happened: undo has nothing
    /// left to decide, which is why it enters the engine at the executor rather
    /// than at the top. What it is not exempt from is everything that makes a run
    /// a run, so it goes into the same `op` slot and is drained by the same
    /// `pump_fileop`, retired by the same `finish_op`, refused while another
    /// operation is live, and cancelled by the same `Esc`.
    fn undo_last(&mut self) {
        if self.op.is_some() {
            // ADR 0017 D2: exactly one operation in flight, refused rather than
            // queued, and said out loud rather than ignored. An undo is an ordinary
            // run and gets no exemption from that rule.
            self.status = Some("busy: an operation is already running".to_string());
            return;
        }
        let Some(journal) = self.journal.pop() else {
            self.status = Some("nothing to undo".to_string());
            return;
        };
        // An undo reverses journal steps, not filesystem entries, so its
        // denominator is the step count: that is the same number `Report::items`
        // comes back with, and a progress line whose two halves counted different
        // things would be worse than no progress line.
        let total = journal.steps.len();
        let label = undo_label(journal.kind);
        let landing = undo_landing(&journal.steps);
        // Nothing to unmark. `finish_op` drops the marks a run consumed, and the
        // forward operation this undoes already dropped its own; an undo consumes
        // no selection of its own, so it hands over an empty list rather than
        // reaching for marks that are not its to clear (ADR 0017 D3).
        let flight = InFlight::new(label, total, Vec::new(), landing);
        self.begin_run(fileop::start_undo(journal), flight);
    }

    /// Take up the one operation slot (ADR 0017 D2), whichever direction the run
    /// travels. The two fields are set together and only here, so `op_progress` is
    /// `Some` exactly while `op` is, which is what every reader of the pair assumes.
    fn begin_run(&mut self, run: fileop::Run, flight: InFlight) {
        self.op_progress = Some(flight);
        self.op = Some(run);
    }

    /// The `Y` binding: hand the selection's ABSOLUTE paths to the terminal for
    /// the system clipboard, one per line (ADR 0017 D4).
    ///
    /// Not an operation, and deliberately not shaped like one: it reads no tree,
    /// writes no path and journals nothing, so there is no plan to confirm, nothing
    /// for `U` to take back, and no reason to refuse it while a copy is running.
    /// It takes the same targets as every verb, though, because "the marks if any,
    /// otherwise the row under the cursor" is a rule about the selection rather
    /// than about mutation.
    fn yank_paths(&mut self) {
        let paths = targets(&self.marks, self.selected().map(|e| e.path.as_path()));
        if paths.is_empty() {
            self.status = Some(fileop::Refusal::NothingSelected.to_string());
            return;
        }
        let text = clipboard_text(&self.cwd, &paths);
        // The refusal (a selection past the terminal's payload ceiling) is already
        // a finished sentence naming the size and the cap, so it goes to the status
        // line verbatim rather than being wrapped in a vaguer one.
        self.status = Some(match crate::util::copy_to_clipboard(&text) {
            Ok(()) => yank_status(paths.len()),
            Err(refusal) => refusal,
        });
    }

    /// Keys while a file-operation overlay is up (ADR 0017 D5). The overlay is
    /// modal: it consumes every key, so nothing leaks through to the listing
    /// underneath while the user is deciding.
    fn handle_op_key(&mut self, code: KeyCode) -> Option<Action> {
        if !matches!(self.mode, Mode::Op(OpView::Confirm(_))) {
            // A report of failures is read, not answered, so any key closes it,
            // following the same which-key convention as the help overlay.
            self.mode = Mode::Browse;
            return None;
        }
        match code {
            KeyCode::Enter | KeyCode::Char('y') => {
                // Taking the mode by value moves the plan into the executor
                // without cloning it; the browser returns to the listing at once
                // so the user can keep navigating while the run streams.
                if let Mode::Op(OpView::Confirm(pending)) =
                    std::mem::replace(&mut self.mode, Mode::Browse)
                {
                    self.start_op(pending.plan);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = Mode::Browse;
                self.status = Some("cancelled".to_string());
            }
            // Toggle overwriting and show what that would do, in the danger
            // colour (ADR 0017 D5). Inert on the trash overlay, which has no
            // destination and so no collision to resolve; the overlay does not
            // advertise the key there either, so nothing invisible happens.
            KeyCode::Char('o') => self.toggle_overwrite(),
            _ => {}
        }
        None
    }

    /// Move the results-list selection by `delta` (ADR 0007 §7); a no-op outside
    /// search mode or with no results.
    fn search_move(&mut self, delta: isize) {
        if let Some(search) = self.search.as_mut() {
            search.move_sel(delta);
        }
    }

    /// Activate the selected hit (ADR 0007 §8). A directory hit leaves search and
    /// navigates into it; an openable file returns `Action::Open` — `App` state
    /// (search included) survives the open-and-return round trip (`run`'s outer
    /// loop), so quitting the viewer lands back in the live results. An unopenable
    /// kind just reports it.
    fn activate_search(&mut self) -> Option<Action> {
        let sel = self.cur_sel()?;
        if sel.kind == Format::Directory {
            self.exit_search();
            self.enter_dir(sel.path, SlideDir::FromRight);
            // Opening a dir HIT is a jump to an arbitrary (possibly deep) descendant,
            // not a sibling step — and the browse pane wasn't even on screen (search
            // was). `enter_dir` armed a slide from the stale pre-search snapshot; drop
            // it so only the (background-anchored) colour fade plays. A slide implying
            // spatial adjacency would be a lie here.
            self.slide = None;
            None
        } else if sel.kind.opens() {
            Some(Action::Open(sel.path))
        } else {
            self.status = Some(format!("no viewer for {}", sel.kind.label()));
            None
        }
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<Action> {
        let mut dirty = true;
        loop {
            // Drain the recursive-search stream (ADR 0007 §5): append newly-arrived
            // hits and notice completion. Run BEFORE the preview recompute so the
            // first hit — which both arrives and sets the initial selection here —
            // has its preview built in this same iteration, not one loop (≤60 ms)
            // later. Inert (an early `false`) when not searching.
            if self.pump_search() {
                dirty = true;
            }
            // Drain the file-operation stream (ADR 0017 D2), beside the search
            // pump and for the same reason: progress must advance and completion
            // must be noticed without waiting on a keypress. Run BEFORE the preview
            // recompute so the reload a finished operation performs settles the
            // selection first, and this iteration previews the settled row rather
            // than one that is about to move. Inert (an early `false`) when no
            // operation is running.
            if self.pump_fileop() {
                dirty = true;
            }
            // Recompute the preview when the selection changed. Sourced from the
            // mode-aware accessor so a search hit drives the preview too (ADR 0007
            // D5); in browse/filter `cur_sel` is the browsed entry, unchanged.
            let cur = self.cur_sel().map(|s| s.path);
            if cur != self.preview_for {
                self.build_preview();
                self.preview_for = cur;
                dirty = true;
            }
            // Service the async rasteriser: install finished posters, retire the
            // in-flight job, and launch the next wanted one.
            if self.pump_raster() {
                dirty = true;
            }
            // Drive the folder fade (ADR 0006 D3). While a fade is live, redraw
            // every loop so the eased colours advance, counting frames for the
            // stats proof. On completion, record the achieved FPS and clear it —
            // the very next render (with `fade == None`) is the final, identity
            // frame, so a fade always settles on the exact non-animated colours.
            // Independent of the raster/GIF arms: a fade and a GIF preview coexist.
            if let Some(fade) = self.fade {
                let now = Instant::now();
                dirty = true;
                if fade.done(now) {
                    crate::anim::record("folder-fade", self.fade_frames, fade.elapsed(now));
                    self.fade = None;
                } else {
                    self.fade_frames = self.fade_frames.saturating_add(1);
                }
            }
            // Drive the folder slide in lockstep with the fade (ADR 0006 D3). Same
            // shape: redraw every loop while live, count frames for the stats
            // proof, and on completion record the achieved FPS and clear it — the
            // next render (with `slide == None`) is the settled frame, which the
            // offset maths make identical to the normal render. `done` is read
            // before the `&mut` borrow so `self.slide` can be cleared cleanly.
            let slide_done = self
                .slide
                .as_ref()
                .map(|s| s.anim.done(Instant::now()))
                .unwrap_or(false);
            if let Some(slide) = self.slide.as_mut() {
                dirty = true;
                if slide_done {
                    crate::anim::record(
                        "folder-slide",
                        slide.frames,
                        slide.anim.elapsed(Instant::now()),
                    );
                } else {
                    slide.frames = slide.frames.saturating_add(1);
                }
            }
            if slide_done {
                self.slide = None;
            }
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }
            // Poll briefly while a raster is in flight or queued (so a finished
            // image installs promptly AND the braille spinner ticks) OR while an
            // animated GIF preview is on screen (so it loops); otherwise idle at
            // the normal 1 s cadence. These are the ONLY conditions under which a
            // bare timeout does any work — a fully idle browser (still image,
            // text, or nothing selected) blocks the full second and does nothing.
            let raster_active = self.raster_pending.is_some() || self.raster_want.is_some();
            let animating = self.preview_animated && matches!(self.pv, Pv::Image);
            // A search whose walk is still streaming polls fast so hits appear live
            // (ADR 0007 §6); the same 60 ms tier as the raster. Once the walk sends
            // `Done` the engine is dropped and this falls back to the idle cadence.
            let searching = self.search.as_ref().is_some_and(|s| s.engine.is_some());
            // An operation streaming progress polls on the same 60 ms tier as a
            // raster or a live search, so the counters and the current path move
            // while the work happens (ADR 0017 D2). It clears the moment `Done`
            // arrives and `pump_fileop` drops the run, so a browser with no
            // operation running still blocks the full second and does nothing.
            let operating = self.op.is_some();
            // A live fade emits as fast as the per-frame budget allows (~4 ms ⇒
            // ≤250 fps) so the interpolation is smooth up to the display refresh
            // (ADR 0006 D2); the heavier raster/GIF paths keep their 60 ms cadence.
            // The blocks above already cleared `fade`/`slide` if they just
            // completed, so a fully idle browser (no fade, no slide, no raster, no
            // GIF) still blocks the full second and does nothing — no new idle
            // churn. A live slide emits at the same ~4 ms cadence as the fade
            // (they run together), so the two share the fast-poll arm.
            let fading = self.fade.is_some() || self.slide.is_some();
            let timeout = if fading {
                Duration::from_millis(4)
            } else if raster_active || animating || searching || operating {
                Duration::from_millis(60)
            } else {
                Duration::from_millis(1000)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        // Interrupt any in-flight fade AND slide: complete them at
                        // once so the next render is the final state (ADR 0006 D2 —
                        // motion never adds latency). A key that changes directory
                        // re-arms a fresh fade+slide inside `handle_key`/`enter_dir`.
                        // Dropping the slide here also frees its owned snapshot buffer.
                        self.fade = None;
                        self.slide = None;
                        if let Some(action) = self.handle_key(key) {
                            return Ok(action);
                        }
                    }
                    // Pointer navigation while mouse capture is on (ADR 0005 D2).
                    // A left-click on the breadcrumb row jumps to the clicked
                    // segment's directory; a click in the current list selects a
                    // row (a second click on the already-selected row opens it); a
                    // click in the Miller parent pane navigates up; the wheel
                    // scrolls the selection. Any mouse event counts as activity →
                    // redraw. When capture is off no mouse events arrive, so this
                    // arm is simply dead.
                    // Search-mode pointer handling (ADR 0007 §9): the wheel moves the
                    // results selection; a left-click selects a row, a second click
                    // on the already-selected row activates it (mirroring the browse
                    // rule). Guarded to search mode so the browse arm below is
                    // strictly unchanged.
                    Event::Mouse(me) if matches!(self.mode, Mode::Search) => match me.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let (offset, len, cur) = match self.search.as_ref() {
                                Some(s) => (s.state.offset(), s.results.len(), s.state.selected()),
                                None => (0, 0, None),
                            };
                            if let Some(idx) =
                                row_to_index(self.search_area, offset, me.row, me.column, len)
                            {
                                if cur == Some(idx) {
                                    if let Some(action) = self.activate_search() {
                                        return Ok(action);
                                    }
                                } else if let Some(s) = self.search.as_mut() {
                                    s.state.select(Some(idx));
                                }
                                dirty = true;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            self.search_move(1);
                            dirty = true;
                        }
                        MouseEventKind::ScrollUp => {
                            self.search_move(-1);
                            dirty = true;
                        }
                        _ => {}
                    },
                    // A click or wheel while a file-operation overlay is up is
                    // swallowed whole (ADR 0017 D5). Deliberately NOT the help
                    // overlay's dismiss-on-click rule: a confirm popup is a
                    // question awaiting an answer, and a stray click that either
                    // cancelled it or scrolled the listing hidden behind it would
                    // be a worse surprise than a click that does nothing.
                    Event::Mouse(_) if matches!(self.mode, Mode::Op(_)) => {}
                    // A click or wheel while the help overlay is up dismisses it
                    // (and is otherwise swallowed), mirroring the keyboard rule.
                    Event::Mouse(_) if self.help => {
                        self.help = false;
                        dirty = true;
                    }
                    Event::Mouse(me) => match me.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if me.row == 0 {
                                // Breadcrumb row (handled first, exactly as before).
                                if let Some(target) = crumb_hit(&self.crumb_hits, me.column) {
                                    if target != self.cwd {
                                        // A breadcrumb always jumps to an ancestor
                                        // (up), so the new listing enters from the
                                        // left, matching `go_parent` (ADR 0006 D3).
                                        self.enter_dir(target, SlideDir::FromLeft);
                                        dirty = true;
                                    }
                                }
                            } else if let Some(idx) = row_to_index(
                                self.list_area,
                                self.state.offset(),
                                me.row,
                                me.column,
                                self.view.len(),
                            ) {
                                // Single-click SELECTS a different row; a click on
                                // the ALREADY-selected row OPENS it. One click moves
                                // the cursor and a second on it activates — this is
                                // discoverable and avoids the accidental opens a
                                // click-to-open-anything rule would cause (and it
                                // mirrors the keyboard: land, then Enter). Opening a
                                // file yields an `Action` that must leave `main_loop`
                                // exactly like the `Enter`/`l` path, so propagate it.
                                if self.state.selected() == Some(idx) {
                                    if let Some(action) = self.activate() {
                                        return Ok(action);
                                    }
                                } else {
                                    self.state.select(Some(idx));
                                }
                                dirty = true;
                            } else if self
                                .parent_area
                                .is_some_and(|a| rect_contains(a, me.column, me.row))
                            {
                                // A click anywhere in the Miller parent pane simply
                                // navigates up (the simple, robust rule from D2).
                                self.go_parent();
                                dirty = true;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            self.move_sel(1);
                            dirty = true;
                        }
                        MouseEventKind::ScrollUp => {
                            self.move_sel(-1);
                            dirty = true;
                        }
                        _ => {}
                    },
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            } else {
                // Timeout with no input. Advance whatever live animation applies;
                // both arms are gated so a fully idle browser does neither and
                // stays at zero CPU (ADR 0004 D3 spinner, ADR 0005 D1 GIF).
                if raster_active {
                    // Spinner: advance one braille frame and redraw at ~60 ms.
                    self.spin = self.spin.wrapping_add(1);
                    dirty = true;
                }
                if animating {
                    // GIF: advance to the next frame if its delay elapsed. `tick`
                    // re-encodes and returns true only on a real frame change, so
                    // we redraw exactly when the picture moved.
                    if let Some(pane) = self.pane.as_mut() {
                        if pane.tick(Instant::now()) {
                            dirty = true;
                        }
                    }
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        let code = key.code;
        // A file-operation overlay is modal and claims the keyboard before any
        // other surface (ADR 0017 D5). It is checked first because it can appear
        // asynchronously, when a run finishes with failures, and it must own the
        // next key even if the browser was in filter mode when that happened.
        if matches!(self.mode, Mode::Op(_)) {
            return self.handle_op_key(code);
        }
        // The inline prompt is the other layer that claims `Esc` before the
        // ladder underneath (ADR 0017 D4), and it claims every printable key
        // besides, so it is answered here beside the filter and search surfaces
        // it belongs with rather than anywhere further down.
        if matches!(self.mode, Mode::Input(_)) {
            return self.handle_input_key(code);
        }
        if let Mode::Filter = self.mode {
            // The filter is a text-input surface; typeahead never applies here
            // (ADR 0002). Every printable key spells the fuzzy query.
            match code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.mode = Mode::Browse;
                    self.refilter();
                }
                KeyCode::Enter => self.mode = Mode::Browse,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                KeyCode::Down => self.move_sel(1),
                KeyCode::Up => self.move_sel(-1),
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                }
                _ => {}
            }
            return None;
        }

        if let Mode::Search = self.mode {
            // Recursive search is a text-input surface like the filter; typeahead
            // never applies (ADR 0007 D1 — its own mode, own key buffer). Input
            // handling MIRRORS the filter's, but the semantics differ: a keystroke
            // restarts a background tree walk rather than narrowing the listing.
            let half = (self.viewport_h / 2).max(1) as isize;
            match code {
                KeyCode::Esc => self.exit_search(),
                KeyCode::Enter | KeyCode::Right => return self.activate_search(),
                KeyCode::Backspace => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.pop();
                    }
                    self.restart_search();
                }
                KeyCode::Down => self.search_move(1),
                KeyCode::Up => self.search_move(-1),
                KeyCode::PageDown => self.search_move(half),
                KeyCode::PageUp => self.search_move(-half),
                KeyCode::Char(c) => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.push(c);
                    }
                    self.restart_search();
                }
                _ => {}
            }
            return None;
        }

        // The which-key overlay is up (only reachable in browse mode): the next
        // key dismisses it and is otherwise swallowed — the which-key convention.
        // Handling it here also keeps `help` a browse-only invariant: any key that
        // would enter filter/search is consumed by the dismiss first.
        if self.help {
            self.help = false;
            return None;
        }

        // Browse mode. Typeahead runs BEFORE the normal key handling: a timed
        // name-buffer that coexists with the vim motions (ADR 0002 D1).
        let now = Instant::now();
        let is_active = typeahead::active(now, self.typeahead_at, typeahead::TIMEOUT);

        // A session that lapsed on the timeout leaves a stale buffer and its
        // "type: …" hint; drop both before normal handling so the key help
        // returns (but never clobber an unrelated status like "no viewer for …").
        if !is_active && !self.typeahead.is_empty() {
            self.typeahead.clear();
            if self
                .status
                .as_deref()
                .is_some_and(|s| s.starts_with("type: "))
            {
                self.status = None;
            }
        }

        // While a session is live, Esc cancels it and Backspace edits the
        // buffer, instead of their normal browse meanings (ADR 0002 D3).
        if is_active {
            match code {
                KeyCode::Esc => {
                    self.cancel_typeahead();
                    return None;
                }
                KeyCode::Backspace => {
                    self.typeahead.pop();
                    self.typeahead_at = Some(now);
                    self.apply_typeahead();
                    return None;
                }
                _ => {}
            }
        }

        // `Ctrl-a` marks every entry in the current filtered view (ADR 0017 D4).
        // This is the one mark binding that does NOT live in `browse_char`, and
        // for a structural reason: `browse_char` maps a bare character, so it has
        // no way to say "with Control held", and registering a plain `a` there
        // would spend a letter the ADR wants left alone. Placing the arm above the
        // typeahead candidate block below is safe because that block never buffers
        // a key with Ctrl or Alt held (its `ctrl_alt` check), so `Ctrl-a` could not
        // have started a name search either way and ADR 0002 D2 is untouched: the
        // set of bound plain CHARACTERS is still exactly `browse_char`.
        if code == KeyCode::Char('a') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let rows = self.view_rows();
            self.marks
                .mark_all(rows.iter().map(|(p, s, d)| (p.as_path(), *s, *d)));
            return None;
        }

        // A printable char with no Ctrl/Alt held is a typeahead candidate; its
        // fate is the session × binding precedence. Ctrl/Alt keys (and Shift,
        // already folded into the char) are never buffered.
        if let KeyCode::Char(c) = code {
            let ctrl_alt = key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            if !ctrl_alt {
                match typeahead::action(is_active, browse_char(c).is_some()) {
                    typeahead::Action::Append => {
                        self.typeahead.push(c);
                        self.typeahead_at = Some(now);
                        self.apply_typeahead();
                        return None;
                    }
                    typeahead::Action::StartNew => {
                        self.typeahead = c.to_string();
                        self.typeahead_at = Some(now);
                        self.apply_typeahead();
                        return None;
                    }
                    // Idle + bound: fall through so the vim motion runs.
                    typeahead::Action::PassThrough => {}
                }
            }
        }

        // Normal browse handling. Char-driven actions route through the single
        // `browse_char` source of truth; the non-char keys keep their own arms.
        let half = (self.viewport_h / 2).max(1) as isize;
        match code {
            KeyCode::Char(c) => {
                if let Some(action) = browse_char(c) {
                    return self.run_char_action(action);
                }
            }
            // `Esc` backs out exactly one layer per press (ADR 0017 D4, extended
            // to the clipboard and to a run in flight). The order lives in the
            // pure [`escape`] ladder; this arm only performs what it decided and
            // says which layer went, because a key that quietly undid a layer the
            // user could not see would be the same silent surprise the whole ADR
            // is written against. The modal surfaces above (an operation overlay,
            // filter, search, a live typeahead session) each claim `Esc` for their
            // own layer before this, which is the same rule one level up.
            KeyCode::Esc => match escape(
                self.op.is_some(),
                self.clip.is_some(),
                !self.marks.is_empty(),
            ) {
                Escape::CancelOp => {
                    if let Some(run) = self.op.as_ref() {
                        // The worker notices the flag between steps and sends its
                        // `Done` with the journal of what it actually completed, so
                        // `finish_op` reports the partial run through the existing
                        // path. Nothing is torn down here: dropping the `Run` would
                        // cancel it too, but silently, with the report lost.
                        run.cancel();
                    }
                    self.status = Some("cancelling: it will report what it finished".to_string());
                }
                Escape::ClearClip => {
                    self.clip = None;
                    self.status = Some("clipboard cleared".to_string());
                }
                Escape::ClearMarks => self.marks.clear(),
                Escape::Quit => return Some(Action::Quit),
            },
            KeyCode::Down => self.move_sel(1),
            KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(half),
            KeyCode::PageUp => self.move_sel(-half),
            KeyCode::Home => self.state.select(Some(0)),
            KeyCode::End => {
                if !self.view.is_empty() {
                    self.state.select(Some(self.view.len() - 1));
                }
            }
            KeyCode::Enter | KeyCode::Right => return self.activate(),
            KeyCode::Left | KeyCode::Backspace => self.go_parent(),
            _ => {}
        }
        None
    }

    /// Execute a char-driven browse action via the existing helpers. The single
    /// consumer of [`browse_char`], so typeahead's `is_bound` and the real
    /// handler can never disagree about which chars are bound (ADR 0002 D2).
    fn run_char_action(&mut self, action: CharAction) -> Option<Action> {
        let half = (self.viewport_h / 2).max(1) as isize;
        match action {
            CharAction::Down => self.move_sel(1),
            CharAction::Up => self.move_sel(-1),
            CharAction::HalfDown => self.move_sel(half),
            CharAction::HalfUp => self.move_sel(-half),
            CharAction::Top => self.state.select(Some(0)),
            CharAction::Bottom => {
                if !self.view.is_empty() {
                    self.state.select(Some(self.view.len() - 1));
                }
            }
            CharAction::Open => return self.activate(),
            CharAction::OpenExternal => {
                if let Some(e) = self.selected() {
                    crate::util::open_in_native_app(&e.path.to_string_lossy());
                }
            }
            CharAction::Parent => self.go_parent(),
            CharAction::Filter => {
                self.mode = Mode::Filter;
                self.filter.clear();
                self.status = None; // drop any stale "type: …" hint
                self.refilter();
            }
            CharAction::Search => {
                // Enter search with a blank prompt and no walk yet — the first
                // keystroke starts one (a blank query must not walk; ADR 0007 D3).
                self.mode = Mode::Search;
                self.search = Some(SearchState::new());
                self.status = None; // drop any stale "type: …" hint
            }
            CharAction::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.refilter();
            }
            CharAction::ToggleLayout => self.layout = self.layout.cycle(),
            CharAction::ToggleMeta => self.meta = self.meta.toggle(),
            CharAction::CycleSort => {
                self.sort.key = self.sort.key.cycle();
                self.resort();
            }
            CharAction::ReverseSort => {
                self.sort.reverse = !self.sort.reverse;
                self.resort();
            }
            CharAction::Help => self.help = !self.help,
            CharAction::ToggleMark => {
                // Toggle the row under the cursor, then step down one row, so
                // holding `Space` marks a run (ADR 0017 D4). The size and kind are
                // the ones the listing is showing; `Marks` documents them as a
                // snapshot that the operation engine re-checks against the real
                // filesystem before acting.
                if let Some(e) = self.selected() {
                    let path = e.path.clone();
                    let (size, is_dir) = (e.size, e.kind == Format::Directory);
                    self.marks.toggle(&path, size, is_dir);
                    let next = mark_advance(self.state.selected(), self.view.len());
                    self.state.select(next);
                }
            }
            CharAction::InvertMarks => {
                // Only the CURRENT filtered view is inverted, in listing order.
                // Rows hidden by the filter or by the dot toggle are not in
                // `view`, so `V` never touches what the user cannot see, and marks
                // held in other directories survive because they are outside this
                // listing entirely (ADR 0017 D3).
                let rows = self.view_rows();
                self.marks
                    .invert(rows.iter().map(|(p, s, d)| (p.as_path(), *s, *d)));
            }
            // Resolve the trash operation and show the plan; nothing is mutated
            // until the overlay is authorised (ADR 0017 D5).
            CharAction::Trash => self.request_trash(),
            // The clipboard keys (ADR 0017 D4). Loading it is free and reversible;
            // pasting goes through the confirm overlay like every other mutation.
            CharAction::Yank => self.load_clip(false),
            CharAction::Cut => self.load_clip(true),
            CharAction::Paste => self.request_paste(),
            // The typed-name operations (ADR 0017 D4). Both only OPEN a prompt
            // here; nothing is planned until `Enter`, and nothing is mutated
            // until the plan resolves.
            CharAction::Rename => self.request_rename(),
            CharAction::Create => self.request_create(),
            // Undo goes straight to the executor: the journal it pops is already
            // the decided list of what to reverse, so there is nothing to show for
            // authorisation the way a plan has (ADR 0017 D8).
            CharAction::Undo => self.undo_last(),
            // The one key in this group that changes nothing on disk, so it neither
            // plans nor confirms nor waits its turn behind a running operation.
            CharAction::YankPath => self.yank_paths(),
            CharAction::Quit => {
                // Quitting mid-operation would leave a half-copied tree behind and
                // no report of what happened, because dropping the `Run` trips its
                // cancel flag on the way out. That is exactly the silent partial
                // ADR 0009 forbids, so `q` is refused while a run is live and the
                // refusal names the key that ends it properly.
                if self.op.is_some() {
                    self.status = Some("an operation is running: [Esc] cancels it".to_string());
                    return None;
                }
                return Some(Action::Quit);
            }
        }
        None
    }

    /// Move the cursor to the first entry matching the current buffer, echoing
    /// it in the status. A miss keeps the buffer and leaves the cursor put — a
    /// silent no-op that still shows what was typed (ADR 0002 D3).
    fn apply_typeahead(&mut self) {
        let idx = {
            let names: Vec<&str> = self
                .view
                .iter()
                .map(|&i| self.all[i].name.as_str())
                .collect();
            typeahead::match_prefix(&names, &self.typeahead)
        };
        if let Some(i) = idx {
            self.state.select(Some(i));
        }
        self.status = Some(format!("type: {}", self.typeahead));
    }

    /// End the session: drop the buffer, its timestamp, and the status hint.
    fn cancel_typeahead(&mut self) {
        self.typeahead.clear();
        self.typeahead_at = None;
        self.status = None;
    }

    /// Insert a finished image into the LRU cache (main-thread owned, cap 8).
    fn cache_put(&mut self, path: PathBuf, img: DynamicImage) {
        self.img_cache.push((path, img));
        if self.img_cache.len() > 8 {
            self.img_cache.remove(0);
        }
    }

    /// Install a decoded still image into the pane as the live preview. A still
    /// clears `preview_animated`, so any prior GIF's ticking stops.
    fn show_image(&mut self, img: DynamicImage) {
        if let Some(pane) = self.pane.as_mut() {
            pane.set(img);
        }
        self.pv = Pv::Image;
        self.preview_animated = false;
    }

    /// Install an animated GIF's frames into the pane as the live preview and
    /// mark it animated so `main_loop` ticks it. Deliberately NOT cached in
    /// `img_cache` (bounded, and frame sets are large); reselecting the GIF
    /// re-decodes off-thread — cheap and backgrounded (ADR 0005 D1).
    fn show_animation(&mut self, frames: Vec<media::Frame>) {
        if let Some(pane) = self.pane.as_mut() {
            pane.set_animation(frames);
        }
        self.pv = Pv::Image;
        self.preview_animated = true;
    }

    /// Drive the single-worker async rasteriser. Runs every main-loop tick:
    /// drains finished posters, retires the in-flight job, and — if the worker
    /// is idle — starts the latest wanted raster (or installs it straight from
    /// cache). Returns true if the preview changed and a redraw is due.
    fn pump_raster(&mut self) -> bool {
        let mut dirty = false;
        // Match finished/queued rasters against the mode-aware selection so image /
        // PDF / video HITS render in search mode too (ADR 0007 D5): with the browse
        // `selected()` here a hit's poster would be judged "stale" (its path never
        // equals the browse cursor) and never install. `cur_sel` equals `selected()`
        // in browse/filter, so that path is byte-for-byte unchanged.
        let cur = self.cur_sel().map(|s| s.path);

        // 1. Drain completed rasters. Cache every finished STILL (animations are
        //    never cached — see `show_animation`); only touch the pane when the
        //    finished path is still the current selection.
        while let Ok((path, result)) = self.raster_rx.try_recv() {
            if let Some(Rastered::Still(img)) = &result {
                self.cache_put(path.clone(), img.clone());
            }
            if self.raster_pending.as_deref() == Some(path.as_path()) {
                self.raster_pending = None;
            }
            if Some(&path) != cur.as_ref() {
                continue; // stale: scrolled away — keep it cached, leave the pane
            }
            match result {
                Some(Rastered::Still(img)) => self.show_image(img),
                Some(Rastered::Animated(frames)) => self.show_animation(frames),
                None => {
                    // Raster failed (e.g. pdftocairo/ffmpeg missing): degrade to
                    // the text preview. The header lines are already in `preview`
                    // from build_preview; append the "no preview" note.
                    self.preview.push(no_preview());
                    self.pv = Pv::Text;
                }
            }
            dirty = true;
        }

        // 2. If the worker is idle, launch the latest wanted raster. The
        //    want/pending split coalesces fast scrolling: only the final landing
        //    spot is ever started once the in-flight job drains.
        if self.raster_pending.is_none() {
            if let Some((path, kind)) = self.raster_want.take() {
                if let Some((_, img)) = self.img_cache.iter().find(|(p, _)| *p == path) {
                    // Became available while waiting — install without a worker.
                    if Some(&path) == cur.as_ref() {
                        let img = img.clone();
                        self.show_image(img);
                        dirty = true;
                    }
                } else {
                    let tx = self.raster_tx.clone();
                    let p = path.clone();
                    thread::spawn(move || {
                        // An image may be an animated GIF: try frames first, and
                        // fall back to a single still decode (which also covers a
                        // static/oversized GIF, where `decode_frames` returns None).
                        // Every other format is always a single still poster.
                        let result: Option<Rastered> = match kind {
                            Format::Image => media::decode_frames(&p)
                                .map(Rastered::Animated)
                                .or_else(|| {
                                    // Decode by content with pixel limits (ADR
                                    // 0009): sniffs magic bytes so a misnamed
                                    // JPEG-as-.png still previews, and the browser
                                    // rasters this merely on scroll-past, so a file
                                    // claiming enormous dimensions must not force a
                                    // huge allocation here.
                                    crate::util::open_image_reader(&p)
                                        .ok()
                                        .and_then(|r| r.decode().ok())
                                        .map(Rastered::Still)
                                }),
                            Format::Pdf => crate::pdf::poster(&p.to_string_lossy())
                                .ok()
                                .map(Rastered::Still),
                            Format::Video => crate::video::poster(&p.to_string_lossy())
                                .ok()
                                .map(Rastered::Still),
                            Format::Svg => crate::svg::render_svg(&p.to_string_lossy())
                                .ok()
                                .map(Rastered::Still),
                            Format::Keynote => crate::keynote::preview_image(&p.to_string_lossy())
                                .ok()
                                .map(Rastered::Still),
                            _ => None,
                        };
                        let _ = tx.send((p, result));
                    });
                    self.raster_pending = Some(path);
                }
            }
        }

        dirty
    }

    fn build_preview(&mut self) {
        self.preview.clear();
        self.pv = Pv::Text;
        // A new selection is not (yet) an animation; clear the flag so any prior
        // GIF's ticking stops the moment the cursor moves off it — no idle churn
        // on the next selection until an `Animated` raster actually installs.
        self.preview_animated = false;
        // A new selection redefines what wants rastering; drop any stale want so
        // fast-scrolling onto a cached/text entry cancels the previous request.
        self.raster_want = None;
        // Source the selection from the mode-aware accessor so a search hit renders
        // through this exact pipeline (ADR 0007 D5); in browse/filter this is the
        // browsed entry, unchanged. Owned, so `self` is free to mutate below.
        let Some(sel) = self.cur_sel() else { return };
        let name = sel.name;
        let kind = sel.kind;
        let size = sel.size;
        let modified = sel.modified;
        let path = sel.path;

        // Caption / header: name, type · size · modified.
        let mut meta = kind.label().to_string();
        if kind != Format::Directory {
            meta.push_str(&format!("  ·  {}", crate::util::human_size(size)));
        }
        if let Some(m) = modified {
            meta.push_str(&format!("  ·  {}", crate::util::rel_time(m)));
        }

        // Text header (also the body a failed async raster degrades back to).
        self.preview.push(Line::from(Span::styled(
            name.clone(),
            Style::default()
                .fg(kind.color())
                .add_modifier(Modifier::BOLD),
        )));
        self.preview.push(Line::from(Span::styled(
            meta.clone(),
            Style::default().fg(theme::palette().dim),
        )));
        self.preview.push(Line::from(""));

        // Pixel previews: async when a graphics pane exists. A cache hit installs
        // instantly; otherwise show a placeholder and queue the one background
        // raster. Graphics-less terminals fall straight through to text below.
        if matches!(
            kind,
            Format::Image | Format::Svg | Format::Pdf | Format::Video | Format::Keynote
        ) && self.pane.is_some()
        {
            let extra = match kind {
                Format::Image => crate::util::image_dimensions(&path)
                    .map(|(w, h)| format!("  ·  {w}×{h}"))
                    .unwrap_or_default(),
                Format::Video => "  ·  Enter to play".into(),
                Format::Pdf => "  ·  page 1".into(),
                Format::Svg => "  ·  Enter for source".into(),
                Format::Keynote => "  ·  preview".into(),
                _ => String::new(),
            };
            self.caption = format!("{name}   {meta}{extra}");
            if let Some((_, img)) = self.img_cache.iter().find(|(p, _)| *p == path) {
                let img = img.clone();
                self.show_image(img); // instant on revisit
            } else {
                self.pv = Pv::Loading;
                self.raster_want = Some((path, kind));
            }
            return;
        }

        match kind {
            Format::Directory => self.preview_dir(&path),
            Format::Markdown => self.preview_markdown(read_capped(&path)),
            Format::Html => {
                // .html → markdown (ADR 0008); on failure show no preview.
                match crate::html::to_markdown(&path.to_string_lossy()) {
                    Ok(src) => self.preview_markdown(src),
                    Err(_) => self.preview.push(no_preview()),
                }
            }
            Format::Docx => {
                // .docx → markdown; on failure show no preview.
                match crate::docx::to_markdown(&path.to_string_lossy()) {
                    Ok(src) => self.preview_markdown(src),
                    Err(_) => self.preview.push(no_preview()),
                }
            }
            Format::Pptx => {
                // .pptx → markdown (slide text); on failure show no preview.
                match crate::pptx::to_markdown(&path.to_string_lossy()) {
                    Ok(src) => self.preview_markdown(src),
                    Err(_) => self.preview.push(no_preview()),
                }
            }
            Format::Epub => {
                // .epub → markdown (spine chapters); on failure show no preview.
                match crate::epub::to_markdown(&path.to_string_lossy()) {
                    Ok(src) => self.preview_markdown(src),
                    Err(_) => self.preview.push(no_preview()),
                }
            }
            Format::Ipynb => {
                // .ipynb → markdown (cells + outputs); on failure show no preview.
                match crate::ipynb::to_markdown(&path.to_string_lossy()) {
                    Ok(src) => self.preview_markdown(src),
                    Err(_) => self.preview.push(no_preview()),
                }
            }
            // Data files (ADR 0016) preview through the same grid as spreadsheets.
            Format::Sheet | Format::Data => self.preview_sheet(&path),
            Format::Archive => self.preview_archive(&path),
            Format::Binary => self.preview_hex(&path),
            // Everything else — including Image/Pdf/Video whose pixel attempt
            // failed above — shows the file head; `head_text` self-guards and
            // yields "No preview" for binary/NUL content.
            _ => self.preview_text_head(&path),
        }
    }

    fn preview_dir(&mut self, path: &Path) {
        let mut kids: Vec<(String, bool)> = match fs::read_dir(path) {
            Ok(rd) => rd
                .flatten()
                .map(|c| {
                    let n = c.file_name().to_string_lossy().into_owned();
                    (n, c.path().is_dir())
                })
                .filter(|(n, _)| self.show_hidden || !n.starts_with('.'))
                .collect(),
            Err(_) => {
                self.preview.push(no_preview());
                return;
            }
        };
        kids.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        let total = kids.len();
        if total == 0 {
            self.preview.push(Line::from(Span::styled(
                "empty",
                Style::default().fg(theme::palette().dim),
            )));
            return;
        }
        self.preview.insert(
            2,
            Line::from(Span::styled(
                format!("{total} items"),
                Style::default().fg(theme::palette().dim),
            )),
        );
        for (n, d) in kids.into_iter().take(300) {
            let (c, suffix) = if d {
                (theme::palette().dir, "/")
            } else {
                (theme::palette().other, "")
            };
            self.preview.push(Line::from(Span::styled(
                format!("{n}{suffix}"),
                Style::default().fg(c),
            )));
        }
    }

    fn preview_markdown(&mut self, src: String) {
        let width = preview_text_width();
        let (lines, _, _) = crate::markdown::Rendered::build(&src).layout(width);
        self.preview.extend(lines.into_iter().take(600));
    }

    /// Spreadsheet preview: the first rows/cols rendered as an aligned grid (the
    /// first row styled as a header). Covers both the binary workbooks — which
    /// otherwise fell through to the text-head previewer and showed "No preview"
    /// — and csv/tsv, which now format as a table instead of raw delimited text.
    fn preview_sheet(&mut self, path: &Path) {
        const MAX_COLS: usize = 20;
        const COL_CAP: usize = 18; // max display width of any one column
        let Some(rows) = crate::sheet::preview_grid(&path.to_string_lossy(), 200, MAX_COLS) else {
            self.preview.push(no_preview());
            return;
        };
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![1usize; ncols];
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count()).min(COL_CAP);
            }
        }
        for (ri, r) in rows.iter().take(400).enumerate() {
            let spans: Vec<Span> = (0..ncols)
                .map(|i| {
                    let cell = r.get(i).map(String::as_str).unwrap_or("");
                    let color = if ri == 0 {
                        theme::palette().accent
                    } else {
                        theme::palette().other
                    };
                    Span::styled(
                        format!("{}  ", pad_cell(cell, widths[i])),
                        Style::default().fg(color),
                    )
                })
                .collect();
            self.preview.push(Line::from(spans));
        }
    }

    /// Archive table-of-contents preview: `size  name` per entry (capped).
    fn preview_archive(&mut self, path: &Path) {
        match crate::archive::entries(&path.to_string_lossy()) {
            Ok(list) => {
                self.preview.insert(
                    2,
                    Line::from(Span::styled(
                        format!("{} entries", list.len()),
                        Style::default().fg(theme::palette().dim),
                    )),
                );
                for e in list.into_iter().take(500) {
                    let size = if e.is_dir {
                        "     dir".to_string()
                    } else {
                        format!("{:>8}", crate::util::human_size(e.size))
                    };
                    let color = if e.is_dir {
                        theme::palette().dir
                    } else {
                        theme::palette().other
                    };
                    self.preview.push(Line::from(vec![
                        Span::styled(
                            format!("{size}  "),
                            Style::default().fg(theme::palette().dim),
                        ),
                        Span::styled(e.name, Style::default().fg(color)),
                    ]));
                }
            }
            Err(_) => self.preview.push(no_preview()),
        }
    }

    /// Binary preview: the head rendered as a canonical hexdump (capped rows).
    fn preview_hex(&mut self, path: &Path) {
        for line in crate::hex::preview(&path.to_string_lossy(), 500) {
            self.preview.push(Line::from(Span::styled(
                line,
                Style::default().fg(theme::palette().other),
            )));
        }
    }

    /// Render the file head. A recognised source/text extension is
    /// syntax-highlighted (its own language syntax, or [`highlight::PLAIN`] for
    /// plain-text types like `txt`/`csv`); an unknown extension that still decodes
    /// as text is shown flat in the plain colour, exactly as before.
    fn preview_text_head(&mut self, path: &Path) {
        let Some(text) = head_text(path, 64 * 1024, 500) else {
            self.preview.push(no_preview());
            return;
        };
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !highlight::is_text_ext(&ext) {
            // Unrecognised type: keep the original flat, single-colour rendering.
            for l in text.lines() {
                self.preview.push(Line::from(Span::styled(
                    l.to_string(),
                    Style::default().fg(theme::palette().other),
                )));
            }
            return;
        }
        let syntax = highlight::syntax_for(&ext).unwrap_or(highlight::PLAIN);
        for line in highlight::highlight(&text, syntax) {
            let spans: Vec<Span> = line
                .into_iter()
                .map(|tok| {
                    Span::styled(tok.text, Style::default().fg(theme::token_color(tok.kind)))
                })
                .collect();
            self.preview.push(Line::from(spans));
        }
    }

    fn render(&mut self, f: &mut Frame) {
        // Search mode draws its own frame (input line + results | preview + status),
        // reusing the preview pane verbatim (ADR 0007 D5/§10); the browse layout is
        // skipped entirely.
        if matches!(self.mode, Mode::Search) {
            self.render_search(f);
            return;
        }
        let area = f.area();
        let rows = RtLayout::default()
            .constraints([
                Constraint::Length(1), // breadcrumb
                Constraint::Min(0),    // body
                Constraint::Length(1), // status
            ])
            .split(area);

        self.render_crumb(f, rows[0]);

        // Reduce the layout mode to a concrete column count for this frame, then
        // compose. In Filter mode the current pane's border shifts to the filter
        // yellow (as before); the parent pane is always inactive.
        let has_parent = self.cwd.parent().is_some();
        let filter = matches!(self.mode, Mode::Filter);
        // The eased fade factor for the CURRENT pane after a directory change
        // (ADR 0006 D3): eased progress in 0..1, or `None` when no fade is live.
        // Read once here at the render edge (the clock lives only at the edges).
        // Only the current pane fades — the parent (Miller) pane didn't change, so
        // it always gets `None`. At progress 1.0 the eased factor is 1.0 and every
        // lerp is the identity, so the final frame equals the non-animated render.
        // Read the clock once at this render edge and reuse it for both the fade
        // and the slide (the clock lives only at the edges — ADR 0006).
        let now = Instant::now();
        let fade_t = self
            .fade
            .map(|a| crate::anim::ease_out_cubic(a.progress(now)));
        if effective_columns(self.layout, area.width, has_parent) == 3 {
            // Miller: parent | current | preview  (~[20%, 34%, 46%]).
            let cols = RtLayout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20),
                    Constraint::Percentage(34),
                    Constraint::Percentage(46),
                ])
                .split(rows[1]);
            self.render_parent(f, cols[0]);
            // Record both panes as click-hit-test surfaces (ADR 0005 D2): the
            // current list is the middle column, the parent the left one.
            self.list_area = cols[1];
            self.parent_area = Some(cols[0]);
            // `viewport_h` drives half-page paging and must track the CURRENT
            // pane — the middle column here.
            self.viewport_h = cols[1].height.saturating_sub(2);
            // Build the view from direct fields (not a `&self` helper) so the
            // shared borrows of `all`/`view` stay disjoint from `&mut state`.
            let view = EntryListView {
                entries: &self.all,
                order: &self.view,
                selected: self.state.selected(),
                title: format!(" {} ", self.view.len()),
                git: self.git.as_ref(), // current pane's gutter (D2).
                meta: self.meta,
                fade_t, // the current pane fades in after a dir change (D3).
                // The mark gutter is a CURRENT-pane affair, and appears only once
                // something is marked (ADR 0017 D3).
                marks: mark_gutter(&self.marks),
            };
            // Only the CURRENT (middle) pane slides; the parent and preview render
            // statically (ADR 0006 D3). A live slide composes the static block plus
            // the old/new inner blits; otherwise the normal one-shot render.
            match self.slide.as_ref().filter(|s| !s.anim.done(now)) {
                Some(slide) => render_entry_slide(
                    f,
                    cols[1],
                    &view,
                    &self.state,
                    self.icons,
                    true,
                    filter,
                    slide,
                    now,
                ),
                None => {
                    render_entry_list(f, cols[1], &view, &mut self.state, true, filter, self.icons)
                }
            }
            self.render_preview(f, cols[2]);
        } else {
            // Double: current | preview — the classic, byte-for-byte split.
            let cols = RtLayout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(rows[1]);
            // Record the current list as the click-hit-test surface; no parent
            // pane exists in the two-column split (ADR 0005 D2).
            self.list_area = cols[0];
            self.parent_area = None;
            self.viewport_h = cols[0].height.saturating_sub(2);
            let view = EntryListView {
                entries: &self.all,
                order: &self.view,
                selected: self.state.selected(),
                title: format!(" {} ", self.view.len()),
                git: self.git.as_ref(), // current pane's gutter (D2).
                meta: self.meta,
                fade_t, // the current pane fades in after a dir change (D3).
                // The mark gutter is a CURRENT-pane affair, and appears only once
                // something is marked (ADR 0017 D3).
                marks: mark_gutter(&self.marks),
            };
            // The current pane slides; the preview renders statically (ADR 0006 D3).
            match self.slide.as_ref().filter(|s| !s.anim.done(now)) {
                Some(slide) => render_entry_slide(
                    f,
                    cols[0],
                    &view,
                    &self.state,
                    self.icons,
                    true,
                    filter,
                    slide,
                    now,
                ),
                None => {
                    render_entry_list(f, cols[0], &view, &mut self.state, true, filter, self.icons)
                }
            }
            self.render_preview(f, cols[1]);
        }

        self.render_status(f, rows[2]);

        // The which-key overlay draws last, over everything (ADR 0007's search
        // frame returns early above, so this is browse/filter only — and `help` is
        // browse-only anyway). `Clear` punches a hole so the popup isn't see-through.
        if self.help {
            render_browse_help(f, area, self.sort);
        }
        // The file-operation overlay draws last of all, over the help overlay too
        // (which `show_op` has already dismissed, so in practice they never
        // coexist). `Mode::Op` renders the ordinary browse layout underneath on
        // purpose: the confirm popup is a question about the listing behind it,
        // and hiding that listing would take away the context the answer needs.
        if let Mode::Op(view) = &self.mode {
            render_op_overlay(f, area, view);
        }
    }

    /// Render the parent-directory pane (Miller's left column): the siblings of
    /// `cwd` with the current directory highlighted. Navigation context only —
    /// always inactive (dim border), no size column, no git, no preview. A no-op
    /// when there is no parent (the caller only reaches three columns when one
    /// exists, but stay honest).
    fn render_parent(&self, f: &mut Frame, area: Rect) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };
        // The parent listing is cached in `self.parent` (kept sorted by `load`/
        // `resort`), so this pane costs no directory read on a render frame.
        let entries = &self.parent;
        // Same hidden-file policy as the current pane, but no smart-query filter:
        // the parent is context, so every visible sibling shows in sorted order.
        let order: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_hidden || !e.name.starts_with('.'))
            .map(|(i, _)| i)
            .collect();
        // Land the highlight on the directory we're currently inside.
        let here = self
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let selected = here
            .as_deref()
            .and_then(|name| order.iter().position(|&i| entries[i].name == name));
        let mut state = ListState::default();
        state.select(selected);
        let view = EntryListView {
            entries,
            order: &order,
            selected,
            title: format!(" {} ", pretty_dir_name(&parent)),
            git: None,
            // No mark gutter here either (ADR 0017 D3). The parent pane is
            // navigation context, not a surface you select on: marks are only ever
            // set from the current listing, which is exactly why this pane also
            // carries `MetaCol::None` and no git gutter.
            marks: None,
            meta: MetaCol::None, // no trailing column — cleaner context pane.
            fade_t: None,        // the parent pane didn't change — never fades (D3).
        };
        // The parent context pane didn't change on this navigation, so it never
        // fades — always `None` in the view (ADR 0006 D3).
        render_entry_list(f, area, &view, &mut state, false, false, self.icons);
    }

    fn render_crumb(&mut self, f: &mut Frame, area: Rect) {
        // Clickable, two-tone breadcrumb (ADR 0005 D2). The path is laid out as a
        // sequence of segments joined by `/`: every parent segment recedes in
        // `dim`, the final segment — the directory you're actually in — pops in
        // accent + BOLD, preserving the prior two-tone look (e.g. `~/foo/` dim +
        // `bar` accent). As each label is placed we record its exact column span
        // and absolute target so a left-click there navigates to it (`crumb_hit`).
        let accent = theme::palette().accent;
        let dim = theme::palette().dim;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let segments = crumb_segments(&self.cwd, home.as_deref());
        let last = segments.len().saturating_sub(1);

        self.crumb_hits.clear();
        let mut spans = vec![Span::raw(" ")];
        // Track the column where the NEXT span begins. The leading space occupies
        // column `area.x`; labels and separators advance `x` by their width so
        // each recorded hit-range is in real screen columns.
        let mut x = area.x.saturating_add(1);
        let mut prev_ends_slash = false;
        for (idx, (label, target)) in segments.iter().enumerate() {
            // Separator between segments, except after a label that already ends
            // in '/' (the filesystem-root `/` segment) — avoids a doubled slash.
            if idx > 0 && !prev_ends_slash {
                spans.push(Span::styled("/".to_string(), Style::default().fg(dim)));
                x = x.saturating_add(1);
            }
            let w = label.chars().count() as u16;
            let start = x;
            let style = if idx == last {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(dim)
            };
            spans.push(Span::styled(label.clone(), style));
            x = x.saturating_add(w);
            self.crumb_hits.push((start..x, target.clone()));
            prev_ends_slash = label.ends_with('/');
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);

        // Repo HEAD readout, right-aligned on the same row (ADR 0004, D2
        // amendment): `⎇ branch ↑a ↓b ●` — branch (or detached short oid),
        // ahead/behind vs upstream when set, and a dirty dot when the status map
        // is non-empty. Dropped whole when it would collide with the crumbs (the
        // path always wins) — `x` is the column right after the last crumb.
        if let Some(head) = &self.head {
            let dirty = self.git.as_ref().is_some_and(|m| !m.is_empty());
            let git_spans = head_spans(head, dirty, self.icons);
            let w: u16 = git_spans
                .iter()
                .map(|s| s.content.chars().count() as u16)
                .sum();
            let right = area.x + area.width;
            if w > 0 && x + 2 + w <= right {
                let rect = Rect::new(right - w, area.y, w, 1);
                f.render_widget(Paragraph::new(Line::from(git_spans)), rect);
            }
        }
    }

    fn render_preview(&mut self, f: &mut Frame, area: Rect) {
        // The preview is the passive pane (see `render_entry_list`): a dim, rounded
        // border that sits quietly behind the accent-lit list. Captions keep
        // their truncation but gain the accent+BOLD styling of the list title.
        match self.pv {
            Pv::Image => {
                let block = preview_block(caption_title(&self.caption, area));
                let inner = block.inner(area);
                f.render_widget(block, area);
                if let Some(pane) = self.pane.as_mut() {
                    pane.render(f, inner);
                }
            }
            Pv::Loading => {
                // Placeholder while the background worker rasters. Never render
                // the (possibly stale) pane here — only a caption + dim note. The
                // braille spinner frame is picked by `spin`, which `main_loop`
                // advances every ~60 ms while the raster is live (ADR 0004 D3).
                let block = preview_block(caption_title(&self.caption, area));
                let inner = block.inner(area);
                f.render_widget(block, area);
                let frame = SPINNER[self.spin % SPINNER.len()];
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("{frame} rendering…"),
                        Style::default().fg(theme::palette().dim),
                    ))),
                    inner,
                );
            }
            Pv::Text => {
                let block = preview_block(preview_caption(" Preview "));
                let inner_h = area.height.saturating_sub(2) as usize;
                let text: Vec<Line> = self.preview.iter().take(inner_h).cloned().collect();
                f.render_widget(Paragraph::new(Text::from(text)).block(block), area);
            }
        }
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        // The inline prompt is the one status line that cannot be a single
        // uniformly coloured string, because it has to show where the cursor is,
        // so it returns here with its own spans (see [`prompt_spans`]). Every
        // other mode shares the one-string path below.
        if let Mode::Input(prompt) = &self.mode {
            f.render_widget(
                Paragraph::new(Line::from(prompt_spans(
                    prompt.ask.prefix(),
                    &prompt.edit,
                    prompt.ask.hint(),
                ))),
                area,
            );
            return;
        }
        let txt = if let Mode::Filter = self.mode {
            // Teach the smart-query syntax until the user is already using a
            // predicate, then drop the hint for a cleaner line.
            let hint = if query::parse(&self.filter).has_predicates() {
                "[Enter] keep  [Esc] clear"
            } else {
                "text + kind: ext: size: modified:   [Enter] keep  [Esc] clear"
            };
            format!(" /{}    {hint}", self.filter)
        } else if let Mode::Op(view) = &self.mode {
            // An overlay owns the screen, so the line under it names the keys that
            // answer it rather than repeating state the popup already shows.
            match view {
                OpView::Confirm(pending) => {
                    format!(
                        " {}    {}",
                        pending.plan.summary(),
                        confirm_keys(pending.inputs.is_some(), true)
                    )
                }
                OpView::Report(report) => {
                    format!(" {}    [any key] close", report_count(report))
                }
            }
        } else if let Some(flight) = &self.op_progress {
            // The in-flight line outranks `self.status` and the mark line both,
            // and for the same reason in each case: it is the only line here that
            // changes on its own. A `self.status` left over from before the run
            // started (the sort blurb, a "no viewer for …") would otherwise hide an
            // operation that has no other way to show itself, and the mark line is
            // standing state that is about to be cleared by the run anyway. Filter
            // mode still wins above, because that line is being typed into.
            format!(
                " {}",
                op_progress_status(&flight.label, flight.items, flight.total, &flight.current)
            )
        } else if let Some(s) = &self.status {
            format!(" {s}")
        } else if !self.marks.is_empty() {
            // A selection held across directories must never be invisible state
            // (ADR 0017 D3), but it is standing state rather than news, so it
            // ranks BELOW the two branches above. Filter mode keeps its own line
            // because that line is being typed into, and a transient `self.status`
            // message ("no viewer for …", the "type: …" typeahead echo, the sort
            // blurb) would otherwise be swallowed for as long as anything stayed
            // marked, which is the whole time the feature is in use. That leaves
            // the static key hint as the branch this replaces, which is the one
            // carrying nothing time-sensitive.
            let dirs = self.marks.marks().iter().filter(|m| m.is_dir).count();
            let line = marks_status(self.marks.len(), dirs, self.marks.bytes());
            format!(" {line}")
        } else if let Some(clip) = &self.clip {
            // A loaded clipboard survives directory changes too, so it must not be
            // invisible state either (ADR 0017 D3's reasoning, same set of keys).
            // It ranks below the mark line because the marks are what the next
            // `y`/`X`/`D` will act on, while the clipboard is answered by one key
            // whose name this line carries anyway.
            format!(" {}", clip_status(clip.cut, clip.paths.len()))
        } else {
            let hidden = if self.show_hidden { "shown" } else { "hidden" };
            // Concise now that `?` opens the full which-key overlay; the dot state
            // stays inline because it's a toggle whose current value matters at a
            // glance. Everything else (sort, layout, meta) lives in the overlay.
            format!(
                " [j/k] move  [Enter] open  [/] filter  [S] search  [.] dot ({hidden})  [?] help"
            )
        };
        // Filter mode borrows the palette's yellow (`doc`, which is exactly the
        // old hardcoded 252,211,77 in sucher-dark) so the mode stays themeable.
        // Written out per mode rather than with a catch-all, so a later mode has
        // to state its colour instead of quietly inheriting `dim`.
        let color = match &self.mode {
            Mode::Filter => theme::palette().doc,
            // Failures borrow the same semantic red the overlay uses, and a report
            // carrying only notes deliberately does not: the red has to mean
            // something went wrong, and on that report nothing did (ADR 0017 D8).
            Mode::Op(OpView::Report(report)) if !report.failures.is_empty() => theme::palette().pdf,
            Mode::Op(OpView::Report(_)) => theme::palette().dim,
            // Named rather than folded into the others, even though the early
            // return above means this arm is never reached: the prompt borrows
            // the filter's yellow because it is the same kind of surface, and
            // stating that here keeps the per-mode rule intact for whoever adds
            // the next mode.
            Mode::Input(_) => theme::palette().doc,
            Mode::Browse | Mode::Search | Mode::Op(OpView::Confirm(_)) => theme::palette().dim,
        };
        f.render_widget(
            Paragraph::new(Line::from(txt)).style(Style::default().fg(color)),
            area,
        );
    }

    /// Render the recursive-search frame (ADR 0007 §10): row 0 an input line, the
    /// middle a horizontal [results | preview] split, row 2 the status. The frame
    /// shape mirrors the browse layout for consistency, and the RIGHT pane is the
    /// browse preview reused verbatim (D5) — it reads `self.preview`/`self.pv`/
    /// `self.pane`, already populated by `build_preview` via `cur_sel`.
    fn render_search(&mut self, f: &mut Frame) {
        let area = f.area();
        let rows = RtLayout::default()
            .constraints([
                Constraint::Length(1), // ⌕ input line
                Constraint::Min(0),    // results | preview
                Constraint::Length(1), // status
            ])
            .split(area);
        self.render_search_input(f, rows[0]);
        // Same 42/58 split as the browse two-column layout, so search reads as the
        // same system: the results list where the listing is, the preview at right.
        let cols = RtLayout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(rows[1]);
        self.render_results(f, cols[0]);
        self.render_preview(f, cols[1]);
        self.render_search_status(f, rows[2]);
    }

    /// The search input line: `⌕ {query}` in the accent colour with a trailing
    /// cursor block, themeable via the palette (ADR 0007 §10). Mirrors the filter's
    /// `/{filter}` line but with search's own glyph and buffer.
    fn render_search_input(&self, f: &mut Frame, area: Rect) {
        let accent = theme::palette().accent;
        let query = self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("");
        let line = Line::from(vec![
            Span::styled(
                format!(" ⌕ {query}"),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            // Trailing cursor block, accent-tinted (palette only — themeable).
            Span::styled("█", Style::default().fg(accent)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    /// The results list: a bordered `List` (accent border, the active surface) whose
    /// rows are drawn SPECIALISED by [`App::search_items`] — relative path + optional
    /// snippet (ADR 0007 D5). Rendered through the search `ListState` so the scroll
    /// offset and selection persist across frames. Records `search_area` +
    /// `viewport_h` for mouse hit-testing and half-page paging (§9/§7).
    fn render_results(&mut self, f: &mut Frame, area: Rect) {
        self.search_area = area;
        self.viewport_h = area.height.saturating_sub(2);
        let accent = theme::palette().accent;
        let n = self.search.as_ref().map(|s| s.results.len()).unwrap_or(0);
        let title = Line::from(Span::styled(
            format!(" {n} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent))
            .title(title);
        f.render_widget(block, area);
        let inner = entry_inner(area);
        // Materialise only the visible window (perf: list virtualisation), sized the
        // same way the browse list is: from the results state's previous offset and
        // selection. Read those before borrowing `&self` for the item build.
        let (offset, len, selected) = match self.search.as_ref() {
            Some(s) => (s.state.offset(), s.results.len(), s.state.selected()),
            None => (0, 0, None),
        };
        let (new_offset, window) = visible_window(offset, selected, len, inner.height as usize);
        // Build items first (borrows `&self`), then render through the state (borrows
        // `&mut self.search`) — sequential, so no aliasing.
        let items = self.search_items(area.width, window.clone());
        // The same soft selection tint + accent cursor gutter the browse list uses
        // (see `entry_list`), so the two surfaces read as one system.
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(theme::palette().selection)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▎ ");
        if let Some(search) = self.search.as_mut() {
            // Render the window through a LOCAL state at offset 0 so ratatui doesn't
            // re-scroll, then write the true first-visible index back into the real
            // results state for `row_to_index`'s click hit-testing (§9).
            let mut local = ListState::default();
            local.select(window_selection(selected, &window));
            render_items_into(f.buffer_mut(), inner, list, &mut local);
            *search.state.offset_mut() = new_offset;
        }
    }

    /// Build the result rows as `ListItem`s (ADR 0007 D5). Each row is drawn
    /// SPECIALISED — the kind glyph (same icon/colour convention as `entry_items`),
    /// the hit's path RELATIVE to cwd (coloured by `hit.kind.color()`), and for a
    /// content match a dimmed ` N: text` snippet. The whole line is length-budgeted
    /// to the inner width so a row never wraps (reusing `truncate`/`snippet_suffix`).
    ///
    /// Only the `window` slice of the results is materialised (perf: list
    /// virtualisation) — the width budget depends only on `width`, so every row that
    /// IS built is identical whatever the window; the caller sizes `window` with
    /// [`visible_window`] and renders it through a local state at offset 0.
    fn search_items(&self, width: u16, window: std::ops::Range<usize>) -> Vec<ListItem<'static>> {
        let Some(search) = self.search.as_ref() else {
            return Vec::new();
        };
        let dim = theme::palette().dim;
        // Width reserved before the path: 2 border + 2 selection-cursor gutter, plus
        // a 2-cell glyph column in the glyphed modes (dropped by `IconMode::None`) —
        // the same chrome arithmetic as `entry_items`.
        let chrome_w = match self.icons {
            IconMode::None => 4,
            _ => 6,
        };
        let inner_w = width.saturating_sub(chrome_w) as usize;
        search.results[window]
            .iter()
            .map(|hit| {
                let mut spans: Vec<Span> = Vec::with_capacity(3);
                // Glyph column + name colour, chosen exactly like `entry_items`.
                let rel_color = match self.icons {
                    IconMode::Unicode => {
                        let c = hit.kind.color();
                        spans.push(Span::styled(
                            format!("{} ", hit.kind.glyph()),
                            Style::default().fg(c),
                        ));
                        c
                    }
                    IconMode::Nerd => {
                        let ext = hit
                            .path
                            .extension()
                            .map(|x| x.to_string_lossy().to_lowercase())
                            .unwrap_or_default();
                        let c = icons::nerd_color(&ext, hit.kind);
                        spans.push(Span::styled(
                            format!("{} ", icons::nerd_glyph(&ext, hit.kind)),
                            Style::default().fg(c),
                        ));
                        c
                    }
                    IconMode::None => hit.kind.color(),
                };
                // The relative path, then the dimmed snippet, each truncated against a
                // shared width budget so the combined row never exceeds `inner_w`.
                let rel_shown = truncate(&hit.rel, inner_w);
                let budget = inner_w.saturating_sub(rel_shown.chars().count());
                spans.push(Span::styled(rel_shown, Style::default().fg(rel_color)));
                let suffix = snippet_suffix(hit.snippet.as_ref());
                if !suffix.is_empty() && budget > 0 {
                    let suffix_shown = truncate(&suffix, budget);
                    spans.push(Span::styled(suffix_shown, Style::default().fg(dim)));
                }
                ListItem::new(Line::from(spans))
            })
            .collect()
    }

    /// The search status line (ADR 0007 §10), reusing `render_status`'s dim look. An
    /// empty query shows a syntax prompt; a live walk shows `searching… N found`; a
    /// finished walk shows `N results` (with ` (capped at 5000)` when capped, or
    /// `no matches` at zero). Always carries the `[Enter] open  [Esc] back` hint.
    fn render_search_status(&self, f: &mut Frame, area: Rect) {
        let dim = theme::palette().dim;
        let txt = match self.search.as_ref() {
            None => String::new(),
            Some(s) if query::parse(&s.query).is_empty() => {
                " type to search  ·  kind: ext: size: content: …    [Esc] back".to_string()
            }
            Some(s) => {
                let n = s.results.len();
                let state = if !s.done {
                    format!("searching… {n} found")
                } else if n == 0 {
                    "no matches".to_string()
                } else if s.capped {
                    format!("{n} results (capped at 5000)")
                } else {
                    format!("{n} results")
                };
                format!(" {state}    [Enter] open  [Esc] back")
            }
        };
        f.render_widget(
            Paragraph::new(Line::from(txt)).style(Style::default().fg(dim)),
            area,
        );
    }
}

/// The smallest frame width (columns) at which Miller opens a third pane. Below
/// this the preview and current panes would be too cramped, so the layout
/// collapses to the classic two-column split (ADR 0004, D1).
const MILLER_MIN: u16 = 100;

/// The duration of a folder-navigation animation (ADR 0006 D3): the colour fade
/// and the directional slide are both started at the same instant with this
/// duration, so they run in lockstep — the incoming listing slides into place
/// while its colours resolve up from the background, and both settle together.
const NAV_ANIM: Duration = Duration::from_millis(150);

/// The assumed terminal background the current-pane fade resolves FROM (ADR 0006
/// D3). A TUI cannot portably query the real background colour, so rather than
/// over-engineer detection we interpolate toward this documented near-black
/// constant; on a terminal whose true background differs the fade origin is
/// approximate, but it lasts only ~150 ms. Because `lerp_color(FADE_BG, c, 1.0)`
/// is exactly `c`, the settle frame is byte-for-byte the non-animated colours.
const FADE_BG: Color = Color::Rgb(16, 16, 20);

/// The classic 10-frame braille spinner cycled in the `Loading` preview while a
/// poster rasters (ADR 0004 D3). Indexed as `SPINNER[spin % SPINNER.len()]`; the
/// `App::spin` counter only advances during live raster work, so this animates
/// exactly when there is real work to show and is otherwise still.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How many completed operations the undo stack keeps (ADR 0017 D8). Bounded
/// because a journal pins the paths it can put back, and an unbounded stack would
/// hold a growing set of them for the life of the process while `U` realistically
/// reaches back one or two operations.
const UNDO_DEPTH: usize = 16;

/// How many screen rows the confirm overlay spends on the vanished-marks block
/// before it collapses into a count (ADR 0017 D3). Small on purpose: the marks
/// that went missing must be visible, but they must not crowd out the plan the
/// user is actually being asked to authorise.
const MISSING_ROWS: usize = 4;

/// A single pane's worth of entries to draw, decoupled from `App` so the SAME
/// renderer ([`render_entry_list`]) serves both the current and the parent pane
/// (ADR 0004, D1). Selection is an index into `order`, not `entries`.
struct EntryListView<'a> {
    /// The backing entries (the current pane's `all`, or the parent's listing).
    entries: &'a [Entry],
    /// Indices into `entries`, in display order: the filtered `view` for the
    /// current pane, every visible sibling for the parent.
    order: &'a [usize],
    /// The highlighted row, as an index into `order` (not `entries`).
    selected: Option<usize>,
    /// The already-formatted block title (` {count} ` for the current pane, the
    /// folder name for the parent).
    title: String,
    /// Optional per-entry git state, keyed by file name (ADR 0004, D2). `Some`
    /// for the current pane in a repo (drawing the gutter); `None` for the parent
    /// context pane and for non-repo / git-disabled dirs (no gutter, no width).
    git: Option<&'a std::collections::HashMap<String, GitStatus>>,
    /// Which trailing metadata column to draw (ADR 0005, D2). The current pane
    /// passes `App.meta` (`Size` or `Modified`); the parent context pane passes
    /// `None`, dropping the column for a cleaner, narrower list and reclaiming
    /// its width. `Size` renders byte-for-byte the pre-feature size column.
    meta: MetaCol,
    /// The eased fade factor for this pane's fade-in after a directory change
    /// (ADR 0006, D3): `Some(t)` (t in 0..1) lerps every entry colour from
    /// [`FADE_BG`] toward its true value; `None` draws the normal colours. Only
    /// the current pane ever sets `Some` (right after a directory change); the
    /// parent context pane — which didn't change — always passes `None`.
    fade_t: Option<f32>,
    /// Optional multi-select state for the mark gutter (ADR 0017 D3). `Some` for
    /// the current pane and ONLY while the set is non-empty, which is what makes
    /// the gutter invisible until used: with nothing marked it reserves no width
    /// and the listing is byte-for-byte the pre-feature render, the same rule
    /// `git` above follows and `MetaCol::None` follows for the trailing column.
    /// The parent pane always passes `None`, because it is navigation context
    /// rather than a surface you act on, which is also why it carries no git
    /// gutter and no metadata column.
    marks: Option<&'a crate::marks::Marks>,
}

/// The mark set an entry pane draws its gutter from, or `None` when there is
/// nothing to draw (ADR 0017 D3). One definition of the "only when non-empty"
/// rule, so the several current-pane construction sites cannot drift apart on it,
/// and a free function over the set rather than a `&self` helper so a caller can
/// build its view from disjoint fields while `&mut self.state` is still needed for
/// the render.
fn mark_gutter(marks: &crate::marks::Marks) -> Option<&crate::marks::Marks> {
    (!marks.is_empty()).then_some(marks)
}

/// The glyph a marked row shows in the mark gutter (ADR 0017 D3). An unmarked row
/// draws two blanks in its place, so names stay column-aligned whatever is
/// selected, and both forms are one cell wide plus a trailing space.
///
/// `IconMode::None` means "no file-type icons" rather than "ASCII terminal", but
/// the browser already reads it as the ASCII-safe mode wherever it decorates
/// something that is not a file: `head_spans` swaps `⎇ ↑ ↓ ●` for `git: + - *`
/// there. The mark gutter follows that convention instead of inventing a second
/// reading of the mode. Pure, so the ASCII guarantee is unit-tested.
fn mark_glyph(icons: IconMode) -> &'static str {
    match icons {
        IconMode::None => "*",
        _ => "◆",
    }
}

/// Total width an entry pane reserves before the name column: 2 cells for the
/// block borders, 2 for the selection cursor gutter that the `highlight_symbol`
/// list reserves on every row, 2 more for the glyph column in the glyphed icon
/// modes (ADR 0003 D5), and 2 each for the git and mark gutters, but only when
/// those are actually drawn. Both optional gutters are invisible until used (ADR
/// 0004 D2, ADR 0017 D3): with neither present a pane reserves exactly what it
/// reserved before either feature existed and the name reclaims the difference.
/// Pure, so that guarantee is unit-tested rather than eyeballed.
fn entry_chrome_w(icons: IconMode, git: bool, marks: bool) -> u16 {
    let base = match icons {
        IconMode::None => 4, // borders + cursor gutter
        _ => 6,              // borders + cursor gutter + glyph cell
    };
    base + if git { 2 } else { 0 } + if marks { 2 } else { 0 }
}

/// Decide the effective column count for a frame: three (Miller) only when the
/// layout asks for it, the frame is wide enough, AND a parent exists; otherwise
/// two. A pure function so the collapse policy is unit-tested without a terminal.
/// `Auto` behaves as Miller here — the width gate is what makes it collapse when
/// narrow, so no separate branch is needed.
fn effective_columns(layout: Layout, width: u16, has_parent: bool) -> u8 {
    let wants_miller = match layout {
        Layout::Miller | Layout::Auto => true,
        Layout::Double => false,
    };
    if wants_miller && width >= MILLER_MIN && has_parent {
        3
    } else {
        2
    }
}

/// The single place browser entries are drawn (ADR 0004, D1): one rounded,
/// bordered list of icon + optional git gutter + name + optional trailing meta
/// column. Both the current pane (`active`, `Size`/`Modified` column,
/// filter-aware border) and the parent context pane (inactive, `MetaCol::None`)
/// route through here.
///
/// A free function, not a method, because the current pane must render through
/// the persistent `App.state` (preserving its scroll offset byte-for-byte) while
/// the parent renders through a throwaway state — passing `state` in lets both
/// share the body without aliasing `self`.
///
/// - `active` lights the border with the accent (and the title accent+BOLD);
///   inactive dims both. `filter` (only ever true for the active current pane)
///   shifts the border to the filter yellow (`doc`), matching the status line.
/// - `view.git` reserves a 2-cell gutter only when `Some` (the current pane in a
///   repo); when `None` (parent pane, non-repo, or git off) it costs zero width
///   and the name reclaims it — so two-column output is byte-for-byte pre-git.
fn render_entry_list(
    f: &mut Frame,
    area: Rect,
    view: &EntryListView,
    state: &mut ListState,
    active: bool,
    filter: bool,
    icons: IconMode,
) {
    // Draw the static border block, then render the items into its inner rect of
    // the frame's own buffer. This is byte-for-byte the pre-slide render: the List
    // widget used to carry the block and render it into `area` before drawing the
    // items into `block.inner(area)`; splitting the block off and rendering the
    // items into that SAME inner rect produces an identical buffer (the List's
    // base `style` is the default, whose `set_style` over the border cells is a
    // no-op), and the `ListState` offset mutation is identical because it depends
    // only on the inner height. Splitting them is what lets the slide keep the
    // border static while the inner content translates (ADR 0006 D3).
    let block = entry_block(view, active, filter);
    f.render_widget(block, area);
    let inner = entry_inner(area);
    // Materialise only the visible window (perf: list virtualisation). We compute
    // the scroll offset ourselves from the state's PREVIOUS offset, so the window
    // is byte-for-byte what ratatui would have shown the full list.
    let (offset, window) = visible_window(
        state.offset(),
        view.selected,
        view.order.len(),
        inner.height as usize,
    );
    let items = entry_items(area, view, icons, window.clone());
    let list = entry_list(items, view.fade_t);
    // Render through a LOCAL state at offset 0 with the selection rebased into the
    // window, so ratatui — which only sees the window's rows — never re-scrolls.
    let mut local = ListState::default();
    local.select(window_selection(view.selected, &window));
    render_items_into(f.buffer_mut(), inner, list, &mut local);
    // Write the TRUE first-visible index back into the real state so `row_to_index`
    // (which reads `state.offset()`) still maps a clicked row to its absolute entry.
    state.select(view.selected);
    *state.offset_mut() = offset;
}

/// Rebase a listing selection (an index into the full `order`) into a virtualised
/// window (perf: list virtualisation): `Some(local)` when the selection falls
/// inside `window` — as it always does once [`visible_window`] has scrolled to
/// keep it visible — else `None`. The local index is what the windowed `ListState`
/// highlights, since that state renders only the window at offset 0.
fn window_selection(selected: Option<usize>, window: &std::ops::Range<usize>) -> Option<usize> {
    selected
        .filter(|s| window.contains(s))
        .map(|s| s - window.start)
}

/// Compose one frame of the current pane's folder slide (ADR 0006 D3). The border
/// block is drawn STATICALLY into the frame; then the OLD snapshot and a freshly
/// rendered NEW inner buffer are blitted into the frame, each translated
/// horizontally by the eased offset and clipped to the inner rect. At progress 1.0
/// the new content lands exactly in `inner` (offset 0) and the old is fully
/// off-screen, so the settle frame is byte-for-byte the normal render — the very
/// reason the loop can clear the slide on completion and let `render_entry_list`
/// draw the final frame.
///
/// Only the current pane is ever slid; the caller passes its `list_area`. The new
/// content carries `view.fade_t`, so it slides in AND resolves its colours up from
/// the background at the same time.
#[allow(clippy::too_many_arguments)]
fn render_entry_slide(
    f: &mut Frame,
    area: Rect,
    view: &EntryListView,
    state: &ListState,
    icons: IconMode,
    active: bool,
    filter: bool,
    slide: &Slide,
    now: Instant,
) {
    let block = entry_block(view, active, filter);
    f.render_widget(block, area);
    let inner = entry_inner(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // Render the NEW inner content (with the colour fade) into a scratch buffer
    // sized to the inner rect. `Buffer::empty(inner)` addresses cells at the
    // inner's GLOBAL coordinates, matching the snapshot and the frame, so the blit
    // is a straight column shift.
    // Same virtualised window as the settled render (perf), computed from the same
    // state offset — so the sliding-in content is byte-for-byte the settle frame.
    let (_, window) = visible_window(
        state.offset(),
        view.selected,
        view.order.len(),
        inner.height as usize,
    );
    let items = entry_items(area, view, icons, window.clone());
    let list = entry_list(items, view.fade_t);
    let mut new_buf = Buffer::empty(inner);
    let mut st = ListState::default(); // local: render the window at offset 0.
    st.select(window_selection(view.selected, &window));
    render_items_into(&mut new_buf, inner, list, &mut st);
    // Eased factor → whole-cell offsets for the two layers.
    let t = crate::anim::ease_out_cubic(slide.anim.progress(now));
    let (old_dx, new_dx) = slide_offsets(slide.dir, t, inner.width);
    blit_shifted(f.buffer_mut(), &slide.old, inner, old_dx);
    blit_shifted(f.buffer_mut(), &new_buf, inner, new_dx);
}

/// The inner content rect of an entry pane: the pane rect minus its 1-cell rounded
/// border on every side. Factored so the normal render, the slide's scratch
/// buffer, and the old-content snapshot all agree on exactly where the items live
/// (`border_type` doesn't affect `inner`, only the border glyphs, so a plain
/// `borders(ALL)` block yields the same rect as the styled [`entry_block`]).
fn entry_inner(area: Rect) -> Rect {
    Block::default().borders(Borders::ALL).inner(area)
}

/// The eased fade tint for a colour (ADR 0006 D3): lerp from the assumed
/// background [`FADE_BG`] toward `c` at factor `t`, or `c` unchanged when `t` is
/// `None` (no fade live, or the parent context pane). Shared by the item spans and
/// the selection highlight so the whole listing resolves together; because
/// `lerp_color(FADE_BG, c, 1.0) == c`, the settle frame is the exact normal colour.
fn fade_color(fade_t: Option<f32>, c: Color) -> Color {
    match fade_t {
        Some(t) => crate::anim::lerp_color(FADE_BG, c, t),
        None => c,
    }
}

/// Build a pane's STATIC border block (ADR 0006 D3): a rounded border plus the
/// styled title. Never fades — during a folder slide the frame stays put while
/// only the inner items translate, so the border/title are drawn once per frame at
/// full colour and the sliding content passes beneath them.
///
/// - `active` lights the border with the accent (and the title accent+BOLD);
///   inactive dims both. `filter` (only ever true for the active current pane)
///   shifts the border to the filter yellow (`doc`), matching the status line.
fn entry_block(view: &EntryListView, active: bool, filter: bool) -> Block<'static> {
    let accent = theme::palette().accent;
    let border = if active {
        if filter {
            theme::palette().doc
        } else {
            accent
        }
    } else {
        theme::palette().dim
    };
    let title_style = if active {
        Style::default().fg(accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::palette().dim)
    };
    let title = Line::from(Span::styled(view.title.clone(), title_style));
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .title(title)
}

/// The scroll offset and the index range of rows actually visible in a list pane
/// (perf: list virtualisation). Replays ratatui's `List` scroll clamp for the
/// uniform 1-cell rows this browser draws — given the PREVIOUS `offset`, the
/// `selected` index, the total `len`, and the viewport `height` — so a caller can
/// build `ListItem`s for ONLY the visible window instead of the whole listing, yet
/// render a buffer byte-for-byte identical to feeding ratatui the full list.
///
/// The returned `offset` is the true first-visible index: scrolled minimally so
/// `selected` stays on screen (up when it's above the window, down when below),
/// then clamped into `[0, len - height]`. Callers WRITE THIS BACK into the real
/// `ListState` so [`row_to_index`] (which reads `state.offset()`) still maps a
/// clicked screen row to the right absolute entry. A zero height or empty list has
/// no visible rows → offset 0 and an empty range.
///
/// Pure so the clamp maths is unit-tested without a terminal.
fn visible_window(
    offset: usize,
    selected: Option<usize>,
    len: usize,
    height: usize,
) -> (usize, std::ops::Range<usize>) {
    if height == 0 || len == 0 {
        return (0, 0..0);
    }
    let max_offset = len.saturating_sub(height);
    let mut offset = offset.min(max_offset);
    if let Some(sel) = selected {
        let sel = sel.min(len - 1);
        if sel < offset {
            offset = sel; // selection above the window → scroll up to it
        } else if sel >= offset + height {
            offset = sel - height + 1; // below the window → scroll down to it
        }
        offset = offset.min(max_offset);
    }
    let end = (offset + height).min(len);
    (offset, offset..end)
}

/// Build a pane's rows as `ListItem`s (ADR 0004 D1): icon + optional git gutter +
/// name + optional trailing meta column, each colour passed through the fade tint
/// from `view.fade_t`. `area` is the OUTER pane rect — its width drives the exact
/// same chrome/name arithmetic as before the block/items split, so the produced
/// items are byte-for-byte identical to the pre-refactor renderer.
///
/// Only the `window` slice of `view.order` is materialised (perf: list
/// virtualisation) — the width arithmetic depends solely on `area.width`, not on
/// how many rows are built, so every row that IS built is identical whatever the
/// window. The caller sizes the window with [`visible_window`] and renders it
/// through a local `ListState` at offset 0, so the on-screen buffer matches a
/// full-list render exactly while off-screen rows cost nothing.
///
/// - `view.git` reserves a 2-cell gutter only when `Some` (the current pane in a
///   repo); when `None` (parent pane, non-repo, or git off) it costs zero width
///   and the name reclaims it — so two-column output is byte-for-byte pre-git.
/// - `view.marks` reserves a second 2-cell gutter under the same rule (ADR 0017
///   D3): `Some` only for the current pane with a non-empty selection, so a
///   browser with nothing marked reserves nothing and looks exactly as it did.
fn entry_items(
    area: Rect,
    view: &EntryListView,
    icons: IconMode,
    window: std::ops::Range<usize>,
) -> Vec<ListItem<'static>> {
    let fade = |c: Color| fade_color(view.fade_t, c);
    // Every optional gutter's width is decided in one place, so the reserved
    // width and the spans built below can never disagree about whether a slot is
    // drawn. Both the git gutter (D2) and the mark gutter (ADR 0017 D3) cost zero
    // width when they are not drawn, and the name reclaims what they do not take.
    let chrome_w = entry_chrome_w(icons, view.git.is_some(), view.marks.is_some());
    let inner_w = area.width.saturating_sub(chrome_w) as usize;
    let size_w = 8usize;
    // The trailing metadata column reserves ` {value:>8}` (9 cells) for both
    // `Size` and `Modified` — they share the width so columns align across a
    // `t` toggle; `None` reserves nothing so the name fills the context column.
    let size_reserve = if view.meta == MetaCol::None {
        0
    } else {
        size_w + 1
    };
    let name_w = inner_w.saturating_sub(size_reserve).max(4);
    // Read the clock once for the whole list — display-only, so a render-time
    // read is fine (ADR 0005 D2); only consulted in `Modified` mode.
    let now = SystemTime::now();

    view.order[window]
        .iter()
        .map(|&i| {
            let e = &view.entries[i];
            let name = truncate(&e.name, name_w);
            // The trailing column's text per mode: byte size (dirs blank),
            // relative modified age (dirs included; missing mtime blank), or —
            // for `None` — nothing (the column isn't drawn at all below).
            let meta_str = match view.meta {
                MetaCol::Size => {
                    if e.kind == Format::Directory {
                        String::new()
                    } else {
                        crate::util::human_size(e.size)
                    }
                }
                MetaCol::Modified => e
                    .modified
                    .map(|m| crate::util::human_age(m, now))
                    .unwrap_or_default(),
                MetaCol::None => String::new(),
            };

            // Icons layer above `Format` and are selected by the mode (D5):
            //   Unicode → the built-in geometric glyph + Format colour (the
            //             default; byte-for-byte the pre-icons rendering).
            //   Nerd    → per-extension Nerd glyph + per-extension tint, with
            //             the SAME tint on the filename so the whole row keys
            //             to language identity.
            //   None    → no glyph column at all; name uses the Format colour.
            let mut spans: Vec<Span> = Vec::with_capacity(5);
            // Mark gutter (ADR 0017 D3), drawn only when `view.marks` is `Some`,
            // which happens only for the current pane with a non-empty set. It
            // sits at the pane's left edge, ahead of the icon, so every mark lines
            // up in one uninterrupted vertical band and a run marked by holding
            // `Space` reads as a run; an unmarked row pays two blanks instead, so
            // the names below it stay column-aligned. The accent is the palette's
            // existing "this is where you are acting" colour, so no new `Palette`
            // field is needed, and it fades with everything else on this row.
            if let Some(marks) = view.marks {
                if marks.contains(&e.path) {
                    spans.push(Span::styled(
                        format!("{} ", mark_glyph(icons)),
                        Style::default().fg(fade(theme::palette().accent)),
                    ));
                } else {
                    spans.push(Span::raw("  "));
                }
            }
            let name_color = match icons {
                IconMode::Unicode => {
                    let c = e.kind.color();
                    spans.push(Span::styled(
                        format!("{} ", e.kind.glyph()),
                        Style::default().fg(fade(c)),
                    ));
                    c
                }
                IconMode::Nerd => {
                    // Same lowercased-extension convention as `classify_path`.
                    let ext = e
                        .path
                        .extension()
                        .map(|x| x.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let c = icons::nerd_color(&ext, e.kind);
                    spans.push(Span::styled(
                        format!("{} ", icons::nerd_glyph(&ext, e.kind)),
                        Style::default().fg(fade(c)),
                    ));
                    c
                }
                IconMode::None => e.kind.color(),
            };
            // Git gutter, between the icon and the name, drawn when `view.git`
            // is present (D2). A clean/absent entry keeps the slot blank so
            // names stay column-aligned.
            if let Some(git) = view.git {
                match git.get(&e.name) {
                    Some(st) => spans.push(Span::styled(
                        format!("{} ", st.glyph()),
                        Style::default().fg(fade(st.color())),
                    )),
                    None => spans.push(Span::raw("  ")),
                }
            }
            spans.push(Span::styled(
                format!("{name:<name_w$}"),
                Style::default().fg(fade(name_color)),
            ));
            if view.meta != MetaCol::None {
                spans.push(Span::styled(
                    format!(" {meta_str:>size_w$}"),
                    Style::default().fg(fade(theme::palette().dim)),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect()
}

/// Wrap items into the `List` widget with the faded selection highlight and the
/// cursor gutter, WITHOUT a block — the block is drawn separately so it can stay
/// static during a slide (ADR 0006 D3). The selection background lerps from
/// [`FADE_BG`] with the rest of the listing and settles on the exact `selection`
/// colour at progress 1.0, replacing the old harsh reverse-video bar with a soft
/// tint plus the accent gutter ("▎").
fn entry_list<'a>(items: Vec<ListItem<'a>>, fade_t: Option<f32>) -> List<'a> {
    List::new(items)
        .highlight_style(
            Style::default()
                .bg(fade_color(fade_t, theme::palette().selection))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▎ ")
}

/// Render a pane's `List` items into an arbitrary buffer at `inner`. The one place
/// the item widget meets a `Buffer`, so the normal render (into the frame's own
/// buffer) and the slide's scratch/snapshot buffers share the identical draw path
/// (ADR 0006 D3). A thin wrapper over `StatefulWidget::render`, which draws into
/// any `&mut Buffer`, not just the frame's.
fn render_items_into(buf: &mut Buffer, inner: Rect, list: List, state: &mut ListState) {
    StatefulWidget::render(list, inner, buf, state);
}

/// Whole-cell horizontal offsets for the two layers of a folder slide at eased
/// factor `t` over an inner pane `w` cells wide (ADR 0006 D3). Returns
/// `(old_dx, new_dx)` — how far to translate the OLD snapshot and the NEW content.
///
/// `FromRight` (entered a child): the old listing slides left (`-round(t*w)`) while
/// the new one enters from the right (`+round((1-t)*w)`). `FromLeft` (went to the
/// parent) mirrors both. The endpoints are the whole point:
/// - `t = 0` → old at `0` (in place), new at `±w` (fully off-screen);
/// - `t = 1` → old at `∓w` (fully off), new at `0` — so the settle frame lands the
///   new content exactly in `inner`, identical to the normal render.
///
/// Pure so the offset maths is unit-tested without a terminal.
fn slide_offsets(dir: SlideDir, t: f32, w: u16) -> (i32, i32) {
    let w = w as f32;
    let shift = (t * w).round() as i32; // how far the outgoing layer has travelled
    let anti = ((1.0 - t) * w).round() as i32; // how far the incoming layer still is
    match dir {
        SlideDir::FromRight => (-shift, anti),
        SlideDir::FromLeft => (shift, -anti),
    }
}

/// Copy every cell of `src` into `dst`, translated horizontally by `dx` and
/// clipped to `inner` (ADR 0006 D3). `src.area` is the inner rect in GLOBAL
/// coordinates, so a source cell at `(x, y)` lands at `(x + dx, y)` iff that
/// column is still inside `inner`; cells shifted past either edge are dropped (the
/// off-screen part of the slide). The frame buffer is reset each draw, so any
/// 1-cell rounding gap between the two layers shows the clean background.
fn blit_shifted(dst: &mut Buffer, src: &Buffer, inner: Rect, dx: i32) {
    let (left, right) = (inner.left() as i32, inner.right() as i32);
    for y in inner.top()..inner.bottom() {
        for x in inner.left()..inner.right() {
            let x2 = x as i32 + dx;
            if x2 < left || x2 >= right {
                continue; // shifted off-screen
            }
            let Some(cell) = src.cell((x, y)) else {
                continue;
            };
            let cell = cell.clone();
            if let Some(d) = dst.cell_mut((x2 as u16, y)) {
                *d = cell;
            }
        }
    }
}

/// Read a directory's entries, classified by extension and ordered by `sort`
/// (directories always first — see [`sort_cmp`]) — the one lister shared by the
/// current pane, the parent pane, and `App::load` (ADR 0004, D1). Pure of app
/// state beyond the passed `sort`; the only IO is the `read_dir`. An unreadable
/// directory yields an empty list rather than erroring, matching the browser's
/// forgiving load.
fn read_entries(dir: &Path, sort: Sort) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let path = ent.path();
            let meta = ent.metadata().ok();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            // Classify by extension only — no per-entry file read, keeping
            // directory loading content-free and fast.
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let kind = crate::format::classify(&ext, is_dir, None);
            entries.push(Entry {
                name,
                path,
                kind,
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified: meta.and_then(|m| m.modified().ok()),
            });
        }
    }
    // Directories first (invariant), then by the requested key — the one place
    // the listing order is decided, shared with in-place re-sorts (`App::resort`)
    // and the search results (`App::pump_search`) via `sort_cmp`.
    entries.sort_by(|a, b| sort_cmp(a, b, sort));
    entries
}

/// A `pct`-sized rectangle centred in `area` (mirrors the markdown viewer's
/// popup geometry). Kept local so the browser owns its overlay layout.
fn centered_rect(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Draw the which-key help overlay: a centred, bordered popup grouping every
/// browser binding under headings, plus a live line echoing the current sort so
/// the overlay doubles as the sort indicator. `Clear` first so the panes behind
/// don't bleed through. Content is a single authored table — the one reference a
/// new user reaches for; the bindings themselves stay sourced from `browse_char`.
fn render_browse_help(f: &mut Frame, area: Rect, sort: Sort) {
    let popup = centered_rect(area, 60, 80);
    f.render_widget(Clear, popup);

    let accent = theme::palette().accent;
    let dim = theme::palette().dim;
    let heading = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
    };
    // `keys` in accent, `desc` dim — the two-tone look the breadcrumb/list use.
    let row = |keys: &str, desc: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<12}"), Style::default().fg(accent)),
            Span::styled(desc.to_string(), Style::default().fg(dim)),
        ])
    };

    let mut lines = vec![
        heading(" Navigate"),
        row("j / k", "down / up  (↑/↓ too)"),
        row("d / u", "half-page down / up"),
        row("g / G", "top / bottom"),
        row("h / l", "parent / open  (←/→, Enter)"),
        row("x", "open in native app  (OS default)"),
        row("type…", "jump to a name (typeahead)"),
        Line::from(""),
        heading(" Select"),
        row("Space", "mark / unmark, then move down"),
        row("V", "invert marks in this view"),
        row("Ctrl-a", "mark everything in this view"),
        // One rung of the ladder per press, in the order `escape` decides them, so
        // the help says what the key does rather than only what it used to do.
        row("Esc", "back out: run → clipboard → marks → quit"),
        Line::from(""),
        heading(" Act"),
        row("y / X", "copy / cut the selection to the clipboard"),
        row("p", "paste here  (shows the plan first; [o] overwrites)"),
        // Both say what the prompt does that the key alone cannot show: `r`
        // opens on the stem so typing keeps the extension, and `a` hides its
        // whole file-or-folder choice in one trailing character (ADR 0017 D4).
        row("r", "rename  (opens on the name, keeps the extension)"),
        row("a", "create  (a trailing / makes a folder)"),
        // Named as trash, not delete: sucher has no permanent-delete binding at
        // all, and the help is where that promise has to be legible (ADR 0017 D7).
        row(
            "D",
            "move to trash  (shows the plan first; never permanent)",
        ),
        // What `U` can and cannot reach, because the one operation it does not put
        // back is the one users will try it on first (ADR 0017 D8).
        row(
            "U",
            "undo the last operation  (a trashing is Finder's to undo)",
        ),
        // The delivery caveat is the description, not a footnote to it: OSC 52 is
        // fire-and-forget, so the honest thing the help can say is who decides.
        row("Y", "yank absolute path(s)  (OSC 52; the terminal decides)"),
        Line::from(""),
        heading(" Find"),
        row("/", "filter this folder  (kind: ext: size: modified:)"),
        row("S", "recursive search  (also content:)"),
        Line::from(""),
        heading(" Sort"),
        row("o", "cycle key: name → size → modified → ext"),
        row("O", "reverse direction"),
        Line::from(""),
        heading(" Display"),
        row(".", "toggle hidden files"),
        row("t", "toggle size / modified column"),
        row("M", "cycle layout: auto → miller → double"),
        Line::from(""),
        heading(" Other"),
        // Named apart from `Esc` now that the two differ: `q` is refused outright
        // while a run is live, rather than quietly cancelling it on the way out.
        row("q", "quit  (refused while an operation runs)"),
        row("?", "close this help"),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", sort.label()),
            Style::default().fg(dim).add_modifier(Modifier::ITALIC),
        )),
    ];
    // Trim to the popup's inner height so a short terminal never panics/clips oddly.
    let inner_h = popup.height.saturating_sub(2) as usize;
    lines.truncate(inner_h);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Line::from(Span::styled(
            " Keys — any key to close ",
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

/// Draw a file-operation overlay: the plan awaiting authorisation, or the
/// failures a finished run left behind (ADR 0017 D5).
///
/// Built on the same `centered_rect` + `Clear` geometry and the same two-tone
/// accent/dim style as [`render_browse_help`], so the two popups read as one
/// system. The keys that answer it live in the title, which buys back a row for
/// the content and matches the help overlay's " any key to close ".
fn render_op_overlay(f: &mut Frame, area: Rect, view: &OpView) {
    let popup = centered_rect(area, 60, 60);
    f.render_widget(Clear, popup);
    let accent = theme::palette().accent;
    // The inner box, minus the border on each side.
    let w = popup.width.saturating_sub(2) as usize;
    let h = popup.height.saturating_sub(2) as usize;

    let (title, lines) = match view {
        OpView::Confirm(pending) => (
            format!(
                " {} · {} ",
                kind_title(pending.plan.kind),
                confirm_keys(pending.inputs.is_some(), false)
            ),
            confirm_lines(&pending.plan, w, h),
        ),
        OpView::Report(report) => (
            format!(" {} · [any key] close ", report_title(report)),
            report_lines(report, w, h),
        ),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(accent))
        .title(Line::from(Span::styled(
            title,
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), popup);
}

/// The confirm overlay's body: what the plan will do, before it does any of it
/// (ADR 0017 D5).
///
/// The step list is built LAST and sized to whatever rows are actually left, and
/// that ordering is what makes the "showing 8 of 41" promise hold. Because
/// [`fit_rows`] spends one of those rows on the count, the body lands on exactly
/// `h` lines when it overflows, so the final `truncate` can only ever fire on a
/// terminal too short for the header alone, where no step rows are drawn either.
/// The overlay can therefore never look like the whole plan when it is not.
fn confirm_lines(plan: &fileop::Plan, w: usize, h: usize) -> Vec<Line<'static>> {
    let accent = theme::palette().accent;
    let dim = theme::palette().dim;
    // ADR 0017 asks for a danger colour and `Palette` is not grown for it: `pdf`
    // is already this codebase's semantic red (`git.rs` renders a deleted path in
    // it), so the facts that deserve alarm borrow it.
    let danger = theme::palette().pdf;
    let styled = |text: String, color: Color| {
        Line::from(Span::styled(truncate(&text, w), Style::default().fg(color)))
    };

    let mut lines = vec![Line::from(Span::styled(
        truncate(&format!("  {}", plan.summary()), w),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    ))];
    // Trash is the one operation with nowhere to name (D7); every other one says
    // where the batch is going before it says what is in it.
    if !plan.dest.as_os_str().is_empty() {
        lines.push(styled(format!("  into {}", plan.dest.display()), dim));
    }
    // What the conflict policy is about to do. Displacing something is the one
    // outcome here that costs an existing file its place, so it is the one drawn
    // in the danger colour; the split lives in [`collision_lines`] so the rule is
    // unit-tested rather than eyeballed against a theme.
    for (text, alarming) in collision_lines(plan.policy, plan.renamed(), plan.overwrites()) {
        lines.push(styled(
            format!("  {text}"),
            if alarming { danger } else { dim },
        ));
    }
    // Marks that had already vanished are shown, never silently dropped (D3).
    if !plan.missing.is_empty() {
        lines.push(Line::from(""));
        lines.push(styled(
            format!("  {} already gone:", mark_count(plan.missing.len())),
            danger,
        ));
        let (shown, hidden) = fit_rows(plan.missing.len(), MISSING_ROWS);
        for path in plan.missing.iter().take(shown) {
            lines.push(styled(format!("    {}", path.display()), dim));
        }
        if hidden > 0 {
            lines.push(styled(format!("    … and {hidden} more"), dim));
        }
    }
    lines.push(Line::from(""));

    let (shown, hidden) = fit_rows(plan.steps.len(), h.saturating_sub(lines.len()));
    for step in plan.steps.iter().take(shown) {
        lines.push(styled(format!("  {}", step_line(step, plan.kind)), accent));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            truncate(&format!("  … and {hidden} more"), w),
            Style::default().fg(dim).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.truncate(h);
    lines
}

/// Whether a finished run has anything to say beyond its one-line totals, and so
/// whether the report overlay opens at all (ADR 0009, ADR 0017 D8).
///
/// Failures are the obvious half. Notes are the half that is easy to lose: an undo
/// that reversed everything it could still has to tell the user which paths only
/// the system trash can restore, and a run whose failure list happens to be empty
/// would otherwise swallow that note entirely. Pure, so the condition is asserted
/// rather than trusted to a negation buried in `finish_op`.
fn report_speaks(report: &fileop::Report) -> bool {
    !report.failures.is_empty() || !report.notes.is_empty()
}

/// The report overlay's rows: every failure, then every note, each flagged as
/// alarming or not.
///
/// The flag rather than a [`Color`] for the same reason [`collision_lines`] uses
/// one: the split between what went wrong and what is merely worth knowing is a
/// rule about the content, so it is decided here and unit-tested, and only the
/// renderer knows which two colours the theme spends on it.
///
/// Failures come first, and that ordering is the whole reason the executor keeps
/// the two lists apart (ADR 0017 D8). A user scanning a red list for what broke
/// must not have to read past advice to find it, and when the popup is too short
/// for everything it is the advice that gets counted away rather than a failure.
fn report_rows(report: &fileop::Report) -> Vec<(String, bool)> {
    report
        .failures
        .iter()
        .map(|failure| (failure.msg.clone(), true))
        .chain(report.notes.iter().map(|note| (note.clone(), false)))
        .collect()
}

/// The report overlay's body: what the run managed, then what it did not and what
/// it wants the user to know. Sized by the same [`fit_rows`] rule as the confirm
/// overlay, so a long list says how many it is not showing rather than ending
/// without warning.
fn report_lines(report: &fileop::Report, w: usize, h: usize) -> Vec<Line<'static>> {
    let accent = theme::palette().accent;
    let dim = theme::palette().dim;
    let danger = theme::palette().pdf;
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(
                &format!(
                    "  {}",
                    op_done_status(report.kind, report.direction, report.items, report.bytes)
                ),
                w,
            ),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let rows = report_rows(report);
    let (shown, hidden) = fit_rows(rows.len(), h.saturating_sub(lines.len()));
    for (text, alarming) in rows.iter().take(shown) {
        lines.push(Line::from(Span::styled(
            truncate(&format!("  {text}"), w),
            // A note takes the ordinary informational colour of a hint, never the
            // danger colour: the red list has to mean "this went wrong" and nothing
            // else, or it stops meaning anything.
            Style::default().fg(if *alarming { danger } else { dim }),
        )));
    }
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            truncate(&format!("  … and {hidden} more"), w),
            Style::default().fg(dim).add_modifier(Modifier::ITALIC),
        )));
    }
    lines.truncate(h);
    lines
}

/// How many of `total` rows to draw in `room` screen rows, and how many are left
/// over. A list that does not fit spends one of its own rows saying how many it
/// is not showing, so a truncated list can never be mistaken for a complete one
/// (ADR 0017 D5). `room == 0` therefore shows nothing and reports everything as
/// hidden, rather than showing a first row with no way to say more follow. Pure,
/// so the arithmetic is unit-tested rather than eyeballed on a short terminal.
fn fit_rows(total: usize, room: usize) -> (usize, usize) {
    if total <= room {
        return (total, 0);
    }
    let shown = room.saturating_sub(1);
    (shown, total - shown)
}

/// The final component of a path for display, or the whole path when it has none
/// (the filesystem root). Used by the overlay and the progress line, which have a
/// popup's width to work with and not a screen's.
fn leaf_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// One step as the confirm overlay lists it: what is acted on, and what it
/// becomes. Pure, so the collision-suffix rendering the user authorises is
/// unit-tested rather than eyeballed.
fn step_line(step: &fileop::Step, kind: fileop::Kind) -> String {
    let src = leaf_name(&step.src);
    let dest = leaf_name(&step.dest);
    match kind {
        // Trash has no destination path to name (ADR 0017 D7), so the line says
        // where the entry is going in words rather than inventing a path for it.
        fileop::Kind::Trash => format!("{src}  →  trash"),
        // A create has no source; the name being made is the whole of the step.
        fileop::Kind::Create => dest,
        fileop::Kind::Copy | fileop::Kind::Move | fileop::Kind::Rename => {
            if src == dest {
                // The common case: nothing collided, so naming the same string
                // twice would only be noise.
                src
            } else {
                format!("{src}  →  {dest}")
            }
        }
    }
}

/// The overlay's title for an operation. Capitalised because it heads a popup,
/// unlike the lowercase verb `Plan::summary` uses mid-sentence.
fn kind_title(kind: fileop::Kind) -> &'static str {
    match kind {
        fileop::Kind::Copy => "Copy",
        fileop::Kind::Move => "Move",
        fileop::Kind::Rename => "Rename",
        fileop::Kind::Create => "Create",
        // Named in full, so the popup states the promise of ADR 0017 D7 rather
        // than saying "Delete" and leaving the user to hope.
        fileop::Kind::Trash => "Move to trash",
    }
}

/// The past-tense verb for a finished forward operation.
fn done_verb(kind: fileop::Kind) -> &'static str {
    match kind {
        fileop::Kind::Copy => "copied",
        fileop::Kind::Move => "moved",
        fileop::Kind::Rename => "renamed",
        fileop::Kind::Create => "created",
        fileop::Kind::Trash => "trashed",
    }
}

/// The name of the operation an undo reversed, as a noun.
///
/// A noun rather than [`done_verb`]'s past tense because an undo's sentence is
/// about the operation and not about the undo: "undid the move" names the thing
/// taken back, where "undid moved" would name nothing at all. `Kind` deliberately
/// has no undo arm of its own (see [`fileop::Direction`]), so this is where the
/// browser composes the two halves the engine hands it.
fn undone_noun(kind: fileop::Kind) -> &'static str {
    match kind {
        fileop::Kind::Copy => "copy",
        fileop::Kind::Move => "move",
        fileop::Kind::Rename => "rename",
        fileop::Kind::Create => "creation",
        fileop::Kind::Trash => "trashing",
    }
}

/// The status line a finished run leaves behind, e.g.
/// `trashed 3 items, 1.2K · restore from the system trash`, or
/// `undid the move of 3 items` when the run travelled the other way.
///
/// The direction is not decoration. `Kind` alone says which operation the report
/// is about, and an undo's report carries the kind of the operation it REVERSED
/// (ADR 0017 D8), so composing the sentence from the kind by itself would have the
/// status line announce a move at the exact moment one was taken back. Both halves
/// therefore go into the wording, which is the reason [`fileop::Direction`] travels
/// beside the kind rather than being folded into it.
///
/// The trash hint rides on the forward direction only. On the way out it is the
/// one thing the user needs (sucher does not restore from the trash in process, so
/// the line points at Finder rather than implying `U` will do it), and on the way
/// back the undo's own note says the same thing about named paths, far better than
/// a generic clause could.
///
/// Sizes go through [`crate::util::human_size`], the formatter the listing's size
/// column uses, so the two never disagree about what a megabyte looks like. Pure,
/// so the wording is unit-tested.
fn op_done_status(
    kind: fileop::Kind,
    direction: fileop::Direction,
    items: usize,
    bytes: u64,
) -> String {
    let noun = if items == 1 { "item" } else { "items" };
    let mut out = match direction {
        fileop::Direction::Forward => format!("{} {items} {noun}", done_verb(kind)),
        fileop::Direction::Undo => format!("undid the {} of {items} {noun}", undone_noun(kind)),
    };
    if bytes > 0 {
        out.push_str(&format!(", {}", crate::util::human_size(bytes)));
    }
    if kind == fileop::Kind::Trash && direction == fileop::Direction::Forward {
        // ADR 0017 D8: sucher does not restore from the trash in process, so the
        // line points at the surface that does instead of implying `U` will. A
        // half-supported undo is worse than an honest pointer to Finder.
        out.push_str(" · restore from the system trash");
    }
    out
}

/// The report overlay's title: which run this is, and how much it has to say.
///
/// It names the direction for the same reason [`op_done_status`] does, since a
/// popup headed `Move` over a body that says an undo happened would contradict
/// itself in two adjacent rows. And it counts notes when there are no failures,
/// because an undo arrives here with an empty failure list and `0 steps failed`
/// would be a strange thing to head a popup that reports nothing having failed.
/// Pure, so both readings are unit-tested.
fn report_title(report: &fileop::Report) -> String {
    let what = match report.direction {
        fileop::Direction::Forward => kind_title(report.kind).to_string(),
        fileop::Direction::Undo => format!("Undo {}", undone_noun(report.kind)),
    };
    format!("{what} · {}", report_count(report))
}

/// How much a report has to say, in the one phrase both the overlay title and the
/// status line under it use, so the two can never disagree about the same popup.
///
/// Failures are counted whenever there are any, because they are what the user
/// most needs the size of. With none, the notes are counted instead: an undo
/// reports here with an empty failure list, and heading its popup `0 steps failed`
/// would describe the absence of the thing that did not happen.
fn report_count(report: &fileop::Report) -> String {
    if report.failures.is_empty() {
        note_count(report.notes.len())
    } else {
        fail_count(report.failures.len())
    }
}

/// The status line while an undo runs: `undo the move · 2/3 · notes.md`.
///
/// A forward run takes this label from its plan's own summary, so the line names
/// the operation the user authorised word for word. An undo has no plan to quote,
/// so it names the operation it is reversing instead, which is the nearest thing
/// to the same promise.
fn undo_label(kind: fileop::Kind) -> String {
    format!("undo the {}", undone_noun(kind))
}

/// Where the cursor lands after an undo, when the journal names exactly one thing
/// to land on.
///
/// The mirror of [`landing_name`], and it exists for the reason that one does: an
/// undone rename abolishes the name the cursor is sitting on, so asking for the
/// old selection would leave the cursor beside the entry the user just restored
/// rather than on it. Only a restored path can be landed on. A journal step that
/// undo REMOVES (a copy or a create being taken away) leaves nothing to select, and
/// a trashed path is not restored in process at all (ADR 0017 D8), so both yield
/// `None` and the caller falls back to keeping the row. Pure, so every arm is
/// unit-tested.
fn undo_landing(steps: &[fileop::Undoable]) -> Option<String> {
    match steps {
        [fileop::Undoable::Moved { from, .. }] => {
            from.file_name().map(|n| n.to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// The status line while an operation runs, e.g. `trash 3 items · 2/3 · notes.md`.
///
/// The label is the plan's own summary, captured at authorisation time, so the
/// line the user is watching names the operation they authorised word for word.
/// Pure, so the format is unit-tested.
fn op_progress_status(label: &str, items: usize, total: usize, current: &Path) -> String {
    let mut out = format!("{label} · {items}/{total}");
    let name = leaf_name(current);
    // Empty before the first progress message arrives, and an empty tail would
    // just leave a dangling separator.
    if !name.is_empty() {
        out.push_str(&format!(" · {name}"));
    }
    out
}

/// `1 step` / `3 steps`, for the failure overlay's title and status line.
fn fail_count(n: usize) -> String {
    let noun = if n == 1 { "step" } else { "steps" };
    format!("{n} {noun} failed")
}

/// `1 mark` / `3 marks`, for the vanished-marks block.
fn mark_count(n: usize) -> String {
    let noun = if n == 1 { "mark" } else { "marks" };
    format!("{n} {noun}")
}

/// `1 note` / `3 notes`, for the report overlay's title when nothing failed.
fn note_count(n: usize) -> String {
    let noun = if n == 1 { "note" } else { "notes" };
    format!("{n} {noun}")
}

/// The text `Y` hands the terminal: one absolute path per line (ADR 0017 D4).
///
/// Newline-separated because that is what every shell, editor and file dialog on
/// the other end of a paste already treats as a list of paths, so a multi-mark
/// yank arrives as several arguments rather than as one impossible filename.
///
/// The browser's paths are absolute in practice, since `run` canonicalises the
/// starting directory and every entry is a child of it, but `base` closes the one
/// door left open (a start path that could not be canonicalised) without asking
/// the filesystem. Resolving them with `canonicalize` would be worse than doing
/// nothing: it follows symlinks, so yanking a link would paste its target, which
/// is not the path the user is pointing at. Pure, so the joining is unit-tested.
fn clipboard_text(base: &Path, paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| {
            if p.is_absolute() {
                p.display().to_string()
            } else {
                base.join(p).display().to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What `Y` says once the sequence is out of the door (ADR 0017 D4).
///
/// It reports **what sucher did**, not what the clipboard now holds, and the
/// distinction is the whole point. OSC 52 has no reply: the terminal is free to
/// ignore it, many do by default, and tmux needs `set -g set-clipboard on` before
/// it will pass one on. `copy_to_clipboard`'s `Ok` therefore means the bytes were
/// written and flushed and nothing more, so a message reading "copied to
/// clipboard" would be a claim sucher is in no position to make: the user would
/// paste, get their old clipboard back, and have been told otherwise. Naming the
/// mechanism and where the decision actually sits is both honest and the fastest
/// route to the tmux setting when nothing arrives. Pure, so the wording is
/// unit-tested.
fn yank_status(n: usize) -> String {
    let noun = if n == 1 { "path" } else { "paths" };
    format!("sent {n} {noun} to the terminal via OSC 52 · the terminal decides whether the clipboard takes it")
}

/// The paths an operation acts on: the mark set when it is non-empty, otherwise
/// the entry under the cursor (ADR 0017 D3).
///
/// Every operation shares this rule, so it lives here once rather than being
/// retyped by each binding as copy, move and rename land. Marks are handed over
/// in mark order, which is the order the planner allocates collision suffixes in,
/// so the same selection always produces the same plan. Pure, so the rule is
/// unit-tested without a browser.
fn targets(marks: &crate::marks::Marks, cursor: Option<&Path>) -> Vec<PathBuf> {
    if !marks.is_empty() {
        return marks.marks().iter().map(|m| m.path.clone()).collect();
    }
    // No marks and nothing under the cursor is an empty batch, which the planner
    // refuses by name rather than this function guessing at one.
    cursor.map(Path::to_path_buf).into_iter().collect()
}

/// The UNFILTERED names in `dir`, as the planner must see them (ADR 0017 D5).
///
/// One `read_dir`, every name, no filtering, read fresh at plan time. Neither of
/// the listings the browser already holds will do: `view` hides dotfiles unless
/// `.` is toggled, and `all` is a snapshot from the last `load` that a `.env`
/// written a second ago is not in. Planning against either would let a paste land
/// on top of a file the user cannot see, which is precisely the hole D5 exists to
/// close.
///
/// A directory that cannot be read yields an empty listing, which is the safe
/// reading rather than a convenient one: every name then looks free, so nothing
/// is planned as an overwrite, and the executor re-checks each destination
/// against the real filesystem before it writes (D5). The outcome is a step that
/// fails honestly, never a silent clobber.
fn dest_listing(dir: &Path) -> Vec<String> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Where the cursor starts in a rename prompt, counted in CHARACTERS: at the end
/// of the name's stem, so the first key typed replaces the name and keeps the
/// extension (ADR 0017 D4).
///
/// The split is the same "last extension only" rule the planner's collision
/// suffixing uses (`foo.tar.gz` becomes `foo.tar (2).gz`), and deliberately so:
/// the two are the same question asked from opposite ends, and a browser that
/// protected `.tar.gz` here while the planner protected `.gz` there would teach
/// the user two contradictory ideas of what an extension is.
///
/// Three names have no split to make. A directory has no extension to protect,
/// so `v1.2` is all stem rather than being cut into `v1` and `2`. A dotfile like
/// `.gitignore` is all stem too, because the leading dot is not a separator. And
/// a trailing dot introduces nothing, so `odd.` keeps the whole name.
///
/// Counted in characters rather than bytes because that is what
/// [`crate::lineedit::LineEdit`] takes, and for the reason it takes it: `café.txt`
/// has a four-character stem and a five-byte one, and a byte index would put the
/// cursor inside the `é`.
fn stem_end(name: &str, is_dir: bool) -> usize {
    if !is_dir {
        if let Some(dot) = name.rfind('.') {
            if dot > 0 && dot + 1 < name.len() {
                return name[..dot].chars().count();
            }
        }
    }
    name.chars().count()
}

/// The name the cursor should land on once a finished operation has reloaded the
/// listing, or `None` when the operation has no single answer to give.
///
/// One step with a real destination created or renamed exactly one entry, and
/// that entry is what the user was looking at when they authorised it: a rename
/// lands on the new name, a create on the thing just created, a one-file paste on
/// the copy. Anything else declines rather than guesses. A multi-step paste
/// brought several entries in at once and choosing among them would be choosing
/// for the user, and a trash step has no destination at all (ADR 0017 D7), so the
/// browser keeps the rule it already had: hold the name that was under the
/// cursor, and failing that the row.
///
/// Kept a pure function of the resolved steps rather than a special case threaded
/// through `finish_op`, so the decision is unit-tested without a filesystem and
/// `finish_op` stays one rule with one fallback.
fn landing_name(steps: &[fileop::Step]) -> Option<String> {
    match steps {
        [only] => only
            .dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned()),
        _ => None,
    }
}

/// The status-line spans for the inline prompt (ADR 0017 D4).
///
/// The same shape and the same colour as the filter's `/…` line, so the two read
/// as one system, plus the one thing the filter has no need of: a visible cursor.
/// A field that opens pre-filled with the cursor parked in the middle of a name
/// is only useful if the user can see where it is, and an invisible cursor in a
/// field that has one is worse than no cursor at all.
///
/// The character under the cursor is drawn reversed, and the end-of-line position
/// gets a block, because there is no character there to reverse: that is the case
/// a fresh create prompt is always in, so an empty span would leave the cursor
/// invisible exactly where it matters most. The block glyph is the one
/// `render_search_input` already uses for the same job.
///
/// Pure, so the whole line is unit-tested without a terminal.
fn prompt_spans(prefix: &str, edit: &crate::lineedit::LineEdit, hint: &str) -> Vec<Span<'static>> {
    // The filter's yellow, taken from the palette rather than hardcoded, so the
    // prompt stays themeable along with every other surface.
    let doc = theme::palette().doc;
    let (head, under, tail) = edit.split();
    vec![
        Span::styled(format!(" {prefix}{head}"), Style::default().fg(doc)),
        if under.is_empty() {
            Span::styled("█", Style::default().fg(doc))
        } else {
            Span::styled(
                under.to_string(),
                Style::default().fg(doc).add_modifier(Modifier::REVERSED),
            )
        },
        Span::styled(tail.to_string(), Style::default().fg(doc)),
        Span::styled(format!("    {hint}"), Style::default().fg(doc)),
    ]
}

/// Resolve a rename of `path` to `new_name` (ADR 0017 D4/D5).
///
/// The destination listing is the UNFILTERED contents of the source's own parent,
/// read fresh from the filesystem, for the reason spelled out at the
/// [`dest_listing`] definition and at the `request_trash` construction site:
/// planning against `self.view` would hide dotfiles and let a rename land on top
/// of a `.env` the user cannot see.
fn rename_plan(path: &Path, new_name: &str, cwd: &Path) -> Result<fileop::Plan, fileop::Refusal> {
    let Some(parent) = path.parent() else {
        return Err(fileop::Refusal::FilesystemRoot);
    };
    let source = rename_source(path)?;
    let listing = dest_listing(parent);
    fileop::plan(
        fileop::Op::Rename {
            source,
            new_name: new_name.to_string(),
        },
        &fileop::PlanCtx {
            dest_listing: &listing,
            cwd,
            // A rename acts on one path the browser is looking at right now, so
            // there is no earlier selection that could have gone stale in the
            // meantime and nothing to report as missing (ADR 0017 D3). A path
            // that vanished shows up as an `Io` refusal from `rename_source`
            // instead, which is the more precise answer.
            missing: &[],
            // A rename onto an existing name is refused outright rather than
            // suffixed, so the policy never comes into play; the default is
            // passed because there is no third value to mean "not applicable".
            policy: fileop::Conflict::Rename,
        },
    )
}

/// Stat one path into the single-entry [`fileop::Source`] a rename needs.
///
/// This deliberately does NOT go through [`fileop::collect`], which every other
/// operation uses, and the difference is the point rather than an oversight. A
/// rename is one `fs::rename` on one inode: nothing below the path is read,
/// written or even opened, so enumerating the subtree would buy nothing and cost
/// three real things.
///
///   * `collect` refuses a walk past `MAX_TREE_ITEMS`, which would turn renaming
///     a `node_modules` into an error about a limit that has nothing to do with
///     what was asked. That bound exists to stop a half-finished COPY (ADR 0017
///     D2); a rename has no partial state to protect against.
///   * The plan's totals feed the status line, and `rename 40182 items, 1.2G`
///     would be a plain lie about an operation that moves no bytes at all.
///     One item and zero bytes is what actually happens.
///   * On a network mount the walk is the entire cost of the operation, paid
///     before a prompt the user might still cancel.
///
/// The one filesystem question a rename does have to ask is whether the path is
/// still there, which is exactly what this stat answers. `symlink_metadata` and
/// not `metadata`, so a symlink is seen as a link rather than as whatever it
/// points at, the same rule `collect` follows for the same reason (ADR 0017 D5).
fn rename_source(path: &Path) -> Result<fileop::Source, fileop::Refusal> {
    let meta = fs::symlink_metadata(path).map_err(|e| fileop::Refusal::Io {
        path: path.to_path_buf(),
        msg: e.to_string(),
    })?;
    let ft = meta.file_type();
    let kind = if ft.is_symlink() {
        fileop::NodeKind::Symlink
    } else if ft.is_dir() {
        fileop::NodeKind::Dir
    } else {
        fileop::NodeKind::File
    };
    Ok(fileop::Source {
        path: path.to_path_buf(),
        kind,
        // A rename relocates the entry itself and never descends, so there is no
        // subtree for the executor to replay.
        nodes: Vec::new(),
        items: 1,
        bytes: 0,
    })
}

/// Resolve a create of `name` in `parent` (ADR 0017 D4/D5). A trailing `/` on the
/// name asks for a directory, which the planner decides.
///
/// Same unfiltered, freshly read destination listing as [`rename_plan`], and for
/// the same reason: creating `notes.md` beside an invisible `notes.md` has to be
/// the refusal the user can act on, not a silent collision.
fn create_plan(parent: &Path, name: &str, cwd: &Path) -> Result<fileop::Plan, fileop::Refusal> {
    let listing = dest_listing(parent);
    fileop::plan(
        fileop::Op::Create {
            parent: parent.to_path_buf(),
            name: name.to_string(),
        },
        &fileop::PlanCtx {
            dest_listing: &listing,
            cwd,
            // A create has no sources at all, so nothing can have vanished.
            missing: &[],
            // Create onto an existing name is refused rather than suffixed, so
            // as with a rename the policy never applies.
            policy: fileop::Conflict::Rename,
        },
    )
}

/// The other conflict policy. `o` toggles rather than switches one way, so a user
/// who pressed it to see what overwriting would cost gets the suffixing default
/// back with the same key (ADR 0017 D5).
fn flip_policy(policy: fileop::Conflict) -> fileop::Conflict {
    match policy {
        fileop::Conflict::Rename => fileop::Conflict::Overwrite,
        fileop::Conflict::Overwrite => fileop::Conflict::Rename,
    }
}

/// The keys that answer the confirm overlay.
///
/// The overwrite toggle is named only where it can do something: trash has no
/// destination and so no collision to resolve, and advertising a key that would
/// then sit inert is worse than not naming it at all. `alternates` spells the
/// single-letter equivalents, which the status line has the width for and the
/// popup title does not.
fn confirm_keys(toggleable: bool, alternates: bool) -> &'static str {
    match (toggleable, alternates) {
        (true, true) => "[Enter]/[y] run  [o] overwrite  [Esc]/[n] cancel",
        (true, false) => "[Enter] run  [o] overwrite  [Esc] cancel",
        (false, true) => "[Enter]/[y] run  [Esc]/[n] cancel",
        (false, false) => "[Enter] run  [Esc] cancel",
    }
}

/// The confirm overlay's lines about what the conflict policy will do, each with
/// whether it is alarming enough for the danger colour (ADR 0017 D5).
///
/// Under the default suffixing policy this is at most one calm line, and nothing
/// at all when nothing collided. Under `Overwrite` there is ALWAYS a line, even
/// when the count is zero: `o` toggles a policy rather than a count, so a press
/// that changed the policy without changing any number still has to show that it
/// landed, or the key would look broken. Only the case that actually displaces
/// something is alarming, and it says where the displaced entry goes, because
/// ADR 0017 D7 makes that a promise rather than an implementation detail.
fn collision_lines(
    policy: fileop::Conflict,
    renamed: usize,
    overwrites: usize,
) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if renamed > 0 {
        out.push((format!("{renamed} suffixed to avoid a collision"), false));
    }
    if policy == fileop::Conflict::Overwrite {
        let noun = if overwrites == 1 { "entry" } else { "entries" };
        out.push(if overwrites == 0 {
            (
                "overwrite is on, but nothing here collides".to_string(),
                false,
            )
        } else {
            (
                format!("{overwrites} existing {noun} replaced, each trashed first"),
                true,
            )
        });
    }
    out
}

/// What the status line says about a loaded clipboard, and the key that answers
/// it (ADR 0017 D4). Pure, so the wording is unit-tested.
fn clip_status(cut: bool, n: usize) -> String {
    let noun = if n == 1 { "item" } else { "items" };
    let verb = if cut { "move" } else { "copy" };
    format!("clipboard: {n} {noun} to {verb} · [p] paste here")
}

/// Whether the clipboard outlives a finished operation (ADR 0017 D4).
///
/// A cut that has run pasted its sources away, so a second paste of the same
/// clipboard would only collect a list of paths that are no longer there: it
/// goes. So does a cut that was cancelled part way, whose clipboard would now be
/// half stale, since a selection that is only partly real is a worse thing to
/// hand back than none. A copy left every source where it was, and pasting the same
/// files into three folders in turn is a real workflow, so it stays. Rename,
/// create and trash never consumed the clipboard in the first place and have no
/// business emptying it. Keyed on the operation rather than on a flag set at
/// paste time because move and copy are only ever reached through `p`.
fn clip_survives(kind: fileop::Kind) -> bool {
    match kind {
        fileop::Kind::Move => false,
        fileop::Kind::Copy | fileop::Kind::Rename | fileop::Kind::Create | fileop::Kind::Trash => {
            true
        }
    }
}

/// Which layer one press of `Esc` backs out of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Escape {
    /// An operation is running: cancel it and let it report what it finished.
    CancelOp,
    /// Nothing is running but the clipboard is loaded: drop it.
    ClearClip,
    /// Nothing is running and the clipboard is empty, but marks are held: clear
    /// them (ADR 0017 D4).
    ClearMarks,
    /// Nothing is held at all, so `Esc` means what it always meant.
    Quit,
}

/// The `Esc` precedence ladder: one press backs out exactly one layer, outermost
/// first (ADR 0017 D4, extended).
///
/// The order is the whole of the decision, so it is a pure function rather than a
/// chain of conditions inside the key handler, and every rung is unit-tested. The
/// running operation is outermost because it is the only layer that is actively
/// changing the disk, and because `Run`'s own `Drop` would otherwise cancel it
/// silently on the way out of the browser, leaving a half-copied tree and no
/// report: exactly the silent partial ADR 0009 forbids. The clipboard comes
/// before the marks because it was loaded later, so backing out retraces the
/// user's own steps.
fn escape(op_running: bool, has_clip: bool, has_marks: bool) -> Escape {
    if op_running {
        Escape::CancelOp
    } else if has_clip {
        Escape::ClearClip
    } else if has_marks {
        Escape::ClearMarks
    } else {
        Escape::Quit
    }
}

/// Where the cursor lands after an operation reloaded the listing.
///
/// The name it was sitting on wins whenever it survived, which is the same rule
/// [`App::go_parent`] uses to land on the directory it came out of. When that
/// name is gone, the same ROW is kept instead, clamped to the shortened listing:
/// after a delete that is the entry which moved up into the deleted one's place,
/// which is where a file manager leaves the cursor and is far more considerate
/// than snapping back to the top of a long folder. Pure, so both branches are
/// unit-tested without a filesystem.
fn reselect(names: &[&str], wanted: Option<&str>, prev: Option<usize>) -> Option<usize> {
    if names.is_empty() {
        return None;
    }
    if let Some(wanted) = wanted {
        if let Some(i) = names.iter().position(|n| *n == wanted) {
            return Some(i);
        }
    }
    Some(prev.unwrap_or(0).min(names.len() - 1))
}

/// Push a completed journal onto the bounded undo stack (ADR 0017 D8).
///
/// It is the OLDEST entry that goes when the bound is reached: undo walks
/// backwards from the most recent operation, so the far end of the stack is the
/// part nobody is going to reach for. Pure, so the bound is unit-tested rather
/// than trusted.
fn push_journal(stack: &mut Vec<fileop::Journal>, journal: fileop::Journal, depth: usize) {
    stack.push(journal);
    while stack.len() > depth {
        stack.remove(0);
    }
}

/// The bare folder name for a pane title (`~/src/foo` → `foo`), or `/` for the
/// filesystem root, which has no final component.
fn pretty_dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string())
}

/// Plain newline-separated listing for non-interactive use (piped output).
pub fn dump(path: &str) -> String {
    let mut names: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    format!("{n}/")
                } else {
                    n
                }
            })
            .collect(),
        Err(e) => return format!("sucher: {path}: {e}\n"),
    };
    names.sort_by_key(|n| n.to_lowercase());
    let mut out = names.join("\n");
    out.push('\n');
    out
}

/// Wrap width for rendered-markdown previews, from the terminal size.
fn preview_text_width() -> usize {
    let cols = crossterm::terminal::size().map(|(c, _)| c).unwrap_or(80);
    ((cols as usize * 58 / 100).saturating_sub(2)).max(20)
}

/// Read up to `max_bytes` of a file as a lossy String (for markdown source).
fn read_capped(path: &Path) -> String {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = vec![0u8; 256 * 1024];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The passive preview pane's frame: a rounded, `dim`-bordered block carrying
/// an already-styled title. One builder so all three preview states (image,
/// loading, text) share the exact same chrome.
fn preview_block(title: Line<'static>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::palette().dim))
        .title(title)
}

/// Style a preview caption as the block title: accent + BOLD, matching the
/// list's title so the two panes read as one system.
fn preview_caption(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_string(),
        Style::default()
            .fg(theme::palette().accent)
            .add_modifier(Modifier::BOLD),
    ))
}

/// A file caption (`name  ·  type · size`) as a styled block title, truncated to
/// the pane width first (the `-4` reserves the two border cells and the two
/// spaces the `" {…} "` padding adds), preserving the prior clipping behaviour.
fn caption_title(caption: &str, area: Rect) -> Line<'static> {
    let shown = truncate(caption, area.width.saturating_sub(4) as usize);
    preview_caption(&format!(" {shown} "))
}

fn no_preview() -> Line<'static> {
    Line::from(Span::styled(
        "No preview",
        Style::default().fg(theme::palette().dim),
    ))
}

/// Read the head of a file as text, or None if it looks binary / unreadable.
fn head_text(path: &Path, max_bytes: usize, max_lines: usize) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    if buf.contains(&0) {
        return None; // NUL byte → binary
    }
    let s = String::from_utf8_lossy(&buf);
    Some(s.lines().take(max_lines).collect::<Vec<_>>().join("\n"))
}

/// Break `cwd` into clickable breadcrumb segments: `(display_label, absolute
/// target)` pairs walking from the anchor down to `cwd` (ADR 0005 D2). Pure so
/// the mapping is unit-tested without a terminal.
///
/// When `cwd` is under `home`, the first segment is `~` (target = `home`) and
/// each further path component follows, its target the cumulative path — so
/// `/Users/j/src` with home `/Users/j` yields `[("~", /Users/j), ("src",
/// /Users/j/src)]`, rendering as `~/src` (the prior `pretty_path` look). A path
/// outside home is root-anchored: the first segment is `/` (target = `/`) and
/// each component follows, so `/usr/bin` yields `[("/", /), ("usr", /usr),
/// ("bin", /usr/bin)]`. The filesystem root itself is the single segment `/`.
/// PURE: compose the right-aligned repo-HEAD readout for the breadcrumb row
/// (ADR 0004, D2 amendment): `⎇ branch ↑a ↓b ●` plus a trailing pad space.
///
/// - The branch glyph tracks the icon mode: powerline `` under Nerd, `⎇`
///   under Unicode, and the ASCII label `git:` under None (which also swaps
///   `↑/↓/●` for `+n/-n/*` so the row stays pure ASCII).
/// - Identity is the branch name, or `@<short-oid>` when HEAD is detached; an
///   unborn repo (no branch, no commit — not reachable via porcelain, but
///   defensive) yields an empty vec, which the caller draws as nothing.
/// - Ahead/behind arrows appear only when the upstream exists AND the count is
///   non-zero; `dirty` appends the dot when the status map has entries.
fn head_spans(head: &git::RepoHead, dirty: bool, icons: IconMode) -> Vec<Span<'static>> {
    let name = match (&head.branch, &head.oid_short) {
        (Some(branch), _) => branch.clone(),
        (None, Some(oid)) => format!("@{oid}"),
        (None, None) => return Vec::new(),
    };
    let p = theme::palette();
    let ascii = icons == IconMode::None;
    let glyph = match icons {
        IconMode::Nerd => "\u{e0a0}",
        IconMode::Unicode => "⎇",
        IconMode::None => "git:",
    };
    let mut spans = vec![
        Span::styled(glyph.to_string(), Style::default().fg(p.dim)),
        Span::raw(" "),
        Span::styled(name, Style::default().fg(p.accent)),
    ];
    if let Some((ahead, behind)) = head.ahead_behind {
        if ahead > 0 {
            let txt = if ascii {
                format!(" +{ahead}")
            } else {
                format!(" ↑{ahead}")
            };
            spans.push(Span::styled(txt, Style::default().fg(p.sheet)));
        }
        if behind > 0 {
            let txt = if ascii {
                format!(" -{behind}")
            } else {
                format!(" ↓{behind}")
            };
            spans.push(Span::styled(txt, Style::default().fg(p.pdf)));
        }
    }
    if dirty {
        let dot = if ascii { " *" } else { " ●" };
        spans.push(Span::styled(dot.to_string(), Style::default().fg(p.doc)));
    }
    spans.push(Span::raw(" "));
    spans
}

fn crumb_segments(cwd: &Path, home: Option<&Path>) -> Vec<(String, PathBuf)> {
    // Under home: anchor on `~`, then append each remaining component.
    if let Some(home) = home {
        if let Ok(rest) = cwd.strip_prefix(home) {
            let mut out = vec![("~".to_string(), home.to_path_buf())];
            let mut acc = home.to_path_buf();
            for comp in rest.components() {
                let name = comp.as_os_str().to_string_lossy().into_owned();
                acc = acc.join(&name);
                out.push((name, acc.clone()));
            }
            return out;
        }
    }
    // Root-anchored: the `/` segment, then each normal component cumulatively.
    let mut out = vec![("/".to_string(), PathBuf::from("/"))];
    let mut acc = PathBuf::from("/");
    for comp in cwd.components() {
        if let std::path::Component::Normal(os) = comp {
            let name = os.to_string_lossy().into_owned();
            acc = acc.join(&name);
            out.push((name, acc.clone()));
        }
    }
    out
}

/// Resolve a breadcrumb click at column `x` to the target directory of whichever
/// recorded segment span contains it, or `None` if the click missed every label
/// (a separator or empty space). Pure — the geometry is built in `render_crumb`
/// but the hit test itself is unit-tested here (ADR 0005 D2).
fn crumb_hit(hits: &[(Range<u16>, PathBuf)], x: u16) -> Option<PathBuf> {
    hits.iter()
        .find(|(range, _)| range.contains(&x))
        .map(|(_, target)| target.clone())
}

/// Resolve a left-click at screen cell `(row, col)` to an index into the current
/// pane's `view`, or `None` when the click misses an entry (ADR 0005 D2). Pure —
/// the geometry (`list_area`, the `ListState` scroll `offset`, and the visible
/// entry count `view_len`) is captured at render time, but the mapping itself is
/// unit-tested here without a terminal.
///
/// The list is drawn inside a rounded, bordered block: the top border occupies
/// the first row of `list_area` and the bottom border its last, so the clickable
/// entry rows are the inner rows `list_area.y + 1 ..= list_area.y + height - 2`.
/// The visible entries start at the `ListState` scroll `offset`, so the row's
/// entry position is `offset + (row - (list_area.y + 1))`. The click must fall
/// within the pane's inner rows AND inside its x-span (a click in a NEIGHBOURING
/// pane on the same row must not select here), and the resolved index must be a
/// real entry (`< view_len`) — a click below the last entry, on a border, or
/// outside the column range all yield `None`.
/// Whether a screen cell `(col, row)` lies within `r` (borders included). Used to
/// route a left-click to the Miller parent pane (ADR 0005 D2).
fn rect_contains(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x
        && col < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

fn row_to_index(
    list_area: Rect,
    offset: usize,
    row: u16,
    col: u16,
    view_len: usize,
) -> Option<usize> {
    // Reject clicks outside the pane's horizontal span (borders included): a
    // click at this row but in an adjacent column belongs to another pane.
    if col < list_area.x || col >= list_area.x.saturating_add(list_area.width) {
        return None;
    }
    // The first entry row is just below the top border; the last inner row is
    // just above the bottom border. A degenerate pane (height < 3) has no inner
    // rows and the range check below rejects every click.
    let first_row = list_area.y.saturating_add(1);
    let last_inner = list_area
        .y
        .saturating_add(list_area.height)
        .saturating_sub(2);
    if row < first_row || row > last_inner {
        return None; // on a border or outside the pane vertically
    }
    let idx = offset + (row - first_row) as usize;
    (idx < view_len).then_some(idx)
}

/// The selection after a `Space` mark (ADR 0017 D4): one row down from `cur` in a
/// listing of `len` rows, so holding the key marks a run without a second hand on
/// `j`. It stops at the last row rather than wrapping, matching the clamp every
/// other browse motion uses ([`App::move_sel`]): wrapping would silently send the
/// cursor back to the top and re-toggle rows the user had just marked. `None` on an
/// empty listing, where there is nothing to mark and nothing to select. Pure, so
/// the arithmetic is unit-tested without a terminal.
fn mark_advance(cur: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some((cur.unwrap_or(0) + 1).min(len - 1))
}

/// The status-line summary of the mark set (ADR 0017 D3), e.g. `3 marked · 1.1M`.
/// A selection held across directories must never be invisible state, and this line
/// is what keeps it visible.
///
/// `bytes` is [`crate::marks::Marks::bytes`], which totals the marked FILES only:
/// listings give directories size 0, so a marked folder contributes nothing and the
/// figure says nothing about the tree underneath it. Printing that number beside a
/// count that includes folders would imply their contents had been measured, so the
/// folders are counted out separately (`3 marked · 1.1M + 2 folders`), and a
/// selection of nothing but folders drops the size rather than claiming a
/// misleading `0 B`. Only the operation planner, which walks each tree, can give a
/// real recursive total. Pure, so the wording is unit-tested.
fn marks_status(total: usize, dirs: usize, bytes: u64) -> String {
    let files = total.saturating_sub(dirs);
    let folders = if dirs == 1 { "folder" } else { "folders" };
    let size = if dirs == 0 {
        crate::util::human_size(bytes)
    } else if files == 0 {
        format!("{dirs} {folders}")
    } else {
        format!("{} + {dirs} {folders}", crate::util::human_size(bytes))
    };
    format!("{total} marked · {size}")
}

/// Clamp a search-results selection move (ADR 0007 §7): from the current selection,
/// a signed `delta`, and the result count `len`, return the new selection. `None`
/// when there are no results; otherwise the moved index clamped into `0..len`
/// (mirrors [`App::move_sel`]). Pure — unit-tested without a walk.
fn search_sel(cur: Option<usize>, delta: isize, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let cur = cur.unwrap_or(0) as isize;
    Some((cur + delta).clamp(0, len as isize - 1) as usize)
}

/// The dimmed trailing snippet segment for a content hit's result row (ADR 0007 D5):
/// ` N: text` (a leading gap separating it from the path), or empty when the hit
/// carries no snippet (a pure name/metadata match). Pure — unit-tested without a
/// render.
fn snippet_suffix(snippet: Option<&(u64, String)>) -> String {
    match snippet {
        Some((lnum, text)) => format!("  {lnum}: {text}"),
        None => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Truncate a cell to `w` columns then right-pad with spaces to exactly `w`, for
/// the aligned spreadsheet preview grid.
fn pad_cell(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let len = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(len)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Build a bare `Entry` for comparator tests — only the fields `sort_cmp`
    /// reads (`name`, `kind`, `size`, `modified`) matter; `path` is a throwaway.
    fn entry(name: &str, kind: Format, size: u64, mtime: Option<SystemTime>) -> Entry {
        Entry {
            name: name.to_string(),
            path: PathBuf::from(name),
            kind,
            size,
            modified: mtime,
        }
    }

    /// Sort a list with `sort` and return the names, for order assertions.
    fn sorted_names(mut v: Vec<Entry>, sort: Sort) -> Vec<String> {
        v.sort_by(|a, b| sort_cmp(a, b, sort));
        v.into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn dirs_always_sort_before_files_regardless_of_key_or_reverse() {
        // A big-sized dir and a small file: under size sort (and its reverse) the
        // directory must still lead — the group boundary is never crossed.
        let v = || {
            vec![
                entry("zzz.txt", Format::Text, 1, None),
                entry("adir", Format::Directory, 999, None),
            ]
        };
        for reverse in [false, true] {
            let names = sorted_names(
                v(),
                Sort {
                    key: SortKey::Size,
                    reverse,
                },
            );
            assert_eq!(names[0], "adir", "dir must lead (reverse={reverse})");
        }
    }

    #[test]
    fn name_sort_is_case_insensitive_and_reverses() {
        let v = || {
            vec![
                entry("Banana", Format::Text, 0, None),
                entry("apple", Format::Text, 0, None),
                entry("Cherry", Format::Text, 0, None),
            ]
        };
        assert_eq!(
            sorted_names(v(), Sort::default()),
            vec!["apple", "Banana", "Cherry"]
        );
        assert_eq!(
            sorted_names(
                v(),
                Sort {
                    key: SortKey::Name,
                    reverse: true
                }
            ),
            vec!["Cherry", "Banana", "apple"]
        );
    }

    #[test]
    fn size_sort_orders_ascending_then_breaks_ties_by_name() {
        let v = vec![
            entry("big", Format::Text, 100, None),
            entry("small", Format::Text, 10, None),
            entry("mid_b", Format::Text, 50, None),
            entry("mid_a", Format::Text, 50, None), // tie with mid_b → name breaks it
        ];
        assert_eq!(
            sorted_names(
                v,
                Sort {
                    key: SortKey::Size,
                    reverse: false
                }
            ),
            vec!["small", "mid_a", "mid_b", "big"]
        );
    }

    #[test]
    fn modified_sort_oldest_first_missing_counts_as_oldest() {
        let base = SystemTime::UNIX_EPOCH;
        let older = base + Duration::from_secs(100);
        let newer = base + Duration::from_secs(200);
        let v = vec![
            entry("new", Format::Text, 0, Some(newer)),
            entry("none", Format::Text, 0, None), // None < Some → oldest
            entry("old", Format::Text, 0, Some(older)),
        ];
        assert_eq!(
            sorted_names(
                v,
                Sort {
                    key: SortKey::Modified,
                    reverse: false
                }
            ),
            vec!["none", "old", "new"]
        );
    }

    #[test]
    fn ext_sort_groups_by_extension_then_name() {
        let v = vec![
            entry("b.rs", Format::Text, 0, None),
            entry("a.rs", Format::Text, 0, None),
            entry("c.md", Format::Text, 0, None),
            entry("readme", Format::Text, 0, None), // no ext → sorts first ("")
        ];
        assert_eq!(
            sorted_names(
                v,
                Sort {
                    key: SortKey::Ext,
                    reverse: false
                }
            ),
            vec!["readme", "c.md", "a.rs", "b.rs"]
        );
    }

    #[test]
    fn name_ext_handles_dotfiles_and_missing() {
        // Returns the RAW slice (not lower-cased); case folding is `cmp_name_ci`'s job.
        assert_eq!(name_ext("photo.JPG"), "JPG"); // raw slice, unfolded
        assert_eq!(name_ext("archive.tar.gz"), "gz"); // last component only
        assert_eq!(name_ext("README"), ""); // none
        assert_eq!(name_ext(".gitignore"), ""); // leading dot = stem, not ext
    }

    #[test]
    fn cmp_name_ci_is_case_insensitive_and_ordered() {
        use std::cmp::Ordering;
        // Same letters, different case → equal.
        assert_eq!(cmp_name_ci("Apple", "apple"), Ordering::Equal);
        // Ordering ignores case ("Apple" before "banana" whatever the casing).
        assert_eq!(cmp_name_ci("Apple", "banana"), Ordering::Less);
        assert_eq!(cmp_name_ci("apple", "Banana"), Ordering::Less);
        assert_eq!(cmp_name_ci("BANANA", "apple"), Ordering::Greater);
        // A prefix sorts before the longer string (shorter runs out first).
        assert_eq!(cmp_name_ci("app", "apple"), Ordering::Less);
        assert_eq!(cmp_name_ci("apple", "app"), Ordering::Greater);
    }

    #[test]
    fn ext_sort_order_is_case_insensitive() {
        // The Ext key folds case via `cmp_name_ci`, so `.RS` and `.rs` group and
        // order together — the raw slices differ in case but the order does not.
        let v = vec![
            entry("b.RS", Format::Text, 0, None),
            entry("a.rs", Format::Text, 0, None),
            entry("c.Md", Format::Text, 0, None),
        ];
        assert_eq!(
            sorted_names(
                v,
                Sort {
                    key: SortKey::Ext,
                    reverse: false
                }
            ),
            vec!["c.Md", "a.rs", "b.RS"] // md < rs, then a < b within .rs/.RS
        );
    }

    #[test]
    fn search_hits_sort_by_relative_path_grouped_by_folder() {
        // Hits sort by `rel` (their Sortable name), so the flat result list reads
        // folder-grouped — every `src/…` before `zzz.txt`, siblings adjacent —
        // rather than in nondeterministic walk-arrival order.
        let hit = |rel: &str| crate::search::Hit {
            path: PathBuf::from(rel),
            rel: rel.to_string(),
            kind: Format::Text,
            size: 0,
            modified: None,
            snippet: None,
        };
        let mut v = [
            hit("zzz.txt"),
            hit("src/b.rs"),
            hit("src/a.rs"),
            hit("readme.md"),
        ];
        v.sort_by(|a, b| sort_cmp(a, b, Sort::default()));
        let rels: Vec<&str> = v.iter().map(|h| h.rel.as_str()).collect();
        assert_eq!(rels, vec!["readme.md", "src/a.rs", "src/b.rs", "zzz.txt"]);
    }

    #[test]
    fn sort_key_cycles_through_all_four() {
        let k = SortKey::Name;
        let k = k.cycle();
        assert!(matches!(k, SortKey::Size));
        let k = k.cycle();
        assert!(matches!(k, SortKey::Modified));
        let k = k.cycle();
        assert!(matches!(k, SortKey::Ext));
        let k = k.cycle();
        assert!(matches!(k, SortKey::Name)); // wraps
    }

    #[test]
    fn miller_three_columns_only_when_wide_and_parented() {
        // Miller + wide + parent → three panes.
        assert_eq!(effective_columns(Layout::Miller, MILLER_MIN, true), 3);
        assert_eq!(effective_columns(Layout::Miller, 200, true), 3);
        // Too narrow → collapse to two.
        assert_eq!(effective_columns(Layout::Miller, MILLER_MIN - 1, true), 2);
        // No parent (filesystem root) → two, however wide.
        assert_eq!(effective_columns(Layout::Miller, 200, false), 2);
    }

    #[test]
    fn double_is_always_two_columns() {
        assert_eq!(effective_columns(Layout::Double, 200, true), 2);
        assert_eq!(effective_columns(Layout::Double, 40, true), 2);
    }

    #[test]
    fn crumb_segments_under_home_are_tilde_anchored() {
        let home = PathBuf::from("/Users/j");
        let segs = crumb_segments(Path::new("/Users/j/src/app"), Some(&home));
        assert_eq!(
            segs,
            vec![
                ("~".to_string(), PathBuf::from("/Users/j")),
                ("src".to_string(), PathBuf::from("/Users/j/src")),
                ("app".to_string(), PathBuf::from("/Users/j/src/app")),
            ]
        );
        // Home itself is the single `~` segment.
        assert_eq!(
            crumb_segments(&home, Some(&home)),
            vec![("~".to_string(), PathBuf::from("/Users/j"))]
        );
    }

    #[test]
    fn crumb_segments_outside_home_are_root_anchored() {
        let home = PathBuf::from("/Users/j");
        let segs = crumb_segments(Path::new("/usr/local/bin"), Some(&home));
        assert_eq!(
            segs,
            vec![
                ("/".to_string(), PathBuf::from("/")),
                ("usr".to_string(), PathBuf::from("/usr")),
                ("local".to_string(), PathBuf::from("/usr/local")),
                ("bin".to_string(), PathBuf::from("/usr/local/bin")),
            ]
        );
        // The filesystem root is the single `/` segment.
        assert_eq!(
            crumb_segments(Path::new("/"), Some(&home)),
            vec![("/".to_string(), PathBuf::from("/"))]
        );
        // No home known → also root-anchored.
        assert_eq!(
            crumb_segments(Path::new("/etc"), None),
            vec![
                ("/".to_string(), PathBuf::from("/")),
                ("etc".to_string(), PathBuf::from("/etc")),
            ]
        );
    }

    /// Flatten spans to the plain text the row would show.
    fn spans_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn head_spans_branch_ahead_behind_dirty() {
        let head = git::RepoHead {
            branch: Some("main".to_string()),
            oid_short: Some("0123456".to_string()),
            ahead_behind: Some((2, 1)),
        };
        let s = head_spans(&head, true, IconMode::Unicode);
        assert_eq!(spans_text(&s), "⎇ main ↑2 ↓1 ● ");
    }

    #[test]
    fn head_spans_in_sync_clean_is_just_the_branch() {
        let head = git::RepoHead {
            branch: Some("main".to_string()),
            oid_short: Some("0123456".to_string()),
            ahead_behind: Some((0, 0)),
        };
        assert_eq!(
            spans_text(&head_spans(&head, false, IconMode::Unicode)),
            "⎇ main "
        );
    }

    #[test]
    fn head_spans_detached_shows_short_oid() {
        let head = git::RepoHead {
            branch: None,
            oid_short: Some("abc1234".to_string()),
            ahead_behind: None,
        };
        assert_eq!(
            spans_text(&head_spans(&head, false, IconMode::Nerd)),
            "\u{e0a0} @abc1234 "
        );
    }

    #[test]
    fn head_spans_ascii_mode_is_pure_ascii() {
        let head = git::RepoHead {
            branch: Some("feat/x".to_string()),
            oid_short: Some("abc1234".to_string()),
            ahead_behind: Some((3, 0)),
        };
        let text = spans_text(&head_spans(&head, true, IconMode::None));
        assert_eq!(text, "git: feat/x +3 * ");
        assert!(text.is_ascii());
    }

    #[test]
    fn head_spans_unborn_repo_renders_nothing() {
        let head = git::RepoHead {
            branch: None,
            oid_short: None,
            ahead_behind: None,
        };
        assert!(head_spans(&head, true, IconMode::Unicode).is_empty());
    }

    #[test]
    fn crumb_hit_resolves_column_to_target() {
        let hits = vec![
            (1u16..2u16, PathBuf::from("/Users/j")),     // "~" at col 1
            (3u16..6u16, PathBuf::from("/Users/j/src")), // "src" at cols 3..6
        ];
        // Inside a span → its target; ranges are half-open (end excluded).
        assert_eq!(crumb_hit(&hits, 1), Some(PathBuf::from("/Users/j")));
        assert_eq!(crumb_hit(&hits, 3), Some(PathBuf::from("/Users/j/src")));
        assert_eq!(crumb_hit(&hits, 5), Some(PathBuf::from("/Users/j/src")));
        // On the separator (col 2), past the end, or before the start → no hit.
        assert_eq!(crumb_hit(&hits, 2), None);
        assert_eq!(crumb_hit(&hits, 6), None);
        assert_eq!(crumb_hit(&hits, 0), None);
    }

    #[test]
    fn row_to_index_maps_clicks_inside_the_pane() {
        // A pane at (x=10, y=2), 30 wide, 10 tall: top border at row 2, first
        // entry row at 3, bottom border at row 11 (y+height-1), last inner row 10.
        let area = Rect {
            x: 10,
            y: 2,
            width: 30,
            height: 10,
        };
        // No scroll: the first entry row → view[0].
        assert_eq!(row_to_index(area, 0, 3, 15, 5), Some(0));
        assert_eq!(row_to_index(area, 0, 4, 15, 5), Some(1));
        // The top border (row 2) is not an entry → None.
        assert_eq!(row_to_index(area, 0, 2, 15, 5), None);
        // A row past the entries (view_len == 5, so rows 3..=7 are valid) → None.
        assert_eq!(row_to_index(area, 0, 8, 15, 5), None);
        // A click on the bottom border (row 11) → None.
        assert_eq!(row_to_index(area, 0, 11, 15, 5), None);
        // A click outside the pane's x-range (adjacent pane, same row) → None.
        assert_eq!(row_to_index(area, 0, 3, 9, 5), None); // left of x
        assert_eq!(row_to_index(area, 0, 3, 40, 5), None); // at x+width (excluded)
                                                           // With a scrolled list (offset 12), the first visible row is view[12].
        assert_eq!(row_to_index(area, 12, 3, 15, 100), Some(12));
        assert_eq!(row_to_index(area, 12, 5, 15, 100), Some(14));
        // Offset math must still bounds-check: row resolves to index 14 but the
        // view only has 13 entries → None (never an out-of-bounds select).
        assert_eq!(row_to_index(area, 12, 5, 15, 13), None);
    }

    #[test]
    fn visible_window_scrolls_to_keep_selection_visible() {
        // Selection already inside the current window → no scroll, window is
        // `offset..offset+height`.
        assert_eq!(visible_window(0, Some(3), 100, 10), (0, 0..10));
        assert_eq!(visible_window(5, Some(8), 100, 10), (5, 5..15));
        // Scroll DOWN: selection past the bottom of the window → offset lands it on
        // the last visible row (`selected - height + 1`).
        assert_eq!(visible_window(0, Some(50), 100, 10), (41, 41..51));
        assert_eq!(visible_window(0, Some(9), 100, 10), (0, 0..10)); // last row still fits
        assert_eq!(visible_window(0, Some(10), 100, 10), (1, 1..11)); // one past → scroll 1
                                                                      // Scroll UP: selection above the window → offset drops to the selection.
        assert_eq!(visible_window(40, Some(12), 100, 10), (12, 12..22));
        // Clamp at the end: a big offset with no lower selection can't scroll past
        // `len - height`, so the last page is fully filled.
        assert_eq!(visible_window(999, None, 100, 10), (90, 90..100));
        assert_eq!(visible_window(0, Some(99), 100, 10), (90, 90..100));
        // Empty / short list, and zero height → offset 0 and an empty range.
        assert_eq!(visible_window(0, None, 0, 10), (0, 0..0));
        assert_eq!(visible_window(7, Some(3), 0, 10), (0, 0..0));
        assert_eq!(visible_window(0, Some(0), 100, 0), (0, 0..0));
        // Shorter than the viewport: the whole list is the window, offset 0.
        assert_eq!(visible_window(0, Some(2), 4, 10), (0, 0..4));
        // A stale offset left over from a bigger listing is clamped down.
        assert_eq!(visible_window(30, Some(0), 5, 40), (0, 0..5));
    }

    #[test]
    fn rect_contains_includes_borders_excludes_beyond() {
        let r = Rect {
            x: 5,
            y: 1,
            width: 4,
            height: 3,
        };
        assert!(rect_contains(r, 5, 1)); // top-left corner
        assert!(rect_contains(r, 8, 3)); // bottom-right corner (x+w-1, y+h-1)
        assert!(!rect_contains(r, 9, 3)); // x+w is excluded
        assert!(!rect_contains(r, 8, 4)); // y+h is excluded
        assert!(!rect_contains(r, 4, 2)); // left of x
    }

    #[test]
    fn slide_offsets_endpoints_and_midpoint() {
        // FromRight (entered a child): old exits left, new enters from the right.
        // t = 0 → old in place (0), new fully off to the right (+w).
        assert_eq!(slide_offsets(SlideDir::FromRight, 0.0, 40), (0, 40));
        // t = 1 → old fully off left (-w), new landed exactly in place (0). THIS is
        // the identity: at the settle frame the new content sits at offset 0, so it
        // matches the normal render pixel-for-pixel.
        assert_eq!(slide_offsets(SlideDir::FromRight, 1.0, 40), (-40, 0));
        // Midpoint: old has travelled round(0.5*40)=20 left, new is round(0.5*40)=20
        // still to the right — the two layers tile the inner width.
        assert_eq!(slide_offsets(SlideDir::FromRight, 0.5, 40), (-20, 20));

        // FromLeft (went to the parent): mirror image of FromRight.
        // t = 0 → old in place (0), new fully off to the left (-w).
        assert_eq!(slide_offsets(SlideDir::FromLeft, 0.0, 40), (0, -40));
        // t = 1 → old fully off right (+w), new landed in place (0) — the identity.
        assert_eq!(slide_offsets(SlideDir::FromLeft, 1.0, 40), (40, 0));
        // Midpoint mirrored.
        assert_eq!(slide_offsets(SlideDir::FromLeft, 0.5, 40), (20, -20));
    }

    #[test]
    fn search_sel_clamps_and_handles_empty() {
        // No results → always None, whatever the delta.
        assert_eq!(search_sel(None, 1, 0), None);
        assert_eq!(search_sel(Some(0), -1, 0), None);
        // Fresh (no selection yet) → treated as 0, then moved.
        assert_eq!(search_sel(None, 0, 5), Some(0));
        assert_eq!(search_sel(None, 1, 5), Some(1));
        // Normal moves.
        assert_eq!(search_sel(Some(2), 1, 5), Some(3));
        assert_eq!(search_sel(Some(2), -1, 5), Some(1));
        // Clamped at both ends (no wrap).
        assert_eq!(search_sel(Some(0), -1, 5), Some(0));
        assert_eq!(search_sel(Some(4), 1, 5), Some(4));
        // Half-page jumps clamp too.
        assert_eq!(search_sel(Some(4), 10, 5), Some(4));
        assert_eq!(search_sel(Some(1), -10, 5), Some(0));
    }

    #[test]
    fn snippet_suffix_formats_or_empties() {
        // A content hit → " N: text" with the leading gap.
        assert_eq!(
            snippet_suffix(Some(&(42, "let x = 1;".to_string()))),
            "  42: let x = 1;"
        );
        // A pure name/metadata hit → empty (no snippet segment drawn).
        assert_eq!(snippet_suffix(None), "");
    }

    /// Every mark key must be a BOUND char, so typeahead treats it as a motion
    /// and never as the first letter of a name search (ADR 0002 D2, ADR 0017 D4).
    /// `Space` is the one that matters most: an unbound space would open a name
    /// buffer that no file name can ever start with.
    #[test]
    fn mark_keys_are_bound_chars_so_typeahead_passes_them_through() {
        assert!(matches!(browse_char(' '), Some(CharAction::ToggleMark)));
        assert!(matches!(browse_char('V'), Some(CharAction::InvertMarks)));
        // The typeahead precedence rule reads `browse_char` directly, so an idle
        // press of either key runs the binding instead of starting a session.
        for c in [' ', 'V'] {
            assert!(matches!(
                typeahead::action(false, browse_char(c).is_some()),
                typeahead::Action::PassThrough
            ));
        }
        // A letter that is still free must keep starting a name search, proving
        // the two new bindings did not quietly swallow the alphabet.
        assert!(browse_char('R').is_none());
        // `Ctrl-a` is still not a `browse_char` binding, and cannot be: that map
        // takes a bare character and has no way to say "with Control held", so
        // `handle_key` tests the modifier itself, above the typeahead block. The
        // BARE `a` is now the create binding (ADR 0017 D4), which is why the two
        // can no longer be told apart from this map alone.
        assert!(matches!(browse_char('a'), Some(CharAction::Create)));
    }

    #[test]
    fn mark_advance_steps_down_and_stops_at_the_last_row() {
        // Nothing listed: nothing to mark, nothing to select.
        assert_eq!(mark_advance(None, 0), None);
        assert_eq!(mark_advance(Some(3), 0), None);
        // A fresh listing with no selection yet behaves as if on row 0.
        assert_eq!(mark_advance(None, 5), Some(1));
        // The ordinary case: holding Space walks down the listing.
        assert_eq!(mark_advance(Some(0), 5), Some(1));
        assert_eq!(mark_advance(Some(3), 5), Some(4));
        // At the last row it toggles in place and does NOT wrap to the top, which
        // would re-toggle rows the user had just marked.
        assert_eq!(mark_advance(Some(4), 5), Some(4));
        // A single-row listing is the same rule taken to its limit.
        assert_eq!(mark_advance(Some(0), 1), Some(0));
    }

    /// The mark gutter is invisible until used (ADR 0017 D3): with no marks the
    /// reserved width is exactly what it was before the feature, and with marks it
    /// is two cells wider, whatever the icon mode and whatever git is doing.
    #[test]
    fn mark_gutter_costs_two_cells_only_when_it_is_drawn() {
        for icons in [IconMode::None, IconMode::Unicode, IconMode::Nerd] {
            for git in [false, true] {
                let without = entry_chrome_w(icons, git, false);
                let with = entry_chrome_w(icons, git, true);
                assert_eq!(with, without + 2, "{git}");
            }
        }
        // The pre-feature values, pinned: borders + cursor gutter, plus the glyph
        // cell in the glyphed modes, plus the git gutter when a repo is listed.
        assert_eq!(entry_chrome_w(IconMode::None, false, false), 4);
        assert_eq!(entry_chrome_w(IconMode::Unicode, false, false), 6);
        assert_eq!(entry_chrome_w(IconMode::Nerd, false, false), 6);
        assert_eq!(entry_chrome_w(IconMode::Unicode, true, false), 8);
        // Both gutters drawn at once still just add their two cells each.
        assert_eq!(entry_chrome_w(IconMode::Unicode, true, true), 10);
        assert_eq!(entry_chrome_w(IconMode::None, true, true), 8);
    }

    /// The gutter is drawn only for a non-empty set, and only where a set is
    /// passed at all: the parent pane passes `None` and so can never draw one.
    #[test]
    fn mark_gutter_is_some_only_when_something_is_marked() {
        let mut marks = crate::marks::Marks::new();
        assert!(mark_gutter(&marks).is_none());
        marks.insert(Path::new("/a/b.txt"), 10, false);
        assert!(mark_gutter(&marks).is_some());
        marks.clear();
        assert!(mark_gutter(&marks).is_none());
    }

    /// The glyph is one cell in every mode, and pure ASCII under `IconMode::None`,
    /// following the convention `head_spans` set for that mode.
    #[test]
    fn mark_glyph_is_one_cell_and_ascii_under_icon_mode_none() {
        assert_eq!(mark_glyph(IconMode::None), "*");
        for icons in [IconMode::None, IconMode::Unicode, IconMode::Nerd] {
            assert_eq!(mark_glyph(icons).chars().count(), 1);
        }
        assert!(mark_glyph(IconMode::None).is_ascii());
    }

    /// The status line is honest about what was measured (ADR 0017 D3): the byte
    /// total covers marked files only, so marked folders are counted out rather
    /// than folded into a figure that would imply their trees had been walked.
    #[test]
    fn marks_status_never_implies_folder_contents_were_measured() {
        // Files only: the plain count and total.
        assert_eq!(marks_status(3, 0, 1_200_000), "3 marked · 1.1M");
        assert_eq!(marks_status(1, 0, 0), "1 marked · 0 B");
        // A mixed selection names the folders separately, so the size is plainly
        // "and these folders on top", not "this is everything".
        assert_eq!(marks_status(3, 1, 2048), "3 marked · 2.0K + 1 folder");
        assert_eq!(marks_status(5, 2, 2048), "5 marked · 2.0K + 2 folders");
        // Folders only: no size at all, because `0 B` would be a lie by omission.
        assert_eq!(marks_status(1, 1, 0), "1 marked · 1 folder");
        assert_eq!(marks_status(2, 2, 0), "2 marked · 2 folders");
    }

    /// A trash step as the planner resolves it: a source, no destination (ADR
    /// 0017 D7). Built by hand so the overlay's formatting is tested without a
    /// filesystem, since nothing in these tests may touch a real path or a real
    /// trash.
    fn trash_step(src: &str, items: usize, bytes: u64) -> fileop::Step {
        fileop::Step {
            src: PathBuf::from(src),
            dest: PathBuf::new(),
            kind: fileop::NodeKind::File,
            nodes: Vec::new(),
            items,
            bytes,
            renamed: false,
            overwrite: false,
        }
    }

    /// `D` must be a BOUND char so typeahead treats it as a motion and never as
    /// the first letter of a name search (ADR 0002 D2, ADR 0017 D4). Unbound, `D`
    /// would jump to `Downloads/` instead of asking about the trash.
    #[test]
    fn trash_is_a_bound_char_so_typeahead_passes_it_through() {
        assert!(matches!(browse_char('D'), Some(CharAction::Trash)));
        assert!(matches!(
            typeahead::action(false, browse_char('D').is_some()),
            typeahead::Action::PassThrough
        ));
        // The lowercase half-page motion is untouched: the two keys are distinct
        // bindings, not one case-folded one.
        assert!(matches!(browse_char('d'), Some(CharAction::HalfDown)));
    }

    /// The rule every later operation will share (ADR 0017 D3): act on the marks
    /// when there are any, otherwise on the row under the cursor.
    #[test]
    fn targets_prefer_the_mark_set_and_fall_back_to_the_cursor() {
        let mut marks = crate::marks::Marks::new();
        let cursor = PathBuf::from("/here/under-cursor.txt");

        // Nothing marked: the cursor alone.
        assert_eq!(
            targets(&marks, Some(cursor.as_path())),
            vec![cursor.clone()]
        );
        // Nothing marked and nothing selected (an empty listing): an empty batch,
        // which the planner refuses by name rather than this helper guessing.
        assert!(targets(&marks, None).is_empty());

        // With marks, the cursor is ignored entirely, even when it is a row that
        // is not itself marked.
        marks.insert(Path::new("/a/one.txt"), 1, false);
        marks.insert(Path::new("/b/two.txt"), 2, false);
        assert_eq!(
            targets(&marks, Some(cursor.as_path())),
            vec![PathBuf::from("/a/one.txt"), PathBuf::from("/b/two.txt")]
        );
        // Mark order, not sorted order: the planner allocates collision suffixes
        // walking this list, so the sequence is a correctness property.
        marks.clear();
        marks.insert(Path::new("/z/last.txt"), 1, false);
        marks.insert(Path::new("/a/first.txt"), 1, false);
        assert_eq!(
            targets(&marks, None),
            vec![PathBuf::from("/z/last.txt"), PathBuf::from("/a/first.txt")]
        );
    }

    /// The undo stack is bounded at 16 and drops the OLDEST journal (ADR 0017 D8).
    #[test]
    fn the_undo_stack_is_bounded_and_evicts_the_oldest() {
        let journal = |n: usize| fileop::Journal {
            kind: fileop::Kind::Trash,
            steps: vec![fileop::Undoable::Trashed {
                path: PathBuf::from(format!("/gone/{n}")),
            }],
        };
        let mut stack = Vec::new();
        for n in 0..UNDO_DEPTH {
            push_journal(&mut stack, journal(n), UNDO_DEPTH);
        }
        assert_eq!(stack.len(), UNDO_DEPTH);
        assert_eq!(stack[0], journal(0));

        // One past the bound: the newest is kept and the oldest goes, because undo
        // walks backwards and nobody reaches the far end.
        push_journal(&mut stack, journal(99), UNDO_DEPTH);
        assert_eq!(stack.len(), UNDO_DEPTH);
        assert_eq!(stack[0], journal(1));
        assert_eq!(stack[UNDO_DEPTH - 1], journal(99));

        // The bound holds however far past it the stack is pushed.
        for n in 100..120 {
            push_journal(&mut stack, journal(n), UNDO_DEPTH);
        }
        assert_eq!(stack.len(), UNDO_DEPTH);
        assert_eq!(stack[UNDO_DEPTH - 1], journal(119));
    }

    /// A finished run as the executor hands it over, built by hand so the whole
    /// report surface is tested without a filesystem, a trash, or a thread.
    fn report(
        kind: fileop::Kind,
        direction: fileop::Direction,
        failures: &[&str],
        notes: &[&str],
    ) -> fileop::Report {
        fileop::Report {
            kind,
            direction,
            items: 1,
            bytes: 0,
            failures: failures
                .iter()
                .map(|msg| fileop::Failure {
                    path: PathBuf::from("/somewhere"),
                    msg: (*msg).to_string(),
                })
                .collect(),
            notes: notes.iter().map(|n| (*n).to_string()).collect(),
            // Empty exactly as an undo's is, which is what keeps `finish_op` from
            // pushing an undo onto the stack it was popped from (ADR 0017 D8).
            journal: fileop::Journal {
                kind,
                steps: Vec::new(),
            },
        }
    }

    /// `U` and `Y` must be BOUND chars, so typeahead treats them as motions and
    /// never as the first letter of a name search (ADR 0002 D2, ADR 0017 D4).
    /// Unbound, `U` would jump to `Users/` rather than undoing anything.
    #[test]
    fn undo_and_yank_path_are_bound_chars_so_typeahead_passes_them_through() {
        assert!(matches!(browse_char('U'), Some(CharAction::Undo)));
        assert!(matches!(browse_char('Y'), Some(CharAction::YankPath)));
        for c in ['U', 'Y'] {
            assert!(matches!(
                typeahead::action(false, browse_char(c).is_some()),
                typeahead::Action::PassThrough
            ));
        }
        // Both are capitals precisely so their lowercase motions survive: `u` is
        // still half a page up and `y` is still the clipboard-for-paste key.
        assert!(matches!(browse_char('u'), Some(CharAction::HalfUp)));
        assert!(matches!(browse_char('y'), Some(CharAction::Yank)));
    }

    /// A run's sentence is composed from BOTH halves the engine reports, because an
    /// undo's report carries the kind of the operation it reversed (ADR 0017 D8):
    /// on `Kind` alone, undoing a move would announce a move.
    #[test]
    fn the_done_status_names_the_direction_the_run_travelled() {
        let fwd = fileop::Direction::Forward;
        let undo = fileop::Direction::Undo;
        // Every kind, both ways. The forward column is the pre-undo wording,
        // unchanged, so nothing about a paste reads differently now.
        let cases = [
            (fileop::Kind::Copy, fwd, "copied 2 items"),
            (fileop::Kind::Copy, undo, "undid the copy of 2 items"),
            (fileop::Kind::Move, fwd, "moved 2 items"),
            (fileop::Kind::Move, undo, "undid the move of 2 items"),
            (fileop::Kind::Rename, fwd, "renamed 2 items"),
            (fileop::Kind::Rename, undo, "undid the rename of 2 items"),
            (fileop::Kind::Create, fwd, "created 2 items"),
            (fileop::Kind::Create, undo, "undid the creation of 2 items"),
            (
                fileop::Kind::Trash,
                fwd,
                "trashed 2 items · restore from the system trash",
            ),
            (fileop::Kind::Trash, undo, "undid the trashing of 2 items"),
        ];
        for (kind, direction, expected) in cases {
            assert_eq!(op_done_status(kind, direction, 2, 0), expected);
        }
        // The trash hint is a forward-only clause: on the way back the undo's own
        // note names the paths, which says it better than a generic sentence can.
        assert!(!op_done_status(fileop::Kind::Trash, undo, 0, 0).contains("restore from"));
        // Singular and the size clause survive the new arm untouched.
        assert_eq!(
            op_done_status(fileop::Kind::Rename, undo, 1, 0),
            "undid the rename of 1 item"
        );
        assert_eq!(
            op_done_status(fileop::Kind::Move, undo, 1, 1024),
            "undid the move of 1 item, 1.0K"
        );
    }

    /// The progress line an undo watches names the operation being reversed, since
    /// an undo has no plan whose summary it could quote.
    #[test]
    fn the_undo_progress_label_names_the_operation_being_reversed() {
        assert_eq!(undo_label(fileop::Kind::Move), "undo the move");
        assert_eq!(undo_label(fileop::Kind::Create), "undo the creation");
        assert_eq!(
            op_progress_status(&undo_label(fileop::Kind::Move), 1, 3, Path::new("/a/b.txt")),
            "undo the move · 1/3 · b.txt"
        );
    }

    /// A report with notes and no failures must still reach the user (ADR 0017 D8).
    /// The old condition asked only about failures, so a successful undo carrying
    /// the one note it exists to deliver would have said nothing at all.
    #[test]
    fn a_report_with_only_notes_still_opens_the_overlay() {
        let quiet = report(fileop::Kind::Copy, fileop::Direction::Forward, &[], &[]);
        assert!(!report_speaks(&quiet), "a clean run has nothing to add");

        let noted = report(
            fileop::Kind::Trash,
            fileop::Direction::Undo,
            &[],
            &["/a.txt went to the system trash"],
        );
        assert!(report_speaks(&noted));

        let failed = report(
            fileop::Kind::Copy,
            fileop::Direction::Forward,
            &["cannot copy /a.txt"],
            &[],
        );
        assert!(report_speaks(&failed));
        // Both at once is the partial undo, and it says both.
        let both = report(
            fileop::Kind::Move,
            fileop::Direction::Undo,
            &["cannot put /a.txt back"],
            &["/b.txt went to the system trash"],
        );
        assert!(report_speaks(&both));
    }

    /// Advice must never be mixed into the red list, which is the whole reason the
    /// engine keeps notes and failures apart (ADR 0017 D8).
    #[test]
    fn notes_render_beside_failures_and_never_in_the_danger_colour() {
        let both = report(
            fileop::Kind::Move,
            fileop::Direction::Undo,
            &["cannot put /a.txt back"],
            &["/b.txt went to the system trash"],
        );
        // Failures first, so a short popup counts away advice rather than a failure.
        assert_eq!(
            report_rows(&both),
            vec![
                ("cannot put /a.txt back".to_string(), true),
                ("/b.txt went to the system trash".to_string(), false),
            ]
        );

        let danger = theme::palette().pdf;
        let dim = theme::palette().dim;
        assert_ne!(danger, dim, "the two readings must be distinguishable");
        let lines = report_lines(&both, 80, 10);
        // Row 0 is the summary and row 1 is blank, so the two list rows follow.
        let fg = |i: usize| lines[i].spans[0].style.fg;
        assert_eq!(fg(2), Some(danger), "a failure keeps the danger colour");
        assert_eq!(fg(3), Some(dim), "a note takes the informational one");
        // The summary above them is the direction-aware sentence, not a kind-only
        // one that would claim a move had just happened.
        assert!(lines[0].spans[0].content.contains("undid the move"));
    }

    /// The popup's own heading has to agree with its body: an undo is titled as an
    /// undo, and a report with nothing failed is not headed `0 steps failed`.
    #[test]
    fn the_report_title_names_the_direction_and_counts_what_it_has() {
        let undone = report(
            fileop::Kind::Trash,
            fileop::Direction::Undo,
            &[],
            &["/a.txt went to the system trash"],
        );
        assert_eq!(report_title(&undone), "Undo trashing · 1 note");
        assert_eq!(report_count(&undone), "1 note");

        // A forward run keeps its old title word for word.
        let failed = report(
            fileop::Kind::Trash,
            fileop::Direction::Forward,
            &["no trash here", "nor here"],
            &[],
        );
        assert_eq!(report_title(&failed), "Move to trash · 2 steps failed");
        // With both, the failures are what the title sizes: they are what the user
        // needs the count of.
        let both = report(
            fileop::Kind::Move,
            fileop::Direction::Undo,
            &["cannot put /a.txt back"],
            &["/b.txt went to the system trash"],
        );
        assert_eq!(report_title(&both), "Undo move · 1 step failed");
    }

    /// Where the cursor lands after an undo: on the entry that came back, when the
    /// journal names exactly one, and otherwise nowhere in particular.
    #[test]
    fn an_undo_lands_the_cursor_only_on_something_it_restored() {
        // The case that matters: an undone rename puts the old name back, and the
        // name the cursor is sitting on is the one the undo just abolished.
        assert_eq!(
            undo_landing(&[fileop::Undoable::Moved {
                from: PathBuf::from("/here/before.txt"),
                to: PathBuf::from("/here/after.txt"),
            }]),
            Some("before.txt".to_string())
        );
        // A copy or a create being taken away leaves nothing to select.
        assert_eq!(
            undo_landing(&[fileop::Undoable::Created {
                path: PathBuf::from("/here/made.txt"),
            }]),
            None
        );
        // A trashed path is not restored in process at all (ADR 0017 D8).
        assert_eq!(
            undo_landing(&[fileop::Undoable::Trashed {
                path: PathBuf::from("/here/gone.txt"),
            }]),
            None
        );
        // Several steps have no single answer, exactly as `landing_name` has none
        // for a multi-step paste.
        assert_eq!(
            undo_landing(&[
                fileop::Undoable::Moved {
                    from: PathBuf::from("/a"),
                    to: PathBuf::from("/b"),
                },
                fileop::Undoable::Moved {
                    from: PathBuf::from("/c"),
                    to: PathBuf::from("/d"),
                },
            ]),
            None
        );
        assert_eq!(undo_landing(&[]), None);
    }

    /// `Y` sends absolute paths, one per line, and says only what sucher actually
    /// did with them (ADR 0017 D4).
    #[test]
    fn a_yank_sends_absolute_lines_and_claims_nothing_about_the_clipboard() {
        let base = Path::new("/home/j/src");
        assert_eq!(
            clipboard_text(base, &[PathBuf::from("/a/one.txt")]),
            "/a/one.txt"
        );
        // Several marks arrive as several lines, which is what a paste target on
        // the far end reads as a list rather than as one impossible filename.
        assert_eq!(
            clipboard_text(
                base,
                &[PathBuf::from("/a/one.txt"), PathBuf::from("/b/two.txt")]
            ),
            "/a/one.txt\n/b/two.txt"
        );
        // A relative path is anchored rather than sent as it stands: the promise is
        // an absolute path, and half of one pastes into the wrong directory.
        assert_eq!(
            clipboard_text(base, &[PathBuf::from("rel.txt")]),
            "/home/j/src/rel.txt"
        );
        assert_eq!(clipboard_text(base, &[]), "");

        // OSC 52 is fire-and-forget, so the message reports what was sent and who
        // decides what becomes of it. Anything claiming the clipboard changed would
        // be a promise `copy_to_clipboard`'s `Ok` does not make.
        let one = yank_status(1);
        assert_eq!(
            one,
            "sent 1 path to the terminal via OSC 52 · the terminal decides whether the clipboard takes it"
        );
        assert!(yank_status(3).starts_with("sent 3 paths"));
        assert!(
            !one.contains("copied") && !one.contains("clipboard now"),
            "the status must not claim the clipboard changed: {one}"
        );
    }

    /// A truncated list must never read as a complete one (ADR 0017 D5): the row
    /// that says how many are missing comes out of the same budget.
    #[test]
    fn fit_rows_spends_a_row_saying_what_it_is_not_showing() {
        // Everything fits: nothing is held back and no note is needed.
        assert_eq!(fit_rows(0, 10), (0, 0));
        assert_eq!(fit_rows(8, 10), (8, 0));
        assert_eq!(fit_rows(10, 10), (10, 0));
        // One too many: the last row becomes the note, so 9 of 11 are drawn and
        // the overlay says "and 2 more" rather than silently stopping at 10.
        assert_eq!(fit_rows(11, 10), (9, 2));
        // The "showing 8 of 41" case: 8 rows plus the note fill the 9 available.
        assert_eq!(fit_rows(41, 9), (8, 33));
        // A single row of space is spent entirely on the honest count, because a
        // lone first entry with no way to say more follow would be the lie.
        assert_eq!(fit_rows(41, 1), (0, 41));
        // No space at all: nothing is shown and everything is reported hidden.
        assert_eq!(fit_rows(41, 0), (0, 41));
        assert_eq!(fit_rows(0, 0), (0, 0));
        // The two counts always account for the whole list, and the rows actually
        // drawn (entries plus the note) never exceed the room they were given.
        for total in 0..20 {
            for room in 0..20 {
                let (shown, hidden) = fit_rows(total, room);
                assert_eq!(shown + hidden, total, "{total} in {room}");
                if room > 0 {
                    assert!(shown + usize::from(hidden > 0) <= room, "{total} in {room}");
                }
            }
        }
    }

    /// A step reads as "what it is now, then what it becomes", except where there
    /// is nothing on one side of the arrow to name.
    #[test]
    fn step_lines_name_both_ends_only_when_they_differ() {
        // Trash has no destination path (ADR 0017 D7), so it says so in words.
        assert_eq!(
            step_line(&trash_step("/here/notes.md", 1, 12), fileop::Kind::Trash),
            "notes.md  →  trash"
        );
        // A copy that collided shows the suffixed name it will actually land as,
        // which is the whole point of showing the plan first (D5).
        let mut copied = trash_step("/src/a.txt", 1, 3);
        copied.dest = PathBuf::from("/dst/a (2).txt");
        copied.renamed = true;
        assert_eq!(
            step_line(&copied, fileop::Kind::Copy),
            "a.txt  →  a (2).txt"
        );
        // A copy that did not collide names one side only.
        let mut plain = trash_step("/src/a.txt", 1, 3);
        plain.dest = PathBuf::from("/dst/a.txt");
        assert_eq!(step_line(&plain, fileop::Kind::Move), "a.txt");
        // A create has no source at all.
        let mut made = trash_step("", 1, 0);
        made.dest = PathBuf::from("/dst/new.md");
        assert_eq!(step_line(&made, fileop::Kind::Create), "new.md");
    }

    /// The two operation status lines, which are the only place a run reports
    /// itself when nothing failed.
    #[test]
    fn operation_status_lines_read_as_sentences() {
        let forward = fileop::Direction::Forward;
        // ADR 0017 D8: trash points at the system trash rather than implying `U`
        // will bring it back.
        assert_eq!(
            op_done_status(fileop::Kind::Trash, forward, 3, 2048),
            "trashed 3 items, 2.0K · restore from the system trash"
        );
        // One item is singular, and a run with no payload bytes drops the size
        // clause instead of printing a `0 B` that says nothing.
        assert_eq!(
            op_done_status(fileop::Kind::Create, forward, 1, 0),
            "created 1 item"
        );
        assert_eq!(
            op_done_status(fileop::Kind::Copy, forward, 2, 1024),
            "copied 2 items, 1.0K"
        );

        // Progress: the label is the plan's own summary, so the line names the
        // operation the user authorised word for word.
        assert_eq!(
            op_progress_status("trash 3 items, 2.0K", 2, 3, Path::new("/a/b/notes.md")),
            "trash 3 items, 2.0K · 2/3 · notes.md"
        );
        // Before the first progress message there is no current path, and a
        // dangling separator would be worse than none.
        assert_eq!(
            op_progress_status("trash 3 items", 0, 3, Path::new("")),
            "trash 3 items · 0/3"
        );
        assert_eq!(fail_count(1), "1 step failed");
        assert_eq!(fail_count(4), "4 steps failed");
        assert_eq!(mark_count(1), "1 mark");
        assert_eq!(mark_count(2), "2 marks");
        assert_eq!(note_count(1), "1 note");
        assert_eq!(note_count(2), "2 notes");
    }

    /// After an operation reloads the listing the cursor keeps its name where it
    /// survived, and otherwise keeps its row rather than snapping to the top.
    #[test]
    fn reselect_keeps_the_name_then_the_row() {
        let after = ["a.txt", "b.txt", "c.txt"];
        // The name survived, wherever it moved to.
        assert_eq!(reselect(&after, Some("c.txt"), Some(0)), Some(2));
        // The name is gone (it was just trashed): the same row is kept, which is
        // the entry that moved up into its place.
        assert_eq!(reselect(&after, Some("gone.txt"), Some(1)), Some(1));
        // The row is clamped when the listing got shorter, so a delete at the
        // bottom lands on the new last row rather than nowhere.
        assert_eq!(reselect(&after, Some("gone.txt"), Some(9)), Some(2));
        // No previous selection behaves as row 0.
        assert_eq!(reselect(&after, None, None), Some(0));
        // Everything in the folder went: there is nothing to select.
        assert_eq!(reselect(&[], Some("a.txt"), Some(0)), None);
    }

    /// Every clipboard key must be a BOUND char, so typeahead treats it as a
    /// motion and never as the first letter of a name search (ADR 0002 D2, ADR
    /// 0017 D4). `p` is the one that would bite hardest: unbound, it would jump to
    /// `Pictures/` instead of pasting.
    #[test]
    fn clipboard_keys_are_bound_chars_so_typeahead_passes_them_through() {
        assert!(matches!(browse_char('y'), Some(CharAction::Yank)));
        assert!(matches!(browse_char('X'), Some(CharAction::Cut)));
        assert!(matches!(browse_char('p'), Some(CharAction::Paste)));
        for c in ['y', 'X', 'p'] {
            assert!(matches!(
                typeahead::action(false, browse_char(c).is_some()),
                typeahead::Action::PassThrough
            ));
        }
        // The lowercase native-open motion is untouched: `x` and `X` are two
        // distinct bindings, not one case-folded one.
        assert!(matches!(browse_char('x'), Some(CharAction::OpenExternal)));
    }

    /// A cut and a yank differ only in which transfer they ask the planner for.
    #[test]
    fn a_cut_pastes_as_a_move_and_a_yank_as_a_copy() {
        assert_eq!(Transfer::of(true), Transfer::Move);
        assert_eq!(Transfer::of(false), Transfer::Copy);
        let sources = vec![plan_source("/src/a.txt")];
        assert!(matches!(
            Transfer::Copy.op(sources.clone(), PathBuf::from("/dst")),
            fileop::Op::Copy { .. }
        ));
        assert!(matches!(
            Transfer::Move.op(sources, PathBuf::from("/dst")),
            fileop::Op::Move { .. }
        ));
    }

    /// A source as `collect` would return it for a plain one-byte file. Built by
    /// hand so the re-plan is exercised without a filesystem.
    fn plan_source(path: &str) -> fileop::Source {
        fileop::Source {
            path: PathBuf::from(path),
            kind: fileop::NodeKind::File,
            nodes: Vec::new(),
            items: 1,
            bytes: 1,
        }
    }

    /// The overwrite toggle is a PURE recomputation over the inputs the overlay
    /// kept (ADR 0017 D5): the same batch, the same destination listing, the other
    /// policy. Nothing is walked again, which is why this test needs no temp dir.
    #[test]
    fn replanning_reuses_the_collected_sources_under_the_other_policy() {
        let inputs = Replan {
            transfer: Transfer::Copy,
            sources: vec![plan_source("/src/a.txt")],
            dest: PathBuf::from("/dst"),
            dest_listing: vec!["a.txt".to_string()],
            missing: vec![PathBuf::from("/src/gone.txt")],
            cwd: PathBuf::from("/dst"),
        };

        // The default: the collision is dodged with a suffix and nothing is lost.
        let suffixed = inputs.plan(fileop::Conflict::Rename).expect("plans");
        assert_eq!(suffixed.steps[0].dest, PathBuf::from("/dst/a (2).txt"));
        assert_eq!(suffixed.overwrites(), 0);
        assert_eq!(suffixed.policy, fileop::Conflict::Rename);

        // The toggle: the same name, now flagged as displacing what is there.
        let over = inputs
            .plan(flip_policy(suffixed.policy))
            .expect("plans either way");
        assert_eq!(over.steps[0].dest, PathBuf::from("/dst/a.txt"));
        assert_eq!(over.overwrites(), 1);
        assert_eq!(over.policy, fileop::Conflict::Overwrite);
        // Vanished entries ride along whichever policy is in force (D3).
        assert_eq!(over.missing, vec![PathBuf::from("/src/gone.txt")]);

        // And back again with the same key, since `o` toggles rather than sets.
        assert_eq!(
            flip_policy(fileop::Conflict::Overwrite),
            fileop::Conflict::Rename
        );
        assert_eq!(
            inputs.plan(flip_policy(over.policy)).expect("plans").steps[0].dest,
            PathBuf::from("/dst/a (2).txt")
        );
    }

    /// Only the outcome that costs an existing file its place is drawn in the
    /// danger colour, and the overwrite policy always says something so the `o`
    /// key can never look broken.
    #[test]
    fn only_a_real_overwrite_earns_the_danger_colour() {
        // Suffixing, nothing collided: nothing to say at all.
        assert!(collision_lines(fileop::Conflict::Rename, 0, 0).is_empty());
        // Suffixing with collisions: one calm line.
        assert_eq!(
            collision_lines(fileop::Conflict::Rename, 2, 0),
            vec![("2 suffixed to avoid a collision".to_string(), false)]
        );
        // Overwriting something: the danger line names the count and where the
        // displaced entry goes, which ADR 0017 D7 makes a promise rather than a
        // detail.
        assert_eq!(
            collision_lines(fileop::Conflict::Overwrite, 0, 1),
            vec![(
                "1 existing entry replaced, each trashed first".to_string(),
                true
            )]
        );
        assert_eq!(
            collision_lines(fileop::Conflict::Overwrite, 0, 3)[0].0,
            "3 existing entries replaced, each trashed first"
        );
        // Overwriting with nothing to overwrite still reports the policy, calmly,
        // so a press of `o` that changed no count still visibly landed.
        assert_eq!(
            collision_lines(fileop::Conflict::Overwrite, 0, 0),
            vec![(
                "overwrite is on, but nothing here collides".to_string(),
                false
            )]
        );
        // Both at once: a batch-internal collision is suffixed even while
        // overwriting is on, so both lines appear, calm one first.
        let both = collision_lines(fileop::Conflict::Overwrite, 1, 1);
        assert_eq!(both.len(), 2);
        assert!(!both[0].1);
        assert!(both[1].1);
    }

    /// The overlay names `o` only where it does something. Trash has no
    /// destination and so no collision to resolve; a key advertised there would
    /// sit inert, which is worse than one that is not mentioned.
    #[test]
    fn the_overwrite_toggle_is_advertised_only_where_it_applies() {
        assert!(confirm_keys(true, false).contains("[o] overwrite"));
        assert!(confirm_keys(true, true).contains("[o] overwrite"));
        assert!(!confirm_keys(false, false).contains("[o]"));
        assert!(!confirm_keys(false, true).contains("[o]"));
        // The status line has room to spell the single-letter answers; the popup
        // title does not, and says so by leaving them out.
        assert!(confirm_keys(true, true).contains("[y] run"));
        assert!(!confirm_keys(true, false).contains("[y]"));
        for keys in [
            confirm_keys(true, true),
            confirm_keys(true, false),
            confirm_keys(false, true),
            confirm_keys(false, false),
        ] {
            assert!(keys.contains("[Enter]"), "{keys}");
            assert!(keys.contains("[Esc]"), "{keys}");
        }
    }

    /// The clipboard line names what is on it and the one key that answers it.
    #[test]
    fn clip_status_names_the_operation_and_the_key() {
        assert_eq!(
            clip_status(false, 3),
            "clipboard: 3 items to copy · [p] paste here"
        );
        assert_eq!(
            clip_status(true, 1),
            "clipboard: 1 item to move · [p] paste here"
        );
    }

    /// A copy leaves its sources where they were, so the clipboard is still worth
    /// something afterwards; a cut does not, so it is dropped (ADR 0017 D4).
    #[test]
    fn the_clipboard_survives_a_copy_and_goes_after_a_cut() {
        assert!(clip_survives(fileop::Kind::Copy));
        assert!(!clip_survives(fileop::Kind::Move));
        // The operations that never consumed the clipboard leave it alone: a
        // trash of unrelated files must not silently empty a loaded clipboard.
        assert!(clip_survives(fileop::Kind::Trash));
        assert!(clip_survives(fileop::Kind::Rename));
        assert!(clip_survives(fileop::Kind::Create));
    }

    /// `Esc` backs out exactly one layer per press, outermost first. The ordering
    /// is the whole of the decision, so every rung is pinned here rather than
    /// eyeballed: a running operation, then the clipboard, then the marks, then
    /// the browser itself.
    #[test]
    fn the_escape_ladder_backs_out_one_layer_per_press() {
        // A run in flight outranks everything, because quitting or clearing
        // underneath it would leave a half-done mutation unreported (ADR 0009).
        assert_eq!(escape(true, true, true), Escape::CancelOp);
        assert_eq!(escape(true, false, false), Escape::CancelOp);
        assert_eq!(escape(true, true, false), Escape::CancelOp);
        assert_eq!(escape(true, false, true), Escape::CancelOp);
        // Nothing running: the clipboard is the most recent thing loaded, so it
        // goes before the marks that fed it.
        assert_eq!(escape(false, true, true), Escape::ClearClip);
        assert_eq!(escape(false, true, false), Escape::ClearClip);
        // Only marks left: the pre-clipboard behaviour, unchanged.
        assert_eq!(escape(false, false, true), Escape::ClearMarks);
        // Nothing held at all: `Esc` still quits, exactly as it always did.
        assert_eq!(escape(false, false, false), Escape::Quit);

        // Pressing it repeatedly walks the ladder down one rung at a time and
        // reaches the quit exactly once, never sooner.
        let (mut running, mut clip, mut marks) = (true, true, true);
        let mut seen = Vec::new();
        for _ in 0..4 {
            let step = escape(running, clip, marks);
            seen.push(step);
            match step {
                Escape::CancelOp => running = false,
                Escape::ClearClip => clip = false,
                Escape::ClearMarks => marks = false,
                Escape::Quit => {}
            }
        }
        assert_eq!(
            seen,
            vec![
                Escape::CancelOp,
                Escape::ClearClip,
                Escape::ClearMarks,
                Escape::Quit
            ]
        );
    }

    /// The typed-name keys must be BOUND chars, so typeahead treats them as
    /// motions and never as the first letter of a name search (ADR 0002 D2, ADR
    /// 0017 D4). Unbound, `r` would jump to `README.md` instead of renaming it.
    #[test]
    fn rename_and_create_are_bound_chars_so_typeahead_passes_them_through() {
        assert!(matches!(browse_char('r'), Some(CharAction::Rename)));
        assert!(matches!(browse_char('a'), Some(CharAction::Create)));
        for c in ['r', 'a'] {
            assert!(matches!(
                typeahead::action(false, browse_char(c).is_some()),
                typeahead::Action::PassThrough
            ));
        }
        // The two prompts sit one key apart and their label is the only thing on
        // screen that says which one is open, so it has to differ.
        let rename = Ask::Rename {
            path: PathBuf::from("/d/a.txt"),
        };
        let create = Ask::Create {
            parent: PathBuf::from("/d"),
        };
        assert_ne!(rename.prefix(), create.prefix());
        // The create hint is the ONLY place the trailing-slash rule can be
        // learned, so it has to carry it (ADR 0017 D4).
        assert!(create.hint().contains('/'), "{}", create.hint());
        // Both name the keys that answer them.
        for ask in [&rename, &create] {
            assert!(ask.hint().contains("[Enter]"), "{}", ask.hint());
            assert!(ask.hint().contains("[Esc]"), "{}", ask.hint());
        }
    }

    /// The rename prompt opens with the cursor at the end of the stem, so the
    /// first key typed replaces the name and keeps the extension. The rule must
    /// agree with the planner's collision suffixing, which splits on the LAST
    /// dot, or the browser would teach two contradictory ideas of "extension".
    #[test]
    fn the_rename_cursor_lands_at_the_end_of_the_stem() {
        // The ordinary case: typing replaces `notes` and keeps `.md`.
        assert_eq!(stem_end("notes.md", false), 5);
        // A dotfile is all stem: the leading dot is not a separator, the same
        // reading `fileop`'s `suffixed` uses for `.gitignore (2)`.
        assert_eq!(stem_end(".gitignore", false), 10);
        // Only the LAST extension is protected, which agrees with the planner's
        // `foo.tar (2).gz`.
        assert_eq!(stem_end("foo.tar.gz", false), 7);
        // A directory has no extension to protect, so the whole name is stem and
        // `v1.2` is never cut into `v1` and `2`.
        assert_eq!(stem_end("v1.2", true), 4);
        // The same name as a FILE does split, so the flag is really doing work.
        assert_eq!(stem_end("v1.2", false), 2);
        // A trailing dot introduces no extension.
        assert_eq!(stem_end("odd.", false), 4);
        // No dot at all: the cursor is simply at the end.
        assert_eq!(stem_end("README", false), 6);
        assert_eq!(stem_end("", false), 0);
        // Counted in CHARACTERS, not bytes: `café` is four characters and five
        // bytes, and a byte index here would park the cursor inside the `é`.
        assert_eq!(stem_end("café.txt", false), 4);
        assert_eq!(stem_end("🎉.png", false), 1);
    }

    /// A resolved step with a real destination, as rename, create and paste all
    /// produce. Built by hand so the landing rule is decided without a filesystem.
    fn dest_step(src: &str, dest: &str) -> fileop::Step {
        fileop::Step {
            src: PathBuf::from(src),
            dest: PathBuf::from(dest),
            kind: fileop::NodeKind::File,
            nodes: Vec::new(),
            items: 1,
            bytes: 0,
            renamed: false,
            overwrite: false,
        }
    }

    /// Where the cursor lands after an operation. A rename is the case that
    /// forced this: the name that was under the cursor no longer exists, so
    /// `reselect` alone would fall back to the row and leave the cursor beside
    /// the file the user just renamed rather than on it.
    #[test]
    fn the_landing_name_comes_only_from_a_single_step_with_a_destination() {
        assert_eq!(
            landing_name(&[dest_step("/d/old.txt", "/d/new.txt")]),
            Some("new.txt".to_string())
        );
        // A create has no source at all and still names what it made.
        assert_eq!(
            landing_name(&[dest_step("", "/d/notes.md")]),
            Some("notes.md".to_string())
        );
        // A multi-step paste brought several entries in at once and has no single
        // answer, so the pre-existing rule stands: keep the name under the cursor.
        assert_eq!(
            landing_name(&[dest_step("/a/one", "/d/one"), dest_step("/b/two", "/d/two")]),
            None
        );
        // Trash has no destination path to land on (ADR 0017 D7), even as one
        // step, so the row rule keeps covering a delete.
        assert_eq!(landing_name(&[trash_step("/d/gone.txt", 1, 0)]), None);
        assert_eq!(landing_name(&[]), None);

        // And it composes with `reselect` exactly as `finish_op` uses it: the new
        // name wins over the old one, which is gone from the reloaded listing.
        let after = ["a.txt", "new.txt", "z.txt"];
        let landing = landing_name(&[dest_step("/d/old.txt", "/d/new.txt")]);
        assert_eq!(reselect(&after, landing.as_deref(), Some(0)), Some(1));
    }

    /// The prompt line must SHOW the cursor: a field that opens pre-filled with
    /// the cursor parked mid-name is useless if the user cannot see where it is.
    #[test]
    fn the_prompt_line_shows_the_cursor_wherever_it_sits() {
        // Mid-name: the character under the cursor is its own span, so it can be
        // drawn reversed, and the text either side of it is intact.
        let edit = crate::lineedit::LineEdit::with_text("notes.md", 5);
        let spans = prompt_spans("rename: ", &edit, "[Esc] cancel");
        assert_eq!(spans_text(&spans), " rename: notes.md    [Esc] cancel");
        assert_eq!(spans[1].content, ".");

        // At the end of the line there is no character to reverse, so the cursor
        // is a block. Without this the cursor would be invisible in exactly the
        // place a fresh create prompt always puts it.
        let edit = crate::lineedit::LineEdit::with_text("notes.md", 8);
        let spans = prompt_spans("rename: ", &edit, "go");
        assert_eq!(spans_text(&spans), " rename: notes.md█    go");
        assert_eq!(spans[1].content, "█");

        // The empty create prompt is that same case, and is nothing but a cursor.
        let empty = crate::lineedit::LineEdit::new();
        assert_eq!(
            spans_text(&prompt_spans("new: ", &empty, "go")),
            " new: █    go"
        );

        // A multi-byte name splits on the character, never inside it, so the line
        // renders whole rather than as two broken halves.
        let edit = crate::lineedit::LineEdit::with_text("café.txt", 3);
        let spans = prompt_spans("rename: ", &edit, "go");
        assert_eq!(spans[1].content, "é");
        assert_eq!(spans_text(&spans), " rename: café.txt    go");
    }

    #[test]
    fn auto_behaves_as_miller_gated_on_width() {
        // Wide + parent → Miller's three panes.
        assert_eq!(effective_columns(Layout::Auto, 200, true), 3);
        // Narrow → the friendly two-column collapse.
        assert_eq!(effective_columns(Layout::Auto, 80, true), 2);
        // Wide but no parent → two.
        assert_eq!(effective_columns(Layout::Auto, 200, false), 2);
    }
}
