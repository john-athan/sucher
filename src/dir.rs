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
}

/// Which file-operation overlay is on screen.
enum OpView {
    /// A fully resolved plan waiting to be authorised. Shown BEFORE anything
    /// happens, which is the whole of ADR 0017 D5: the user sees the outcome,
    /// including every collision-dodging rename, before a byte moves.
    Confirm(fileop::Plan),
    /// A finished run that did not do everything it set out to. A partial result
    /// is reported, never swallowed (ADR 0009), so this is deliberately an overlay
    /// and not a status line: a one-line summary of four failures would be exactly
    /// the silent truncation that doctrine exists to forbid.
    Failures(fileop::Report),
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
    /// Cumulative counters as last reported by `Msg::Progress`.
    items: usize,
    bytes: u64,
    /// The path the worker was on when it last reported.
    current: PathBuf,
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
    // first, bounded at `UNDO_DEPTH` by `push_journal`. Only ever appended to for
    // now; the `U` binding that pops it lands in a later step, which is why this
    // grows without anything yet reading it.
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

        // Reload so the listing reflects what happened; `load` also refreshes the
        // git gutter and the parent cache, both of which a mutation can invalidate.
        let wanted = self.selected().map(|e| e.name.clone());
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
        self.status = Some(op_done_status(report.kind, report.items, report.bytes));
        if report.failures.is_empty() {
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
        self.show_op(OpView::Failures(report));
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
            Ok(plan) => self.show_op(OpView::Confirm(plan)),
            Err(refusal) => self.status = Some(refusal.to_string()),
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
        self.op_progress = Some(InFlight {
            label,
            total,
            targets,
            items: 0,
            bytes: 0,
            current: PathBuf::new(),
        });
        self.op = Some(fileop::start(plan));
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
                if let Mode::Op(OpView::Confirm(plan)) =
                    std::mem::replace(&mut self.mode, Mode::Browse)
                {
                    self.start_op(plan);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                self.mode = Mode::Browse;
                self.status = Some("cancelled".to_string());
            }
            // `o` (toggle overwrite, re-plan, show the result in the danger
            // colour) belongs here and is deliberately absent: ADR 0017 D5 gives
            // it to paste, and trash has no destination and no collisions, so
            // there is nothing for it to toggle. A key that silently did nothing
            // would be worse than an unbound one.
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
            // `Esc` clears a live selection before it quits (ADR 0017 D4). This is
            // the single guard on the key, and it earns its place: a key that
            // silently quit the browser while a set of marks was held would throw
            // away a selection the user had deliberately gathered across folders,
            // which is worse than the small conditional. With nothing marked the
            // key quits exactly as it always did. The typeahead branch further up
            // still claims `Esc` while a name session is live (ADR 0002 D3), so
            // this arm is only reached when there is no session to cancel.
            KeyCode::Esc => {
                if self.marks.is_empty() {
                    return Some(Action::Quit);
                }
                self.marks.clear();
            }
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
            CharAction::Quit => return Some(Action::Quit),
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
                OpView::Confirm(plan) => {
                    format!(" {}    [Enter]/[y] run  [Esc]/[n] cancel", plan.summary())
                }
                OpView::Failures(report) => {
                    format!(" {}    [any key] close", fail_count(report.failures.len()))
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
            // Failures borrow the same semantic red the overlay uses.
            Mode::Op(OpView::Failures(_)) => theme::palette().pdf,
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
        row("Esc", "clear marks (quits when nothing is marked)"),
        Line::from(""),
        heading(" Act"),
        // Named as trash, not delete: sucher has no permanent-delete binding at
        // all, and the help is where that promise has to be legible (ADR 0017 D7).
        row(
            "D",
            "move to trash  (shows the plan first; never permanent)",
        ),
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
        row("q / Esc", "quit"),
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
        OpView::Confirm(plan) => (
            format!(" {} · [Enter] run  [Esc] cancel ", kind_title(plan.kind)),
            confirm_lines(plan, w, h),
        ),
        OpView::Failures(report) => (
            format!(
                " {} · {} · [any key] close ",
                kind_title(report.kind),
                fail_count(report.failures.len())
            ),
            failure_lines(report, w, h),
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
    let renamed = plan.renamed();
    if renamed > 0 {
        lines.push(styled(
            format!("  {renamed} suffixed to avoid a collision"),
            dim,
        ));
    }
    let overwrites = plan.overwrites();
    if overwrites > 0 {
        // Displacing something is the one thing here that loses a name, so it is
        // the one line drawn in the danger colour.
        lines.push(styled(
            format!("  {overwrites} will be overwritten"),
            danger,
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

/// The failure overlay's body: what the run managed, then what it did not, each
/// with the executor's own one-line reason (ADR 0009). Sized by the same
/// [`fit_rows`] rule as the confirm overlay, so a long failure list says how many
/// it is not showing rather than ending without warning.
fn failure_lines(report: &fileop::Report, w: usize, h: usize) -> Vec<Line<'static>> {
    let accent = theme::palette().accent;
    let dim = theme::palette().dim;
    let danger = theme::palette().pdf;
    let mut lines = vec![
        Line::from(Span::styled(
            truncate(
                &format!(
                    "  {}",
                    op_done_status(report.kind, report.items, report.bytes)
                ),
                w,
            ),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    let (shown, hidden) = fit_rows(report.failures.len(), h.saturating_sub(lines.len()));
    for failure in report.failures.iter().take(shown) {
        lines.push(Line::from(Span::styled(
            truncate(&format!("  {}", failure.msg), w),
            Style::default().fg(danger),
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

/// The past-tense verb for a finished operation.
fn done_verb(kind: fileop::Kind) -> &'static str {
    match kind {
        fileop::Kind::Copy => "copied",
        fileop::Kind::Move => "moved",
        fileop::Kind::Rename => "renamed",
        fileop::Kind::Create => "created",
        fileop::Kind::Trash => "trashed",
    }
}

/// The status line a finished run leaves behind, e.g.
/// `trashed 3 items, 1.2K · restore from the system trash`.
///
/// Sizes go through [`crate::util::human_size`], the formatter the listing's size
/// column uses, so the two never disagree about what a megabyte looks like. Pure,
/// so the wording is unit-tested.
fn op_done_status(kind: fileop::Kind, items: usize, bytes: u64) -> String {
    let noun = if items == 1 { "item" } else { "items" };
    let mut out = format!("{} {items} {noun}", done_verb(kind));
    if bytes > 0 {
        out.push_str(&format!(", {}", crate::util::human_size(bytes)));
    }
    if kind == fileop::Kind::Trash {
        // ADR 0017 D8: sucher does not restore from the trash in process, so the
        // line points at the surface that does instead of implying `U` will. A
        // half-supported undo is worse than an honest pointer to Finder.
        out.push_str(" · restore from the system trash");
    }
    out
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
        // `Ctrl-a` is deliberately NOT a `browse_char` binding: a bare `a` stays
        // free for typeahead, and the handler tests the modifier itself.
        assert!(browse_char('a').is_none());
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
        // ADR 0017 D8: trash points at the system trash rather than implying `U`
        // will bring it back.
        assert_eq!(
            op_done_status(fileop::Kind::Trash, 3, 2048),
            "trashed 3 items, 2.0K · restore from the system trash"
        );
        // One item is singular, and a run with no payload bytes drops the size
        // clause instead of printing a `0 B` that says nothing.
        assert_eq!(op_done_status(fileop::Kind::Create, 1, 0), "created 1 item");
        assert_eq!(
            op_done_status(fileop::Kind::Copy, 2, 1024),
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
