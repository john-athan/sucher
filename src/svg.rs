// SVG viewer: rasterise the vector with resvg and show the picture *above* the
// source. The image occupies the upper region; the XML source scrolls in a pane
// below it, so an SVG is both seen and readable. Terminals without a graphics
// protocol still get the source (the image pane simply draws nothing).
//
// Rasterisation lives in [`render_svg`], reused by the directory browser's
// preview pane. Supersedes ADR-0001 D3 ("SVG is Text, no in-tree rasteriser") —
// resvg/usvg/tiny-skia are now that rasteriser.

use crate::media::ImagePane;
use crate::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use image::DynamicImage;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use resvg::{tiny_skia, usvg};
use std::fs;
use std::io;
use std::time::Duration;

/// Cap the rasterised bitmap's largest side, keeping crisp output bounded.
const MAX_DIM: u32 = 2000;

/// Rasterise an SVG file to an RGBA image. Scales up small documents for a crisp
/// result and clamps very large ones to [`MAX_DIM`]. Errors on unreadable files
/// or invalid SVG.
pub fn render_svg(path: &str) -> Result<DynamicImage, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(&data, &opt).map_err(|e| e.to_string())?;
    let size = tree.size();
    let (w0, h0) = (size.width(), size.height());
    if w0 <= 0.0 || h0 <= 0.0 {
        return Err("SVG has zero size".to_string());
    }
    // Scale so the larger side lands near MAX_DIM (never below 1×), for sharpness.
    let scale = (MAX_DIM as f32 / w0.max(h0)).clamp(1.0, 8.0);
    let w = ((w0 * scale).ceil() as u32).clamp(1, MAX_DIM);
    let h = ((h0 * scale).ceil() as u32).clamp(1, MAX_DIM);

    let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("could not allocate pixmap")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(w as f32 / w0, h as f32 / h0),
        &mut pixmap.as_mut(),
    );
    let rgba = image::RgbaImage::from_raw(w, h, pixmap.data().to_vec())
        .ok_or("pixmap → image conversion failed")?;
    Ok(DynamicImage::ImageRgba8(rgba))
}

struct App {
    title: String,
    pane: ImagePane,
    dims: (u32, u32),
    src: Vec<String>,
    offset: usize,
    text_h: u16,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let img = match render_svg(&path) {
        Ok(img) => img,
        Err(e) => {
            // No raster (invalid SVG / no allocator): fall back to the text viewer
            // so the source is still readable.
            eprintln!("sucher: {path}: {e}");
            return crate::text::run(title, path);
        }
    };
    let dims = (img.width(), img.height());
    let mut pane = ImagePane::new()?; // must probe before the alternate screen
    pane.set(img);

    let src = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();

    let mut app = App {
        title,
        pane,
        dims,
        src,
        offset: 0,
        text_h: 0,
    };
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

impl App {
    fn max_offset(&self) -> usize {
        self.src.len().saturating_sub(self.text_h.max(1) as usize)
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
                            KeyCode::Char('j') | KeyCode::Down => {
                                self.offset = (self.offset + 1).min(self.max_offset())
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                self.offset = self.offset.saturating_sub(1)
                            }
                            KeyCode::Char('g') | KeyCode::Home => self.offset = 0,
                            KeyCode::Char('G') | KeyCode::End => self.offset = self.max_offset(),
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
        // Image on top (~65%), source text below, then a status line.
        let rows = Layout::default()
            .constraints([
                Constraint::Percentage(65),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        self.pane.render(f, rows[0]);
        self.render_source(f, rows[1]);

        let (w, h) = self.dims;
        let status = format!(
            " {}   SVG {w}×{h}px   [j/k] scroll source  [q] quit",
            self.title
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme::DIM)),
            rows[2],
        );
    }

    fn render_source(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::TOP).title(" source ");
        let inner = block.inner(area);
        self.text_h = inner.height;
        let end = (self.offset + inner.height as usize).min(self.src.len());
        let lines: Vec<Line> = (self.offset..end)
            .map(|i| {
                Line::from(Span::styled(
                    self.src[i].clone(),
                    Style::default().fg(Color::Rgb(180, 190, 205)),
                ))
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::render_svg;

    #[test]
    fn rasterises_sample_svg() {
        let img = render_svg("samples/vector.svg").expect("render");
        assert!(img.width() >= 100 && img.height() >= 60, "scaled up");
    }

    #[test]
    fn invalid_svg_errors() {
        assert!(render_svg("Cargo.toml").is_err());
    }
}
