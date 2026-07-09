// Directory browser: a fast, two-pane file navigator. Left pane is the entry
// list for the current directory; right pane previews the selection (child
// listing for folders, head of the file for text, dimensions for images,
// metadata otherwise). Enter opens a file in its viewer and returns here.

use crate::config::{IconMode, Layout};
use crate::format::Format;
use crate::git::{self, GitStatus};
use crate::media::{self, ImagePane};
use crate::{highlight, icons, query, theme, typeahead};
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

/// The lower-cased extension of a file name, or `""` when it has none. Pure —
/// unit-tested. Used only by [`SortKey::Ext`]; kept a free fn so the comparator
/// and its tests share one definition.
fn name_ext(name: &str) -> String {
    match name.rsplit_once('.') {
        // A leading-dot name (`.gitignore`) is all "stem", no extension.
        Some((stem, ext)) if !stem.is_empty() => ext.to_lowercase(),
        _ => String::new(),
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
    let by_name = || a.sort_name().to_lowercase().cmp(&b.sort_name().to_lowercase());
    let ord = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Size => a.sort_size().cmp(&b.sort_size()).then_with(by_name),
        SortKey::Modified => a.sort_modified().cmp(&b.sort_modified()).then_with(by_name),
        SortKey::Ext => name_ext(a.sort_name())
            .cmp(&name_ext(b.sort_name()))
            .then_with(by_name),
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
    all: Vec<Entry>,
    view: Vec<usize>, // indices into `all` matching the filter
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
        all: Vec::new(),
        view: Vec::new(),
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
        // Refresh the git gutter for the new directory (cheap, correct after a
        // dir change — D2). Disabled, git-absent, or non-repo dirs yield `None`,
        // which the pane renderer treats as "no gutter" (byte-for-byte pre-git).
        self.git = if self.git_enabled {
            git::status_map(&self.cwd)
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
        self.refilter();
        self.status = Some(self.sort.label());
    }

    fn selected(&self) -> Option<&Entry> {
        let i = self.state.selected()?;
        self.all.get(*self.view.get(i)?)
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
        };
        let items = entry_items(area, &view, self.icons);
        let list = entry_list(items, None);
        let mut buf = Buffer::empty(inner);
        // A copy of the live state carries the exact scroll offset onto the
        // snapshot without disturbing the real one (`ListState` is `Copy`).
        let mut state = self.state;
        state.select(view.selected);
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
        // position rather than tacking onto the end.
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
            } else if raster_active || animating || searching {
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
            KeyCode::Esc => return Some(Action::Quit),
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
                                    image::ImageReader::open(&p)
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
                Format::Image => image::image_dimensions(&path)
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
            Format::Sheet => self.preview_sheet(&path),
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
        let entries = read_entries(&parent, self.sort);
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
            entries: &entries,
            order: &order,
            selected,
            title: format!(" {} ", pretty_dir_name(&parent)),
            git: None,
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
        } else if let Some(s) = &self.status {
            format!(" {s}")
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
        let color = if let Mode::Filter = self.mode {
            theme::palette().doc
        } else {
            theme::palette().dim
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
        let query = self
            .search
            .as_ref()
            .map(|s| s.query.as_str())
            .unwrap_or("");
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
        // Build items first (borrows `&self`), then render through the state (borrows
        // `&mut self.search`) — sequential, so no aliasing.
        let items = self.search_items(area.width);
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
            render_items_into(f.buffer_mut(), inner, list, &mut search.state);
        }
    }

    /// Build the result rows as `ListItem`s (ADR 0007 D5). Each row is drawn
    /// SPECIALISED — the kind glyph (same icon/colour convention as `entry_items`),
    /// the hit's path RELATIVE to cwd (coloured by `hit.kind.color()`), and for a
    /// content match a dimmed ` N: text` snippet. The whole line is length-budgeted
    /// to the inner width so a row never wraps (reusing `truncate`/`snippet_suffix`).
    fn search_items(&self, width: u16) -> Vec<ListItem<'static>> {
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
        search
            .results
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
    let items = entry_items(area, view, icons);
    let list = entry_list(items, view.fade_t);
    // Sync the passed state to the view's selection (a no-op for the current
    // pane, whose state already holds it — so its scroll offset is untouched).
    state.select(view.selected);
    render_items_into(f.buffer_mut(), inner, list, state);
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
    let items = entry_items(area, view, icons);
    let list = entry_list(items, view.fade_t);
    let mut new_buf = Buffer::empty(inner);
    let mut st = *state; // `ListState` is `Copy`; don't disturb the caller's.
    st.select(view.selected);
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

/// Build a pane's rows as `ListItem`s (ADR 0004 D1): icon + optional git gutter +
/// name + optional trailing meta column, each colour passed through the fade tint
/// from `view.fade_t`. `area` is the OUTER pane rect — its width drives the exact
/// same chrome/name arithmetic as before the block/items split, so the produced
/// items are byte-for-byte identical to the pre-refactor renderer.
///
/// - `view.git` reserves a 2-cell gutter only when `Some` (the current pane in a
///   repo); when `None` (parent pane, non-repo, or git off) it costs zero width
///   and the name reclaims it — so two-column output is byte-for-byte pre-git.
fn entry_items(area: Rect, view: &EntryListView, icons: IconMode) -> Vec<ListItem<'static>> {
    let fade = |c: Color| fade_color(view.fade_t, c);
    // Width reserved before the name: 2 for the block borders, 2 for the
    // selection cursor gutter (the `highlight_symbol` List reserves on every row
    // so names align whether selected or not), plus a 2-cell glyph column ("X ")
    // in the glyphed modes. `IconMode::None` drops the glyph, so the name
    // reclaims those two cells (D5).
    let chrome_w = match icons {
        IconMode::None => 4, // borders + gutter
        _ => 6,              // borders + gutter + glyph cell
    };
    // The git gutter is a 2-cell slot drawn only when a git map is present; when
    // absent (parent pane, non-repo, git off) it costs nothing and the name
    // reclaims it, keeping the pre-git layout byte-for-byte.
    let git_w: u16 = if view.git.is_some() { 2 } else { 0 };
    let inner_w = area.width.saturating_sub(chrome_w + git_w) as usize;
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

    view.order
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
            let mut spans: Vec<Span> = Vec::with_capacity(4);
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
        row("type…", "jump to a name (typeahead)"),
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
        assert_eq!(name_ext("photo.JPG"), "jpg"); // lower-cased
        assert_eq!(name_ext("archive.tar.gz"), "gz"); // last component only
        assert_eq!(name_ext("README"), ""); // none
        assert_eq!(name_ext(".gitignore"), ""); // leading dot = stem, not ext
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
        let mut v = vec![hit("zzz.txt"), hit("src/b.rs"), hit("src/a.rs"), hit("readme.md")];
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
