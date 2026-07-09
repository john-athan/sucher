// Interactive terminal UI for navigating a markdown document.
// Scroll, table-of-contents sidebar with jump, in-document search, and a
// link picker that opens URLs in the default browser.

use crate::markdown::Rendered;
use crate::media::ImagePane;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::io;
use std::path::PathBuf;
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
    doc: Rendered,
    display: Vec<Line<'static>>,
    plain: Vec<String>,
    log2disp: Vec<usize>,
    offset: usize,
    laid_width: u16,
    viewport_h: u16,
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
}

/// Open a markdown document. `images` are embedded rasters (docx/pptx media)
/// browsable in a gallery overlay; pass an empty vec for plain markdown.
pub fn run(title: String, src: String, images: Vec<PathBuf>) -> io::Result<()> {
    let doc = Rendered::build(&src);
    // The graphics protocol must be probed over stdio *before* the alternate
    // screen is entered, so build the pane up front — only when there are images
    // to show, and tolerate terminals without graphics (None → gallery disabled).
    let pane = if images.is_empty() {
        None
    } else {
        ImagePane::new().ok()
    };
    let mut app = App {
        title,
        doc,
        display: Vec::new(),
        plain: Vec::new(),
        log2disp: Vec::new(),
        offset: 0,
        laid_width: 0,
        viewport_h: 0,
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
    };
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
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
        let (d, p, m) = self.doc.layout(width as usize);
        self.display = d;
        self.plain = p;
        self.log2disp = m;
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

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
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
                        if self.handle_key(key.code) {
                            return Ok(());
                        }
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
        }
    }

    /// Returns true to quit.
    fn handle_key(&mut self, code: KeyCode) -> bool {
        match self.mode {
            Mode::Search => self.key_search(code),
            Mode::Toc => self.key_toc(code),
            Mode::Links => self.key_links(code),
            Mode::Gallery => self.key_gallery(code),
            Mode::Help => {
                self.mode = Mode::Doc;
                false
            }
            Mode::Doc => self.key_doc(code),
        }
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
        if let Ok(Ok(img)) = image::ImageReader::open(path).map(|r| r.decode()) {
            if let Some(pane) = self.pane.as_mut() {
                pane.set(img);
            }
        }
    }

    fn key_gallery(&mut self, code: KeyCode) -> bool {
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
        false
    }

    fn key_doc(&mut self, code: KeyCode) -> bool {
        let half = (self.viewport_h / 2).max(1) as usize;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
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
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
        false
    }

    fn key_search(&mut self, code: KeyCode) -> bool {
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
        false
    }

    fn key_toc(&mut self, code: KeyCode) -> bool {
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
        false
    }

    fn key_links(&mut self, code: KeyCode) -> bool {
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
                    open_url(&self.doc.links[i].url);
                }
                self.mode = Mode::Doc;
            }
            _ => {}
        }
        false
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
        let status = format!(
            " {}%  {} lines   [j/k] scroll  [t] toc  [/] search  [l] links{imgs}  [?] help  [q] quit",
            pct.min(100),
            self.display.len()
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Rgb(140, 140, 150))),
            body[1],
        );
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
                    .title(" Links — Enter to open, Esc to close "),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        f.render_stateful_widget(list, popup, &mut self.link_state);
    }
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
        Line::from("  l                link picker"),
        Line::from("  i                image gallery (docx / pptx media)"),
        Line::from("  ?                this help"),
        Line::from("  q / Esc          quit / close overlay"),
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
