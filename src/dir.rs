// Directory browser: a fast, two-pane file navigator. Left pane is the entry
// list for the current directory; right pane previews the selection (child
// listing for folders, head of the file for text, dimensions for images,
// metadata otherwise). Enter opens a file in its viewer and returns here.

use crate::config::IconMode;
use crate::format::Format;
use crate::media::ImagePane;
use crate::{highlight, query, theme, typeahead};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
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

struct App {
    cwd: PathBuf,
    // The resolved icon mode (ADR 0003, D5). Stored now so the browser can key
    // its glyph column off it; wired but not yet consumed — glyph rendering
    // changes land in a later phase.
    #[allow(dead_code)]
    icons: IconMode,
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
    // `DynamicImage` (Send) back over the channel to the main thread.
    raster_tx: Sender<(PathBuf, Option<DynamicImage>)>,
    raster_rx: Receiver<(PathBuf, Option<DynamicImage>)>,
    raster_pending: Option<PathBuf>, // path in the ONE live worker
    raster_want: Option<(PathBuf, Format)>, // latest selection awaiting a raster
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
        'q' => CharAction::Quit,
        _ => return None,
    })
}

pub fn run(start: String, icons: IconMode) -> io::Result<()> {
    let cwd = fs::canonicalize(&start).unwrap_or_else(|_| PathBuf::from(&start));
    // Probe the graphics protocol once, before any alternate screen. If the
    // terminal can't do pixels, previews fall back to text/metadata.
    let pane = ImagePane::new().ok();
    let (raster_tx, raster_rx) = std::sync::mpsc::channel();
    let mut app = App {
        cwd,
        icons,
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
        self.all.clear();
        if let Ok(rd) = fs::read_dir(&self.cwd) {
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
                self.all.push(Entry {
                    name,
                    path,
                    kind,
                    size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    modified: meta.and_then(|m| m.modified().ok()),
                });
            }
        }
        // Directories first, then case-insensitive by name.
        self.all.sort_by(|a, b| {
            let ad = a.kind == Format::Directory;
            let bd = b.kind == Format::Directory;
            bd.cmp(&ad)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
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
            // Poll briefly while a raster is in flight or queued so a finished
            // image installs promptly; otherwise idle at the normal cadence.
            let timeout = if self.raster_pending.is_some() || self.raster_want.is_some() {
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

    /// Install a decoded image into the pane as the live preview.
    fn show_image(&mut self, img: DynamicImage) {
        if let Some(pane) = self.pane.as_mut() {
            pane.set(img);
        }
        self.pv = Pv::Image;
    }

    /// Drive the single-worker async rasteriser. Runs every main-loop tick:
    /// drains finished posters, retires the in-flight job, and — if the worker
    /// is idle — starts the latest wanted raster (or installs it straight from
    /// cache). Returns true if the preview changed and a redraw is due.
    fn pump_raster(&mut self) -> bool {
        let mut dirty = false;
        let cur = self.selected().map(|e| e.path.clone());

        // 1. Drain completed rasters. Cache every success; only touch the pane
        //    when the finished path is still the current selection.
        while let Ok((path, result)) = self.raster_rx.try_recv() {
            if let Some(img) = result.clone() {
                self.cache_put(path.clone(), img);
            }
            if self.raster_pending.as_deref() == Some(path.as_path()) {
                self.raster_pending = None;
            }
            if Some(&path) != cur.as_ref() {
                continue; // stale: scrolled away — keep it cached, leave the pane
            }
            match result {
                Some(img) => self.show_image(img),
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
                        let img = match kind {
                            Format::Image => image::ImageReader::open(&p)
                                .map_err(|e| e.to_string())
                                .and_then(|r| r.decode().map_err(|e| e.to_string())),
                            Format::Pdf => crate::pdf::poster(&p.to_string_lossy()),
                            Format::Video => crate::video::poster(&p.to_string_lossy()),
                            Format::Svg => crate::svg::render_svg(&p.to_string_lossy()),
                            Format::Keynote => crate::keynote::preview_image(&p.to_string_lossy()),
                            _ => Err(String::new()),
                        };
                        let _ = tx.send((p, img.ok()));
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
        let rows = Layout::default()
            .constraints([
                Constraint::Length(1), // breadcrumb
                Constraint::Min(0),    // body
                Constraint::Length(1), // status
            ])
            .split(area);

        self.render_crumb(f, rows[0]);

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
            .split(rows[1]);
        self.render_list(f, cols[0]);
        self.render_preview(f, cols[1]);

        self.render_status(f, rows[2]);
    }

    fn render_crumb(&self, f: &mut Frame, area: Rect) {
        let shown = pretty_path(&self.cwd);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    shown,
                    Style::default()
                        .fg(theme::palette().accent)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            area,
        );
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect) {
        self.viewport_h = area.height.saturating_sub(2);
        let inner_w = area.width.saturating_sub(4) as usize; // borders + glyph
        let size_w = 8usize;
        let name_w = inner_w.saturating_sub(size_w + 1).max(4);

        let items: Vec<ListItem> = self
            .view
            .iter()
            .map(|&i| {
                let e = &self.all[i];
                let name = truncate(&e.name, name_w);
                let size = if e.kind == Format::Directory {
                    String::new()
                } else {
                    crate::util::human_size(e.size)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", e.kind.glyph()),
                        Style::default().fg(e.kind.color()),
                    ),
                    Span::styled(
                        format!("{name:<name_w$}"),
                        Style::default().fg(e.kind.color()),
                    ),
                    Span::styled(
                        format!(" {size:>size_w$}"),
                        Style::default().fg(theme::palette().dim),
                    ),
                ]))
            })
            .collect();

        let count = self.view.len();
        let title = format!(" {count} ");
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED));
        f.render_stateful_widget(list, area, &mut self.state);
    }

    fn render_preview(&mut self, f: &mut Frame, area: Rect) {
        match self.pv {
            Pv::Image => {
                let title = format!(
                    " {} ",
                    truncate(&self.caption, area.width.saturating_sub(4) as usize)
                );
                let block = Block::default().borders(Borders::ALL).title(title);
                let inner = block.inner(area);
                f.render_widget(block, area);
                if let Some(pane) = self.pane.as_mut() {
                    pane.render(f, inner);
                }
            }
            Pv::Loading => {
                // Placeholder while the background worker rasters. Never render
                // the (possibly stale) pane here — only a caption + dim note.
                let title = format!(
                    " {} ",
                    truncate(&self.caption, area.width.saturating_sub(4) as usize)
                );
                let block = Block::default().borders(Borders::ALL).title(title);
                let inner = block.inner(area);
                f.render_widget(block, area);
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "rendering…",
                        Style::default().fg(theme::palette().dim),
                    ))),
                    inner,
                );
            }
            Pv::Text => {
                let block = Block::default().borders(Borders::ALL).title(" Preview ");
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
                " [j/k] move  [Enter/l] open  [h] up  [/] filter  [.] dotfiles ({hidden})  [q] quit"
            )
        };
        let color = if let Mode::Filter = self.mode {
            Color::Rgb(252, 211, 77)
        } else {
            theme::palette().dim
        };
        f.render_widget(
            Paragraph::new(Line::from(txt)).style(Style::default().fg(color)),
            area,
        );
    }
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
