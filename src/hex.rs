// Hex viewer for binary (and any unopenable) files.
//
// A scrolling, read-only canonical hexdump: each row shows a 16-byte-aligned
// `offset │ 16 hex bytes │ ASCII gutter`. Non-printable bytes render as `.` in
// the ASCII column and dimmed in the hex column, so structure stays legible.
// Bytes are capped for bounded memory, matching the text viewer's ethos. One
// data row maps to one screen row, so the row index is the scroll offset.

use crate::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::fs;
use std::io::{self, Read};
use std::time::Duration;

/// Read cap: bounded memory for pathological inputs (mirrors `text::MAX_BYTES`).
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Bytes per hexdump row.
const ROW: usize = 16;

pub struct App {
    title: String,
    data: Vec<u8>,
    truncated: bool,
    offset: usize,   // top visible row (each row is ROW bytes)
    viewport_h: u16, // content rows (set at render)
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let (data, truncated) = read_capped(&path)?;
    let mut app = App {
        title,
        data,
        truncated,
        offset: 0,
        viewport_h: 0,
    };
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// One-shot canonical hexdump to stdout for piped/non-TTY output.
pub fn dump(path: &str) -> String {
    let (data, truncated) = match read_capped(path) {
        Ok(v) => v,
        Err(e) => return format!("sucher: {path}: {e}\n"),
    };
    let mut out = String::new();
    for (i, chunk) in data.chunks(ROW).enumerate() {
        out.push_str(&row_text(i * ROW, chunk));
        out.push('\n');
    }
    if truncated {
        out.push_str("(… truncated)\n");
    }
    out
}

impl App {
    fn rows(&self) -> usize {
        self.data.len().div_ceil(ROW)
    }

    fn max_offset(&self) -> usize {
        self.rows().saturating_sub(self.viewport_h.max(1) as usize)
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
            _ => {}
        }
        false
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let body = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));
        let inner = block.inner(body[0]);
        self.viewport_h = inner.height;

        let end = (self.offset + inner.height as usize).min(self.rows());
        let rows: Vec<Line> = (self.offset..end)
            .map(|r| {
                let start = r * ROW;
                let chunk = &self.data[start..(start + ROW).min(self.data.len())];
                row_line(start, chunk)
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(rows)).block(block), body[0]);

        let pct = if self.rows() <= 1 {
            100
        } else {
            (self.offset * 100) / self.max_offset().max(1)
        };
        let trunc = if self.truncated {
            "  ·  truncated"
        } else {
            ""
        };
        let status = format!(
            " {pct}%  {} bytes   [j/k] scroll  [d/u] page  [g/G] top/end  [q] quit{trunc}",
            self.data.len(),
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme::palette().dim)),
            body[1],
        );
    }
}

/// Build one coloured hexdump [`Line`]: offset (dim), hex bytes (NUL/non-print
/// dimmed), then the ASCII gutter (printable as-is, other bytes as `.`).
fn row_line(offset: usize, chunk: &[u8]) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{offset:08x}  "),
        Style::default().fg(theme::palette().dim),
    )];
    for i in 0..ROW {
        if let Some(&b) = chunk.get(i) {
            let color = if b == 0 {
                theme::palette().dim
            } else if is_print(b) {
                theme::palette().other
            } else {
                Color::Rgb(160, 160, 170)
            };
            spans.push(Span::styled(
                format!("{b:02x} "),
                Style::default().fg(color),
            ));
        } else {
            spans.push(Span::raw("   "));
        }
        if i == 7 {
            spans.push(Span::raw(" "));
        }
    }
    let ascii: String = chunk
        .iter()
        .map(|&b| if is_print(b) { b as char } else { '.' })
        .collect();
    spans.push(Span::styled(
        format!(" │{ascii}"),
        Style::default().fg(theme::palette().other),
    ));
    Line::from(spans)
}

/// Plain-text form of one row for [`dump`], mirroring [`row_line`]'s layout.
fn row_text(offset: usize, chunk: &[u8]) -> String {
    let mut hex = String::new();
    for i in 0..ROW {
        match chunk.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x} ")),
            None => hex.push_str("   "),
        }
        if i == 7 {
            hex.push(' ');
        }
    }
    let ascii: String = chunk
        .iter()
        .map(|&b| if is_print(b) { b as char } else { '.' })
        .collect();
    format!("{offset:08x}  {hex} │{ascii}")
}

/// Up to `max_rows` hexdump lines of a file's head, for the directory preview
/// pane. Reads only what it needs; returns a single "unreadable" note on error.
pub fn preview(path: &str, max_rows: usize) -> Vec<String> {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec!["No preview".to_string()],
    };
    let mut buf = vec![0u8; max_rows * ROW];
    let n = match f.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return vec!["No preview".to_string()],
    };
    buf[..n]
        .chunks(ROW)
        .enumerate()
        .map(|(i, chunk)| row_text(i * ROW, chunk))
        .collect()
}

/// Printable 7-bit ASCII (space through `~`).
fn is_print(b: u8) -> bool {
    (0x20..=0x7e).contains(&b)
}

/// Read a file capped at [`MAX_BYTES`]; the bool is true when the cap truncated.
fn read_capped(path: &str) -> io::Result<(Vec<u8>, bool)> {
    let f = fs::File::open(path)?;
    let mut buf = Vec::new();
    // Read one extra byte so an exactly-capped file still reports truncation.
    f.take((MAX_BYTES + 1) as u64).read_to_end(&mut buf)?;
    let truncated = buf.len() > MAX_BYTES;
    buf.truncate(MAX_BYTES);
    Ok((buf, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_text_layout_and_ascii() {
        let line = row_text(0, b"ABC\x00\xff");
        assert!(line.starts_with("00000000  "));
        assert!(line.contains("41 42 43 00 ff"));
        // NUL and 0xff are non-printable → `.` in the ASCII gutter; ABC stay.
        assert!(line.ends_with("│ABC.."));
    }

    #[test]
    fn is_print_covers_ascii_range_only() {
        assert!(is_print(b' '));
        assert!(is_print(b'~'));
        assert!(!is_print(0x00));
        assert!(!is_print(0x7f));
        assert!(!is_print(0xff));
    }

    #[test]
    fn short_final_row_pads_hex_columns() {
        // A single byte still produces a full-width hex field (fixed columns).
        let line = row_text(16, b"Z");
        assert!(line.starts_with("00000010  5a "));
        assert!(line.ends_with("│Z"));
    }
}
