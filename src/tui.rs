// Interactive terminal UI for navigating a markdown document.
// Scroll, table-of-contents sidebar with jump, in-document search, and a
// link picker that opens URLs in the default browser.

use crate::markdown::{LinkHit, Rendered};
use crate::media::ImagePane;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TOC_W: u16 = 32;

#[derive(PartialEq)]
enum Mode {
    Doc,
    Toc,
    Search,
    Links,
    Help,
    Gallery,
}

pub struct App {
    title: String,
    /// Source file on disk for "open in native app" (`x`) AND for resolving a
    /// relative link path (a `./notes.md` link resolves against this file's
    /// directory, not the process cwd); the doc shown here is a *rendered* form
    /// (docx/html → markdown), so this is the original, not the markdown.
    /// `None` when the source has no file (rendered from stdin): a relative
    /// link then resolves against the process's current directory instead.
    open: Option<String>,
    doc: Rendered,
    display: Vec<Line<'static>>,
    plain: Vec<String>,
    log2disp: Vec<usize>,
    /// One entry per `display` line: the link runs on that line (mirrors
    /// `markdown::Layout::hits`), so a click can be mapped back to a link.
    hits: Vec<Vec<LinkHit>>,
    offset: usize,
    laid_width: u16,
    viewport_h: u16,
    /// The doc pane's content rect (inside the block border, excluding the
    /// status line), set each render so a later mouse click can be mapped back
    /// to a display line/column without recomputing the frame layout.
    content_area: Rect,
    mode: Mode,
    show_toc: bool,
    toc_state: ListState,
    link_state: ListState,
    query: String,
    matches: Vec<usize>,
    match_set: std::collections::HashSet<usize>,
    match_cur: usize,
    // Embedded images (docx/pptx media) viewable in a gallery overlay.
    images: Vec<PathBuf>,
    pane: Option<ImagePane>,
    gallery_idx: usize,
    /// A short-lived status message (e.g. a link that resolved to nothing),
    /// shown in place of the normal status line and cleared on the next
    /// keypress so it never lingers or looks like part of the permanent UI.
    flash: Option<String>,
}

enum Action {
    Quit,
    Open(PathBuf),
}

/// Enables crossterm mouse capture on construction (when `on`) and guarantees
/// its teardown on drop, mirroring `dir::MouseGuard` (ADR 0005 D2). The guard
/// is created right after `ratatui::init()` and dropped right before
/// `ratatui::restore()`, so capture is off on every exit: quit, the
/// open-in-sucher round trip, an error return, or a panic (drop still runs
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

/// Open a markdown document. `images` are embedded rasters (docx/pptx media)
/// browsable in a gallery overlay; pass an empty vec for plain markdown.
pub fn run(
    title: String,
    src: String,
    images: Vec<PathBuf>,
    open: Option<String>,
) -> io::Result<()> {
    let doc = Rendered::build(&src);
    // The graphics protocol must be probed over stdio *before* the alternate
    // screen is entered, so build the pane up front, only when there are images
    // to show, and tolerate terminals without graphics (None disables the
    // gallery).
    let pane = if images.is_empty() {
        None
    } else {
        ImagePane::new().ok()
    };
    let mut app = App {
        title,
        open,
        doc,
        display: Vec::new(),
        plain: Vec::new(),
        log2disp: Vec::new(),
        hits: Vec::new(),
        offset: 0,
        laid_width: 0,
        viewport_h: 0,
        content_area: Rect::default(),
        mode: Mode::Doc,
        show_toc: false,
        toc_state: ListState::default(),
        link_state: ListState::default(),
        query: String::new(),
        matches: Vec::new(),
        match_set: std::collections::HashSet::new(),
        match_cur: 0,
        images,
        pane,
        gallery_idx: 0,
        flash: None,
    };
    // Same shape as `dir::run`: mouse capture is enabled fresh on every entry
    // into the alternate screen, and opening a link that resolves to a local
    // file tears down this screen, hands the path to sucher's own viewer, and
    // re-enters here on return (ADR 0014's "open in native app" round trip,
    // but staying inside sucher rather than handing off to the OS).
    loop {
        let mut term = ratatui::init();
        let guard = MouseGuard::enable(crate::config::mouse_enabled());
        let action = app.main_loop(&mut term);
        drop(guard);
        ratatui::restore();
        match action {
            Ok(Action::Quit) => return Ok(()),
            Ok(Action::Open(path)) => {
                crate::open_interactive(&path.to_string_lossy());
            }
            Err(e) => return Err(e),
        }
    }
}

impl App {
    fn content_width(&self, total: u16) -> u16 {
        let avail = if self.show_toc {
            total.saturating_sub(TOC_W)
        } else {
            total
        };
        avail.saturating_sub(2).max(8) // minus borders
    }

    fn relayout(&mut self, width: u16) {
        let l = self.doc.layout(width as usize);
        self.display = l.display;
        self.plain = l.plain;
        self.log2disp = l.log2disp;
        self.hits = l.hits;
        self.laid_width = width;
        self.recompute_matches();
        self.clamp();
    }

    fn clamp(&mut self) {
        let max = self.display.len().saturating_sub(1);
        if self.offset > max {
            self.offset = max;
        }
    }

    fn max_offset(&self) -> usize {
        self.display
            .len()
            .saturating_sub(self.viewport_h.max(1) as usize)
    }

    fn recompute_matches(&mut self) {
        self.matches.clear();
        self.match_set.clear();
        if self.query.is_empty() {
            return;
        }
        let q = self.query.to_lowercase();
        for (i, line) in self.plain.iter().enumerate() {
            if line.to_lowercase().contains(&q) {
                self.matches.push(i);
                self.match_set.insert(i);
            }
        }
    }

    fn jump_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let cur = self.offset;
        let idx = if forward {
            self.matches.iter().position(|&m| m > cur).unwrap_or(0)
        } else {
            self.matches
                .iter()
                .rposition(|&m| m < cur)
                .unwrap_or(self.matches.len() - 1)
        };
        self.match_cur = idx;
        self.offset = self.matches[idx];
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<Action> {
        let mut dirty = true;
        loop {
            let size = term.size()?;
            let w = self.content_width(size.width);
            if w != self.laid_width {
                self.relayout(w);
                dirty = true;
            }
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }

            if event::poll(Duration::from_millis(1000))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        // A flash message is cleared by the very next key, whatever
                        // it does; if that key itself sets a new flash (below), the
                        // fresh one wins.
                        self.flash = None;
                        if let Some(action) = self.handle_key(key.code) {
                            return Ok(action);
                        }
                    }
                    // Pointer input only in the plain document view (ADR 0005 D2
                    // shape): while an overlay (search/toc/links/help/gallery) is
                    // up, a click must not reach a link underneath it, so the
                    // whole mouse arm is gated to `Mode::Doc`.
                    Event::Mouse(me) if matches!(self.mode, Mode::Doc) => {
                        dirty = true;
                        match me.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                self.flash = None;
                                if let Some(action) = self.click_link(me.row, me.column) {
                                    return Ok(action);
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                self.offset = (self.offset + 3).min(self.max_offset());
                            }
                            MouseEventKind::ScrollUp => {
                                self.offset = self.offset.saturating_sub(3);
                            }
                            _ => {}
                        }
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
        }
    }

    /// `Some` to leave `main_loop` (quit, or open a resolved path in sucher).
    fn handle_key(&mut self, code: KeyCode) -> Option<Action> {
        match self.mode {
            Mode::Search => self.key_search(code),
            Mode::Toc => self.key_toc(code),
            Mode::Links => self.key_links(code),
            Mode::Gallery => self.key_gallery(code),
            Mode::Help => {
                self.mode = Mode::Doc;
                None
            }
            Mode::Doc => self.key_doc(code),
        }
    }

    /// Map a left-click at terminal `(row, col)` to a display line/column
    /// inside the doc pane's content area and activate the link under it, if
    /// any. A click outside the content area, or that hits no link, does
    /// nothing.
    fn click_link(&mut self, row: u16, col: u16) -> Option<Action> {
        let a = self.content_area;
        if row < a.y || row >= a.y + a.height || col < a.x || col >= a.x + a.width {
            return None;
        }
        let line = self.offset + (row - a.y) as usize;
        let ccol = (col - a.x) as usize;
        let link = self
            .hits
            .get(line)?
            .iter()
            .find(|h| ccol >= h.col && ccol < h.col + h.width)?
            .link;
        self.activate_link(link).map(Action::Open)
    }

    /// Whether an image gallery can open: the document carries images and the
    /// terminal has a working graphics protocol.
    fn has_gallery(&self) -> bool {
        self.pane.is_some() && !self.images.is_empty()
    }

    /// Decode the current gallery image into the pane. Silently keeps the prior
    /// image on a decode error.
    fn load_gallery_image(&mut self) {
        let Some(path) = self.images.get(self.gallery_idx) else {
            return;
        };
        if let Ok(Ok(img)) = crate::util::open_image_reader(path).map(|r| r.decode()) {
            if let Some(pane) = self.pane.as_mut() {
                pane.set(img);
            }
        }
    }

    fn key_gallery(&mut self, code: KeyCode) -> Option<Action> {
        let n = self.images.len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('i') => self.mode = Mode::Doc,
            KeyCode::Char('n') | KeyCode::Char('l') | KeyCode::Right | KeyCode::Down => {
                self.gallery_idx = (self.gallery_idx + 1) % n;
                self.load_gallery_image();
            }
            KeyCode::Char('p') | KeyCode::Char('h') | KeyCode::Left | KeyCode::Up => {
                self.gallery_idx = (self.gallery_idx + n - 1) % n;
                self.load_gallery_image();
            }
            _ => {}
        }
        None
    }

    fn key_doc(&mut self, code: KeyCode) -> Option<Action> {
        let half = (self.viewport_h / 2).max(1) as usize;
        match code {
            // The document pane has no horizontal axis at all, so Left carries no
            // motion here and reads as the back gesture (ADR 0020 D1).
            KeyCode::Char('q') | KeyCode::Esc | KeyCode::Left => return Some(Action::Quit),
            KeyCode::Char('j') | KeyCode::Down => {
                self.offset = (self.offset + 1).min(self.max_offset())
            }
            KeyCode::Char('k') | KeyCode::Up => self.offset = self.offset.saturating_sub(1),
            KeyCode::Char('d') | KeyCode::PageDown => {
                self.offset = (self.offset + half).min(self.max_offset())
            }
            KeyCode::Char('u') | KeyCode::PageUp => self.offset = self.offset.saturating_sub(half),
            KeyCode::Char('g') | KeyCode::Home => self.offset = 0,
            KeyCode::Char('G') | KeyCode::End => self.offset = self.max_offset(),
            KeyCode::Char('t') => {
                self.show_toc = true;
                self.mode = Mode::Toc;
                if self.toc_state.selected().is_none() && !self.doc.toc.is_empty() {
                    self.toc_state.select(Some(0));
                }
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.query.clear();
            }
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),
            KeyCode::Char('l') => {
                if !self.doc.links.is_empty() {
                    self.mode = Mode::Links;
                    self.link_state.select(Some(0));
                }
            }
            KeyCode::Char('i') => {
                if self.has_gallery() {
                    self.mode = Mode::Gallery;
                    self.load_gallery_image();
                }
            }
            KeyCode::Char('x') => {
                if let Some(p) = &self.open {
                    crate::util::open_in_native_app(p);
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        None
    }

    fn key_search(&mut self, code: KeyCode) -> Option<Action> {
        match code {
            KeyCode::Esc => self.mode = Mode::Doc,
            KeyCode::Enter => {
                self.recompute_matches();
                self.jump_match(true);
                self.mode = Mode::Doc;
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => {}
        }
        None
    }

    fn key_toc(&mut self, code: KeyCode) -> Option<Action> {
        let n = self.doc.toc.len();
        match code {
            KeyCode::Esc | KeyCode::Char('t') | KeyCode::Char('q') => {
                self.show_toc = false;
                self.mode = Mode::Doc;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                let i = self.toc_state.selected().unwrap_or(0);
                if i + 1 < n {
                    self.toc_state.select(Some(i + 1));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let i = self.toc_state.selected().unwrap_or(0);
                self.toc_state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Enter => {
                if let Some(i) = self.toc_state.selected() {
                    let log = self.doc.toc[i].line;
                    if let Some(&d) = self.log2disp.get(log) {
                        self.offset = d.min(self.max_offset());
                    }
                    self.show_toc = false;
                    self.mode = Mode::Doc;
                }
            }
            _ => {}
        }
        None
    }

    fn key_links(&mut self, code: KeyCode) -> Option<Action> {
        let n = self.doc.links.len();
        match code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Doc,
            KeyCode::Char('j') | KeyCode::Down => {
                let i = self.link_state.selected().unwrap_or(0);
                if i + 1 < n {
                    self.link_state.select(Some(i + 1));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let i = self.link_state.selected().unwrap_or(0);
                self.link_state.select(Some(i.saturating_sub(1)));
            }
            KeyCode::Enter => {
                if let Some(i) = self.link_state.selected() {
                    // Route through the same decision function a mouse click
                    // on a link uses, so the picker's Enter and a click can
                    // never diverge (see `activate_link`).
                    let action = self.activate_link(i).map(Action::Open);
                    self.mode = Mode::Doc;
                    return action;
                }
                self.mode = Mode::Doc;
            }
            _ => {}
        }
        None
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        if self.mode == Mode::Gallery {
            self.render_gallery(f, area);
            return;
        }
        let cols = if self.show_toc {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(TOC_W), Constraint::Min(0)])
                .split(area)
        } else {
            Layout::default()
                .constraints([Constraint::Min(0)])
                .split(area)
        };
        let main_area = cols[cols.len() - 1];

        if self.show_toc {
            self.render_toc(f, cols[0]);
        }
        self.render_doc(f, main_area);

        match self.mode {
            Mode::Search => self.render_search(f, area),
            Mode::Links => self.render_links(f, area),
            Mode::Help => render_help(f, area),
            _ => {}
        }
    }

    fn render_doc(&mut self, f: &mut Frame, area: Rect) {
        let inner_h = area.height.saturating_sub(3); // borders + status
        self.viewport_h = inner_h;
        // Content rect a later mouse click is mapped against: inside the block
        // border (1 row top, 1 col left), above the status line.
        self.content_area = Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: inner_h,
        };

        let body = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let end = (self.offset + inner_h as usize).min(self.display.len());
        let slice: Vec<Line> = self.display[self.offset..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let gidx = self.offset + i;
                if self.match_set.contains(&gidx) {
                    let bg = if self.matches.get(self.match_cur) == Some(&gidx) {
                        Color::Rgb(80, 70, 20)
                    } else {
                        Color::Rgb(50, 50, 30)
                    };
                    let spans: Vec<Span> = line
                        .spans
                        .iter()
                        .map(|s| Span::styled(s.content.clone(), s.style.bg(bg)))
                        .collect();
                    Line::from(spans)
                } else {
                    line.clone()
                }
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));
        let para = Paragraph::new(Text::from(slice))
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(para, body[0]);

        let pct = if self.display.len() <= 1 {
            100
        } else {
            (self.offset * 100) / self.max_offset().max(1)
        };
        // Surface the image gallery only when it's actually available.
        let imgs = if self.has_gallery() {
            format!("  [i] images ({})", self.images.len())
        } else {
            String::new()
        };
        // `[x] open` only when there's a source file to hand to the OS.
        let open = if self.open.is_some() {
            "  [x] open"
        } else {
            ""
        };
        // Advertise clicking only when mouse capture is actually on; otherwise
        // no mouse events reach us at all and the hint would be a lie.
        let mouse = if crate::config::mouse_enabled() {
            "  click a link"
        } else {
            ""
        };
        let (status, status_style) = if let Some(msg) = &self.flash {
            (
                format!(" {msg}"),
                Style::default().fg(Color::Rgb(252, 211, 77)),
            )
        } else {
            (
                format!(
                    " {}%  {} lines   [j/k] scroll  [t] toc  [/] search  [l] links{imgs}{open}{mouse}  [?] help  [←/q] back",
                    pct.min(100),
                    self.display.len()
                ),
                Style::default().fg(Color::Rgb(140, 140, 150)),
            )
        };
        f.render_widget(Paragraph::new(status).style(status_style), body[1]);
    }

    /// Full-screen image gallery over the document's embedded media.
    fn render_gallery(&mut self, f: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        if let Some(pane) = self.pane.as_mut() {
            pane.render(f, rows[0]);
        }
        let name = self
            .images
            .get(self.gallery_idx)
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let status = format!(
            " image {}/{}  {name}   [n/p] next/prev  [i/q/Esc] back to document",
            self.gallery_idx + 1,
            self.images.len(),
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Rgb(140, 140, 150))),
            rows[1],
        );
    }

    fn render_toc(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .doc
            .toc
            .iter()
            .map(|e| {
                let indent = "  ".repeat(e.level.saturating_sub(1) as usize);
                ListItem::new(format!("{indent}{}", e.title))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Contents "))
            .highlight_style(
                Style::default()
                    .fg(Color::Rgb(125, 211, 252))
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            );
        f.render_stateful_widget(list, area, &mut self.toc_state);
    }

    fn render_search(&self, f: &mut Frame, area: Rect) {
        let bar = Rect {
            x: area.x,
            y: area.height.saturating_sub(1),
            width: area.width,
            height: 1,
        };
        f.render_widget(Clear, bar);
        let hits = self.matches.len();
        let txt = format!(
            "/{}    ({hits} matches, Enter to jump, Esc to cancel)",
            self.query
        );
        f.render_widget(
            Paragraph::new(txt).style(Style::default().fg(Color::Rgb(252, 211, 77))),
            bar,
        );
    }

    fn render_links(&mut self, f: &mut Frame, area: Rect) {
        let popup = centered(area, 70, 60);
        f.render_widget(Clear, popup);
        let items: Vec<ListItem> = self
            .doc
            .links
            .iter()
            .map(|l| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{:<24}", truncate(&l.text, 24)),
                        Style::default().fg(Color::Rgb(96, 165, 250)),
                    ),
                    Span::styled(
                        l.url.clone(),
                        Style::default().fg(Color::Rgb(140, 140, 150)),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Links: Enter to open (web, file, or #anchor), Esc to close "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, popup, &mut self.link_state);
    }

    /// The directory a relative link resolves against: the directory of the
    /// source document (`self.open`), or the process's current directory when
    /// the document was rendered from stdin and has no source file.
    fn source_dir(&self) -> PathBuf {
        match &self.open {
            Some(p) => Path::new(p)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// The single place a link (index `i` into `self.doc.links`) is turned
    /// into an effect, whether it was reached by clicking it or by pressing
    /// Enter in the link picker (`key_links`), so the two can never diverge.
    /// `Some(path)` means "open this path in sucher's own viewer"; every
    /// other outcome is handled here and returns `None`.
    fn activate_link(&mut self, i: usize) -> Option<PathBuf> {
        let url = self.doc.links.get(i)?.url.clone();
        let base_dir = self.source_dir();
        match decide_link(&url, &base_dir) {
            LinkDecision::Browser(u) => {
                open_url(&u);
                None
            }
            LinkDecision::Anchor(fragment) => {
                let target = crate::markdown::slug(&fragment);
                let entry = self
                    .doc
                    .toc
                    .iter()
                    .find(|e| crate::markdown::slug(&e.title) == target)?;
                if let Some(&d) = self.log2disp.get(entry.line) {
                    self.offset = d.min(self.max_offset());
                }
                None
            }
            LinkDecision::Refuse => {
                self.flash = Some("refused: unsupported link scheme".to_string());
                None
            }
            LinkDecision::Open(path) => {
                // A local path is safe to open IN SUCHER even though the
                // scheme check above refuses `file://`/etc for the OS opener
                // (ADR 0019 D2): sucher only ever VIEWS the file, it never
                // executes it, so handing it to sucher's own viewer carries
                // none of the risk `open`/`xdg-open` would.
                if path.exists() {
                    Some(path)
                } else {
                    self.flash = Some(format!("no such file: {url}"));
                    None
                }
            }
        }
    }
}

/// The pure outcome of deciding what a link target means, before any impure
/// effect (opening a browser, jumping the viewport, touching the filesystem)
/// runs. Kept separate from [`App::activate_link`] specifically so the
/// decision can be unit-tested without a terminal or a real file on disk.
#[derive(Debug, PartialEq, Eq)]
enum LinkDecision {
    /// Hand to the OS browser/mail opener (`http`/`https`/`mailto`).
    Browser(String),
    /// An in-document `#fragment` jump; the fragment with the leading `#`
    /// stripped.
    Anchor(String),
    /// A local path resolved against the source document's directory (or the
    /// process cwd), NOT yet checked for existence.
    Open(PathBuf),
    /// A scheme sucher will not act on at all (`file:`, `javascript:`, any
    /// other custom scheme).
    Refuse,
}

/// Decide what `url` (a link target from an untrusted document) means, given
/// the directory a relative path should resolve against. Pure: does not touch
/// the filesystem, open a browser, or move the viewport, so it is testable in
/// isolation. See `LinkDecision` for what each outcome means and
/// `App::activate_link` for how the caller turns it into an effect.
fn decide_link(url: &str, base_dir: &Path) -> LinkDecision {
    // (a) web/mail: unchanged from the existing link-picker behaviour.
    if crate::util::is_safe_url(url) {
        return LinkDecision::Browser(url.to_string());
    }
    // (b) in-document anchor.
    if let Some(fragment) = url.strip_prefix('#') {
        return LinkDecision::Anchor(fragment.to_string());
    }
    // (c) any OTHER explicit scheme is refused, never resolved as a path.
    // This is what keeps `file://`, `javascript:`, and custom schemes away
    // from sucher's own opener: only a target with NO scheme prefix at all
    // reaches step (d) below (ADR 0019 D2).
    if has_scheme(url) {
        return LinkDecision::Refuse;
    }
    // (d) a bare path. Split off a trailing `#fragment` (a path can carry one
    // too, e.g. `./doc.md#section`, though sucher does not currently jump to
    // it after opening), percent-decode the path portion, and resolve it
    // against `base_dir` unless it is already absolute.
    let path_part = url.split('#').next().unwrap_or(url);
    let decoded = percent_decode(path_part);
    let candidate = Path::new(&decoded);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base_dir.join(candidate)
    };
    LinkDecision::Open(resolved)
}

/// Whether `url` starts with an explicit URI scheme, matching
/// `^[A-Za-z][A-Za-z0-9+.-]*:`. A path with no scheme (`./a.md`, `../b/c.md`,
/// `/abs/path.md`) does not match, so it falls through to path resolution.
fn has_scheme(url: &str) -> bool {
    let mut chars = url.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-') {
            return false;
        }
    }
    false
}

/// Minimal percent-decoding for link paths: decodes `%XX` escapes (the
/// decoded bytes are reassembled as UTF-8, lossily if they are not valid),
/// leaving anything that is not a well-formed escape untouched. No new
/// dependency: a markdown link target only ever needs this common case, not
/// full RFC 3986 handling.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn render_help(f: &mut Frame, area: Rect) {
    let popup = centered(area, 56, 60);
    f.render_widget(Clear, popup);
    let text = Text::from(vec![
        Line::from("  j / k  ↑ / ↓     scroll one line"),
        Line::from("  d / u            half-page down / up"),
        Line::from("  g / G            top / bottom"),
        Line::from("  t                table of contents"),
        Line::from("  /                search  (n / N = next / prev)"),
        Line::from("  l                link picker (web, local file, #anchor)"),
        Line::from("  click            open a link (mouse, when enabled)"),
        Line::from("  i                image gallery (docx / pptx media)"),
        Line::from("  x                open in native app  (OS default)"),
        Line::from("  ?                this help"),
        Line::from("  q / Esc / ←      quit / close overlay"),
    ]);
    let p = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" Keys "));
    f.render_widget(p, popup);
}

fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let w = area.width * pct_w / 100;
    let h = area.height * pct_h / 100;
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
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

fn open_url(url: &str) {
    // The link target comes from an untrusted document; only hand web/mail URLs
    // to the OS opener (ADR 0009 / S5). A `file://`, custom-scheme, or `-`-leading
    // target is silently ignored rather than spawned.
    if !crate::util::is_safe_url(url) {
        return;
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    // `rundll32 …FileProtocolHandler` opens the URL in the default handler
    // without going through `cmd`, which would re-parse the argument and let a
    // crafted link (e.g. `& calc`) inject a command.
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn percent_decode_common_escapes() {
        assert_eq!(percent_decode("%20"), " ");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("no-escapes-here"), "no-escapes-here");
        // A trailing/malformed escape is left byte-for-byte, not dropped.
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn scheme_detection() {
        assert!(has_scheme("file:///etc/passwd"));
        assert!(has_scheme("javascript:alert(1)"));
        assert!(!has_scheme("./a.md"));
        assert!(!has_scheme("../b/c.md"));
        assert!(!has_scheme("/abs/path.md"));
    }

    #[test]
    fn decide_link_routes_by_kind() {
        let base = Path::new("/base");
        assert_eq!(
            decide_link("http://e.com", base),
            LinkDecision::Browser("http://e.com".to_string())
        );
        assert_eq!(
            decide_link("#install", base),
            LinkDecision::Anchor("install".to_string())
        );
        assert_eq!(
            decide_link("file:///etc/passwd", base),
            LinkDecision::Refuse
        );
        assert_eq!(
            decide_link("javascript:alert(1)", base),
            LinkDecision::Refuse
        );
        match decide_link("./a.md", base) {
            LinkDecision::Open(p) => {
                assert_eq!(p.file_name().unwrap(), "a.md");
                assert!(p.starts_with(base));
            }
            other => panic!("expected Open, got {other:?}"),
        }
        match decide_link("/abs/path.md", base) {
            LinkDecision::Open(p) => assert_eq!(p, PathBuf::from("/abs/path.md")),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    /// Per-test-unique temp directory, cleaned up on drop; mirrors the fixture
    /// pattern already used by `search.rs`'s tests.
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sucher-tui-link-test-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn relative_link_resolves_against_source_document_dir() {
        let fx = Fixture::new("resolve");
        fs::write(fx.root.join("sibling.md"), "hi").unwrap();

        let resolved = match decide_link("./sibling.md", &fx.root) {
            LinkDecision::Open(p) => p,
            other => panic!("expected Open, got {other:?}"),
        };
        assert!(resolved.exists());
        assert_eq!(resolved.file_name().unwrap(), "sibling.md");
    }

    #[test]
    fn missing_relative_link_does_not_exist() {
        let fx = Fixture::new("missing");
        let resolved = match decide_link("./missing.md", &fx.root) {
            LinkDecision::Open(p) => p,
            other => panic!("expected Open, got {other:?}"),
        };
        assert!(!resolved.exists());
    }
}
