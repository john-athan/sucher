// Directory browser: a fast, two-pane file navigator. Left pane is the entry
// list for the current directory; right pane previews the selection (child
// listing for folders, head of the file for text, dimensions for images,
// metadata otherwise). Enter opens a file in its viewer and returns here.

use crate::config::{IconMode, Layout};
use crate::format::Format;
use crate::git::{self, GitStatus};
use crate::media::{self, ImagePane};
use crate::{highlight, icons, query, theme, typeahead};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
// The ratatui layout builder is aliased to `RtLayout` so `Layout` can name the
// browser's own pane-layout mode (auto/miller/double) from `config`.
use ratatui::layout::{Constraint, Direction, Layout as RtLayout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::fs;
use std::io::{self, Read};
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
    // Braille-spinner frame counter for the `Loading` preview (ADR 0004 D3). It
    // advances ONLY while a raster is live (pending or wanted) — never on an idle
    // redraw — so the spinner animates during real work without the fully-idle
    // browser ever churning the CPU. See the tick in `main_loop`.
    spin: usize,
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
    ToggleHidden,
    ToggleLayout,
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
        '.' => CharAction::ToggleHidden,
        // Cycle the pane layout (auto→miller→double→auto). Binding `M` here also
        // keeps typeahead correct: a bound char never starts a name search.
        'M' => CharAction::ToggleLayout,
        'q' => CharAction::Quit,
        _ => return None,
    })
}

pub fn run(start: String, icons: IconMode, layout: Layout, git_enabled: bool) -> io::Result<()> {
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
        spin: 0,
    };
    app.load();

    loop {
        let mut term = ratatui::init();
        let action = app.main_loop(&mut term);
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

impl App {
    /// Read the current directory into `all`, then apply the filter.
    fn load(&mut self) {
        self.all = read_entries(&self.cwd);
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

    fn enter_dir(&mut self, path: PathBuf) {
        self.cwd = path;
        self.filter.clear();
        self.mode = Mode::Browse;
        self.state.select(Some(0));
        self.status = None;
        self.typeahead.clear();
        self.typeahead_at = None;
        self.load();
    }

    fn go_parent(&mut self) {
        if let Some(parent) = self.cwd.parent().map(Path::to_path_buf) {
            let from = self
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            self.enter_dir(parent);
            // Land on the directory we came out of.
            if let Some(name) = from {
                if let Some(pos) = self.view.iter().position(|&i| self.all[i].name == name) {
                    self.state.select(Some(pos));
                }
            }
        }
    }

    fn activate(&mut self) -> Option<Action> {
        let e = self.selected()?;
        if e.kind == Format::Directory {
            let p = e.path.clone();
            self.enter_dir(p);
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

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<Action> {
        let mut dirty = true;
        loop {
            // Recompute the preview when the selection changed.
            let cur = self.selected().map(|e| e.path.clone());
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
            let timeout = if raster_active || animating {
                Duration::from_millis(60)
            } else {
                Duration::from_millis(1000)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        if let Some(action) = self.handle_key(key) {
                            return Ok(action);
                        }
                    }
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
            CharAction::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.refilter();
            }
            CharAction::ToggleLayout => self.layout = self.layout.cycle(),
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
        let cur = self.selected().map(|e| e.path.clone());

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
        let Some(e) = self.selected() else { return };
        // Snapshot what we need so `self` is free to mutate below.
        let name = e.name.clone();
        let kind = e.kind;
        let size = e.size;
        let modified = e.modified;
        let path = e.path.clone();

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
                show_size: true,
            };
            render_entry_list(f, cols[1], &view, &mut self.state, true, filter, self.icons);
            self.render_preview(f, cols[2]);
        } else {
            // Double: current | preview — the classic, byte-for-byte split.
            let cols = RtLayout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(rows[1]);
            self.viewport_h = cols[0].height.saturating_sub(2);
            let view = EntryListView {
                entries: &self.all,
                order: &self.view,
                selected: self.state.selected(),
                title: format!(" {} ", self.view.len()),
                git: self.git.as_ref(), // current pane's gutter (D2).
                show_size: true,
            };
            render_entry_list(f, cols[0], &view, &mut self.state, true, filter, self.icons);
            self.render_preview(f, cols[1]);
        }

        self.render_status(f, rows[2]);
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
        let entries = read_entries(&parent);
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
            show_size: false, // drop the size column — cleaner for a context pane.
        };
        render_entry_list(f, area, &view, &mut state, false, false, self.icons);
    }

    fn render_crumb(&self, f: &mut Frame, area: Rect) {
        let shown = pretty_path(&self.cwd);
        // Two-tone breadcrumb: the parent portion recedes in `dim` while the
        // final segment — the directory you're actually in — pops in accent +
        // BOLD, so the current location reads at a glance. Split on the last
        // '/', keeping the separator with the parent (`~/foo/` + `bar`); a
        // path with no slash (e.g. `~`) is all accent.
        let accent = theme::palette().accent;
        let dim = theme::palette().dim;
        let mut spans = vec![Span::raw(" ")];
        match shown.rfind('/') {
            Some(i) => {
                let (parent, last) = shown.split_at(i + 1);
                spans.push(Span::styled(parent.to_string(), Style::default().fg(dim)));
                spans.push(Span::styled(
                    last.to_string(),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ));
            }
            None => spans.push(Span::styled(
                shown,
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            )),
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
            format!(
                " [j/k] move  [Enter/l] open  [h] up  [/] filter  [.] dotfiles ({hidden})  [M] layout  [q] quit"
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
}

/// The smallest frame width (columns) at which Miller opens a third pane. Below
/// this the preview and current panes would be too cramped, so the layout
/// collapses to the classic two-column split (ADR 0004, D1).
const MILLER_MIN: u16 = 100;

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
    /// Whether to draw the right-aligned size column. The current pane keeps it;
    /// the parent context pane drops it for a cleaner, narrower list.
    show_size: bool,
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
/// bordered list of icon + optional git gutter + name + optional size. Both the
/// current pane (`active`, size column, filter-aware border) and the parent
/// context pane (inactive, no size) route through here.
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
    // The current pane reserves the size column (` {size:>8}`); the parent drops
    // it entirely so the name fills the narrow context column.
    let size_reserve = if view.show_size { size_w + 1 } else { 0 };
    let name_w = inner_w.saturating_sub(size_reserve).max(4);

    let items: Vec<ListItem> = view
        .order
        .iter()
        .map(|&i| {
            let e = &view.entries[i];
            let name = truncate(&e.name, name_w);
            let size = if e.kind == Format::Directory {
                String::new()
            } else {
                crate::util::human_size(e.size)
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
                        Style::default().fg(c),
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
                        Style::default().fg(c),
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
                        Style::default().fg(st.color()),
                    )),
                    None => spans.push(Span::raw("  ")),
                }
            }
            spans.push(Span::styled(
                format!("{name:<name_w$}"),
                Style::default().fg(name_color),
            ));
            if view.show_size {
                spans.push(Span::styled(
                    format!(" {size:>size_w$}"),
                    Style::default().fg(theme::palette().dim),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let accent = theme::palette().accent;
    // The active pane lights its border with the accent (and its title
    // accent+BOLD); the passive parent pane dims both so it recedes. In Filter
    // mode the active border shifts to the filter yellow (`doc`), matching the
    // status line's filter colour.
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
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border))
                .title(title),
        )
        // Soft background tint + accent cursor gutter, replacing the harsh
        // reverse-video bar. The gutter bar ("▎") takes the selection bg too;
        // the tint carries the accent read.
        .highlight_style(
            Style::default()
                .bg(theme::palette().selection)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▎ ");
    // Sync the passed state to the view's selection (a no-op for the current
    // pane, whose state already holds it — so its scroll offset is untouched).
    state.select(view.selected);
    f.render_stateful_widget(list, area, state);
}

/// Read a directory's entries, classified by extension and sorted
/// directories-first then case-insensitive by name — the one lister shared by
/// the current pane, the parent pane, and `App::load` (ADR 0004, D1). Pure of
/// app state; the only IO is the `read_dir`. An unreadable directory yields an
/// empty list rather than erroring, matching the browser's forgiving load.
fn read_entries(dir: &Path) -> Vec<Entry> {
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
    // Directories first, then case-insensitive by name.
    entries.sort_by(|a, b| {
        let ad = a.kind == Format::Directory;
        let bd = b.kind == Format::Directory;
        bd.cmp(&ad)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    entries
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

/// Home-relative, `~`-prefixed path for the breadcrumb.
fn pretty_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = s.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    s
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
    fn auto_behaves_as_miller_gated_on_width() {
        // Wide + parent → Miller's three panes.
        assert_eq!(effective_columns(Layout::Auto, 200, true), 3);
        // Narrow → the friendly two-column collapse.
        assert_eq!(effective_columns(Layout::Auto, 80, true), 2);
        // Wide but no parent → two.
        assert_eq!(effective_columns(Layout::Auto, 200, false), 2);
    }
}
