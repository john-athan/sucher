// PDF viewer. Rasterizes pages with poppler's `pdftoppm` (no native linking),
// displays them via the terminal graphics protocol, and pages with the
// keyboard. Falls back to `pdftotext` for the non-interactive dump.

use crate::media::ImagePane;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const CACHE_MAX: usize = 12;

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
    let out = Command::new("pdfinfo").arg(path).output();
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
        std::env::temp_dir().join(format!("vellum-pdf-{}-{}", std::process::id(), page));
    // pdftocairo (cairo backend) over pdftoppm (splash) — splash renders some
    // PDFs (e.g. certain reportlab output) as blank pages; cairo is robust.
    // Render straight to the display width instead of 150dpi + downscale.
    let status = Command::new("pdftocairo")
        .args(["-png", "-scale-to-x"])
        .arg(target_w.to_string())
        .args(["-scale-to-y", "-1", "-f"])
        .arg((page + 1).to_string())
        .arg("-l")
        .arg((page + 1).to_string())
        .arg("-singlefile")
        .arg(path)
        .arg(&prefix)
        .status()
        .map_err(|e| format!("pdftocairo: {e}"))?;
    if !status.success() {
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

struct PdfApp {
    title: String,
    path: String,
    page: usize,
    pages: usize,
    pane: ImagePane,
    err: Option<String>,
    cache: HashMap<usize, image::DynamicImage>,
    order: VecDeque<usize>,
    target_w: u32,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let pages = page_count(&path);
    let pane = ImagePane::new()?;
    let mut app = PdfApp {
        title,
        path,
        page: 0,
        pages,
        pane,
        err: None,
        cache: HashMap::new(),
        order: VecDeque::new(),
        target_w: target_px_width(),
    };
    app.load();
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// Non-interactive: extract text.
pub fn dump(path: &str) -> String {
    match Command::new("pdftotext").arg(path).arg("-").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => format!("vellum: pdftotext: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("vellum: pdftotext: {e}"),
    }
}

impl PdfApp {
    fn load(&mut self) {
        if let Some(img) = self.cache.get(&self.page) {
            self.pane.set(img.clone());
            self.err = None;
            return;
        }
        match render_page(&self.path, self.page, self.target_w) {
            Ok(img) => {
                self.pane.set(img.clone());
                self.err = None;
                self.cache.insert(self.page, img);
                self.order.push_back(self.page);
                while self.order.len() > CACHE_MAX {
                    if let Some(old) = self.order.pop_front() {
                        self.cache.remove(&old);
                    }
                }
            }
            Err(e) => self.err = Some(e),
        }
    }

    fn goto(&mut self, page: usize) {
        let p = page.min(self.pages.saturating_sub(1));
        if p != self.page || self.err.is_some() {
            self.page = p;
            self.load();
        }
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }
            if event::poll(Duration::from_millis(1000))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('j') | KeyCode::Right | KeyCode::Char(' ')
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
        let status = format!(
            " {}   page {}/{}   [j/k or ←/→] page  [g/G] first/last  [q] quit",
            self.title,
            self.page + 1,
            self.pages
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Rgb(140, 140, 150))),
            chunks[1],
        );
    }
}
