// PDF viewer. Rasterizes pages with poppler's `pdftoppm` (no native linking),
// displays them via the terminal graphics protocol, and pages with the
// keyboard. Falls back to `pdftotext` for the non-interactive dump.

use crate::media::ImagePane;
use crate::util;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

const CACHE_MAX: usize = 12;

/// A finished page render arriving from a worker thread: the page index and the
/// rastered image (or the error text if `pdftocairo` failed for that page).
type RenderMsg = (usize, Result<image::DynamicImage, String>);

/// Target raster width in pixels: match the terminal's pixel width so we don't
/// render at a fixed 150dpi and then throw most of it away downscaling.
fn target_px_width() -> u32 {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 => (ws.width as u32).clamp(400, 1600),
        // pixel size unreported: estimate from columns (~8px/cell)
        Ok(ws) => (ws.columns as u32 * 8).clamp(400, 1600),
        Err(_) => 1000,
    }
}

fn page_count(path: &str) -> usize {
    let mut cmd = Command::new("pdfinfo");
    cmd.arg("--").arg(util::cmd_path_arg(path));
    let out = util::run_with_timeout(cmd, util::SUBPROCESS_TIMEOUT);
    if let Ok(o) = out {
        let txt = String::from_utf8_lossy(&o.stdout);
        for line in txt.lines() {
            if let Some(rest) = line.strip_prefix("Pages:") {
                if let Ok(n) = rest.trim().parse::<usize>() {
                    return n;
                }
            }
        }
    }
    1
}

fn render_page(path: &str, page: usize, target_w: u32) -> Result<image::DynamicImage, String> {
    let prefix: PathBuf =
        std::env::temp_dir().join(format!("sucher-pdf-{}-{}", std::process::id(), page));
    // pdftocairo (cairo backend) over pdftoppm (splash) — splash renders some
    // PDFs (e.g. certain reportlab output) as blank pages; cairo is robust.
    // Render straight to the display width instead of 150dpi + downscale.
    let mut cmd = Command::new("pdftocairo");
    cmd.args(["-png", "-scale-to-x"])
        .arg(target_w.to_string())
        .args(["-scale-to-y", "-1", "-f"])
        .arg((page + 1).to_string())
        .arg("-l")
        .arg((page + 1).to_string())
        .arg("-singlefile")
        .arg("--")
        .arg(util::cmd_path_arg(path))
        .arg(&prefix);
    let output = util::run_with_timeout(cmd, util::SUBPROCESS_TIMEOUT)
        .map_err(|e| format!("pdftocairo: {e}"))?;
    if !output.status.success() {
        return Err("pdftocairo failed".into());
    }
    let png = prefix.with_extension("png");
    let img = image::ImageReader::open(&png)
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&png);
    Ok(img)
}

/// First page rastered to an image, for the directory browser's preview pane.
pub fn poster(path: &str) -> Result<image::DynamicImage, String> {
    render_page(path, 0, target_px_width().min(900))
}

struct PdfApp {
    title: String,
    path: String,
    page: usize,
    pages: usize,
    pane: ImagePane,
    err: Option<String>,
    cache: HashMap<usize, image::DynamicImage>,
    order: VecDeque<usize>,
    /// Pages a worker is currently rendering — coalesces so the same page is
    /// never spawned twice (a re-visit while its first render is in flight).
    pending: HashSet<usize>,
    /// Current page requested but not yet in `cache`: the pane still shows the
    /// previous page (or nothing on first open) while the worker rasters.
    rendering: bool,
    tx: Sender<RenderMsg>,
    rx: Receiver<RenderMsg>,
    target_w: u32,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let pages = page_count(&path);
    let pane = ImagePane::new()?;
    let (tx, rx) = mpsc::channel();
    let mut app = PdfApp {
        title,
        path,
        page: 0,
        pages,
        pane,
        err: None,
        cache: HashMap::new(),
        order: VecDeque::new(),
        pending: HashSet::new(),
        rendering: false,
        tx,
        rx,
        target_w: target_px_width(),
    };
    // Kick off the first page (and its neighbour) asynchronously; the main loop
    // installs each as it arrives. Rendering off-thread keeps input responsive
    // even on a slow first raster.
    app.show_current();
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// Non-interactive: extract text.
pub fn dump(path: &str) -> String {
    let mut cmd = Command::new("pdftotext");
    cmd.arg("--").arg(util::cmd_path_arg(path)).arg("-");
    match util::run_with_timeout(cmd, util::SUBPROCESS_TIMEOUT) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => format!("sucher: pdftotext: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("sucher: pdftotext: {e}"),
    }
}

impl PdfApp {
    /// Spawn a worker to raster `page` unless it's already cached or in flight.
    /// Each page writes a distinct temp PNG (`…-{page}.png`), so concurrent
    /// workers never collide.
    fn request(&mut self, page: usize) {
        if page >= self.pages || self.cache.contains_key(&page) || self.pending.contains(&page) {
            return;
        }
        self.pending.insert(page);
        let tx = self.tx.clone();
        let path = self.path.clone();
        let w = self.target_w;
        thread::spawn(move || {
            let _ = tx.send((page, render_page(&path, page, w)));
        });
    }

    /// Warm the neighbours of the current page so the common next/prev step is a
    /// cache hit. Forward first (reading bias), then back.
    fn prefetch(&mut self) {
        self.request(self.page + 1);
        if self.page > 0 {
            self.request(self.page - 1);
        }
    }

    /// Install the current page from cache if present; otherwise request it and
    /// leave the previous page on screen (marked "rendering") until it arrives.
    /// Always warms the neighbours.
    fn show_current(&mut self) {
        self.err = None;
        if let Some(img) = self.cache.get(&self.page) {
            self.pane.set(img.clone());
            self.rendering = false;
        } else {
            self.rendering = true;
            self.request(self.page);
        }
        self.prefetch();
    }

    fn goto(&mut self, page: usize) {
        let p = page.min(self.pages.saturating_sub(1));
        if p != self.page || self.err.is_some() {
            self.page = p;
            self.show_current();
        }
    }

    /// Drop least-recently-rendered pages past the cache cap, never evicting the
    /// page currently on screen. Returns whether the visible page changed.
    fn evict(&mut self) {
        while self.order.len() > CACHE_MAX {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if old == self.page {
                // Keep the visible page; retire it to the back of the queue.
                self.order.push_back(old);
                continue;
            }
            self.cache.remove(&old);
        }
    }

    /// Absorb finished renders from the workers. Installs the current page the
    /// moment its render lands. Returns whether the frame needs a redraw.
    fn drain(&mut self) -> bool {
        let mut dirty = false;
        while let Ok((page, res)) = self.rx.try_recv() {
            self.pending.remove(&page);
            match res {
                Ok(img) => {
                    self.cache.insert(page, img);
                    self.order.push_back(page);
                    self.evict();
                    if page == self.page {
                        // Re-borrow: `evict` may have touched the map.
                        if let Some(img) = self.cache.get(&page) {
                            self.pane.set(img.clone());
                        }
                        self.rendering = false;
                        self.err = None;
                        dirty = true;
                    }
                }
                Err(e) if page == self.page => {
                    self.err = Some(e);
                    self.rendering = false;
                    dirty = true;
                }
                Err(_) => {} // a prefetch failed; ignore until it's the current page
            }
        }
        dirty
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }
            // Poll briefly while a render is outstanding so its result installs
            // promptly; idle otherwise to avoid needless wakeups.
            let timeout = if self.pending.is_empty() {
                Duration::from_millis(1000)
            } else {
                Duration::from_millis(40)
            };
            if event::poll(timeout)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('x') => util::open_in_native_app(&self.path),
                            KeyCode::Char('j')
                            | KeyCode::Right
                            | KeyCode::Char(' ')
                            | KeyCode::PageDown => self.goto(self.page + 1),
                            KeyCode::Char('k') | KeyCode::Left | KeyCode::PageUp => {
                                self.goto(self.page.saturating_sub(1))
                            }
                            KeyCode::Char('g') | KeyCode::Home => self.goto(0),
                            KeyCode::Char('G') | KeyCode::End => self.goto(self.pages),
                            _ => {}
                        }
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
            if self.drain() {
                dirty = true;
            }
        }
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        if let Some(e) = &self.err {
            f.render_widget(
                Paragraph::new(format!("render error: {e}"))
                    .style(Style::default().fg(Color::Rgb(248, 113, 113))),
                chunks[0],
            );
        } else {
            self.pane.render(f, chunks[0]);
        }
        let hint = if self.rendering { "  rendering…" } else { "" };
        let status = format!(
            " {}   page {}/{}{}   [j/k or ←/→] page  [g/G] first/last  [x] open  [q] quit",
            self.title,
            self.page + 1,
            self.pages,
            hint
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Rgb(140, 140, 150))),
            chunks[1],
        );
    }
}
