// Interactive terminal UI for a source/plain-text file.
//
// A faithful, scrolling, syntax-highlighted viewer: one source line maps to one
// screen row (no soft-wrap), so line indices are the scroll offset directly. A
// dim line-number gutter sits left of the highlighted content; long lines are
// reached by horizontal panning rather than wrapping. Search mirrors the
// markdown viewer (`tui.rs`): case-insensitive substring on the raw lines with
// matching rows highlighted. Colours come from `theme`; classification and
// tokenisation come from `highlight` — this module owns only presentation.

use crate::highlight::{self, Syntax, Token};
use crate::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;

/// Read caps: bounded memory for pathological inputs, matching Sucher's ethos.
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINES: usize = 100_000;
/// Columns panned per horizontal step.
const HSTEP: usize = 4;

#[derive(PartialEq)]
enum Mode {
    Doc,
    Search,
}

pub struct App {
    title: String,
    lines: Vec<String>,  // raw lines, for search + horizontal slicing
    hl: Vec<Vec<Token>>, // one token row per raw line (1:1 with `lines`)
    lang: String,        // detected language label, or "text"
    truncated: bool,     // capped by byte/line limit
    max_line_len: usize, // longest line in chars, for horizontal clamp
    offset: usize,       // top visible line
    hoffset: usize,      // leftmost visible column
    viewport_h: u16,     // content rows (set at render)
    viewport_w: u16,     // content columns after the gutter (set at render)
    mode: Mode,
    query: String,
    matches: Vec<usize>,
    match_set: std::collections::HashSet<usize>,
    match_cur: usize,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let (lines, truncated) = read_capped(&path)?;
    let ext = ext_of(&path);
    let syntax = highlight::syntax_for(&ext).unwrap_or(highlight::PLAIN);
    let hl = highlight_all(&lines, syntax);
    let max_line_len = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);

    let mut app = App {
        title,
        lines,
        hl,
        lang: language_name(&ext),
        truncated,
        max_line_len,
        offset: 0,
        hoffset: 0,
        viewport_h: 0,
        viewport_w: 0,
        mode: Mode::Doc,
        query: String::new(),
        matches: Vec::new(),
        match_set: std::collections::HashSet::new(),
        match_cur: 0,
    };
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// Raw file contents for piped/non-TTY output: faithful bytes, lossy-decoded,
/// with NO highlighting or ANSI so `v file.rs | grep` behaves like `cat`.
pub fn dump(path: &str) -> String {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => format!("sucher: {path}: {e}\n"),
    }
}

impl App {
    /// Width of the line-number gutter: the digits of the largest line number
    /// plus the ` │ ` separator (3 columns).
    fn num_width(&self) -> usize {
        self.lines.len().max(1).to_string().len()
    }
    fn gutter_w(&self) -> usize {
        self.num_width() + 3
    }

    fn max_offset(&self) -> usize {
        self.lines
            .len()
            .saturating_sub(self.viewport_h.max(1) as usize)
    }

    /// Furthest a line can be panned while keeping its tail reachable.
    fn max_hoffset(&self) -> usize {
        self.max_line_len
            .saturating_sub(self.viewport_w.max(1) as usize)
    }

    fn recompute_matches(&mut self) {
        self.matches.clear();
        self.match_set.clear();
        if self.query.is_empty() {
            return;
        }
        let q = self.query.to_lowercase();
        for (i, line) in self.lines.iter().enumerate() {
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
        self.offset = self.matches[idx].min(self.max_offset());
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
        match self.mode {
            Mode::Search => self.key_search(code),
            Mode::Doc => self.key_doc(code),
        }
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
            KeyCode::Char('l') | KeyCode::Right => {
                self.hoffset = (self.hoffset + HSTEP).min(self.max_hoffset())
            }
            KeyCode::Char('h') | KeyCode::Left => self.hoffset = self.hoffset.saturating_sub(HSTEP),
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.query.clear();
            }
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),
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

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        self.render_doc(f, area);
        if self.mode == Mode::Search {
            self.render_search(f, area);
        }
    }

    fn render_doc(&mut self, f: &mut Frame, area: Rect) {
        let body = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", self.title));
        let inner = block.inner(body[0]);
        self.viewport_h = inner.height;

        let num_w = self.num_width();
        let gutter_w = self.gutter_w();
        let content_w = (inner.width as usize).saturating_sub(gutter_w);
        self.viewport_w = content_w as u16;

        let end = (self.offset + inner.height as usize).min(self.lines.len());
        let rows: Vec<Line> = (self.offset..end)
            .map(|gidx| {
                let gutter = Span::styled(
                    format!("{n:>num_w$} │ ", n = gidx + 1),
                    Style::default().fg(theme::palette().dim),
                );
                let mut spans = vec![gutter];
                for tok in slice_tokens(&self.hl[gidx], self.hoffset, content_w) {
                    spans.push(Span::styled(
                        tok.text,
                        Style::default().fg(theme::token_color(tok.kind)),
                    ));
                }
                // Tint matching rows, brighter for the current match (as `tui.rs`).
                if self.match_set.contains(&gidx) {
                    let bg = if self.matches.get(self.match_cur) == Some(&gidx) {
                        Color::Rgb(80, 70, 20)
                    } else {
                        Color::Rgb(50, 50, 30)
                    };
                    spans = spans
                        .into_iter()
                        .map(|s| Span::styled(s.content.clone(), s.style.bg(bg)))
                        .collect();
                }
                Line::from(spans)
            })
            .collect();

        // No wrap: one source line per row, so indices map to the scroll offset.
        let para = Paragraph::new(Text::from(rows)).block(block);
        f.render_widget(para, body[0]);

        let pct = if self.lines.len() <= 1 {
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
            " {}%  {} lines   [j/k] scroll  [h/l] pan  [/] search  [n/N] next/prev  [q] quit   {}{}",
            pct.min(100),
            self.lines.len(),
            self.lang,
            trunc,
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme::palette().dim)),
            body[1],
        );
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
            Paragraph::new(txt).style(Style::default().fg(theme::palette().doc)),
            bar,
        );
    }
}

/// Take the visible `[start, start + width)` character columns of a token row,
/// splitting tokens that straddle either edge. Columns are counted in `char`s
/// (not bytes); `width == 0` or a `start` past the row's end yields no tokens.
fn slice_tokens(tokens: &[Token], start: usize, width: usize) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    if width == 0 {
        return out;
    }
    let end = start + width;
    let mut col = 0usize; // column at which the current token begins
    for tok in tokens {
        let chars: Vec<char> = tok.text.chars().collect();
        let tok_start = col;
        let tok_end = col + chars.len();
        col = tok_end;
        // Skip tokens entirely left of or right of the window.
        if tok_end <= start || tok_start >= end {
            continue;
        }
        let s = start.max(tok_start) - tok_start;
        let e = end.min(tok_end) - tok_start;
        let text: String = chars[s..e].iter().collect();
        if !text.is_empty() {
            out.push(Token {
                text,
                kind: tok.kind,
            });
        }
    }
    out
}

/// Highlight the whole (capped) file into one token row per line, keeping a strict
/// 1:1 mapping with `lines` so a line index is also its token-row index. The
/// highlighter is run over the joined text in one pass so block-comment state
/// carries across lines (a `/* … */` spanning rows highlights correctly); the
/// result is padded to `lines.len()` since `str::lines()` yields no row for an
/// empty file.
fn highlight_all(lines: &[String], syntax: Syntax) -> Vec<Vec<Token>> {
    let mut rows = highlight::highlight(&lines.join("\n"), syntax);
    rows.resize_with(lines.len(), Vec::new);
    rows
}

/// Read a file capped at `MAX_BYTES` and `MAX_LINES`, lossy-decoded to lines.
/// The bool is true when either cap truncated the content.
fn read_capped(path: &str) -> io::Result<(Vec<String>, bool)> {
    let f = fs::File::open(path)?;
    let mut buf = Vec::new();
    // Read one extra byte so an exactly-capped file still reports truncation.
    f.take((MAX_BYTES + 1) as u64).read_to_end(&mut buf)?;
    let byte_truncated = buf.len() > MAX_BYTES;
    buf.truncate(MAX_BYTES);

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let line_truncated = lines.len() > MAX_LINES;
    lines.truncate(MAX_LINES);
    if lines.is_empty() {
        lines.push(String::new()); // always render at least one (blank) row
    }
    Ok((lines, byte_truncated || line_truncated))
}

/// Lowercased extension of a path.
fn ext_of(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Language label for the status bar: the extension when a syntax is known,
/// else "text" (plain-text types and unknown extensions).
fn language_name(ext: &str) -> String {
    if highlight::syntax_for(ext).is_some() {
        ext.to_string()
    } else {
        "text".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::TokenKind;

    fn tok(text: &str, kind: TokenKind) -> Token {
        Token {
            text: text.to_string(),
            kind,
        }
    }

    fn row() -> Vec<Token> {
        // columns: 0..3 "let", 3..4 " ", 4..7 "abc"
        vec![
            tok("let", TokenKind::Keyword),
            tok(" ", TokenKind::Plain),
            tok("abc", TokenKind::Plain),
        ]
    }

    #[test]
    fn slice_within_one_token() {
        // columns 4..6 fall inside the third token ("abc" -> "ab")
        let out = slice_tokens(&row(), 4, 2);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "ab");
        assert_eq!(out[0].kind, TokenKind::Plain);
    }

    #[test]
    fn slice_across_token_boundaries() {
        // columns 2..5 span the end of "let", the space, and start of "abc"
        let out = slice_tokens(&row(), 2, 3);
        let joined: String = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "t a");
        assert_eq!(out[0].kind, TokenKind::Keyword); // "t"
    }

    #[test]
    fn slice_start_beyond_end_is_empty() {
        assert!(slice_tokens(&row(), 100, 10).is_empty());
    }

    #[test]
    fn slice_zero_width_is_empty() {
        assert!(slice_tokens(&row(), 0, 0).is_empty());
    }

    #[test]
    fn slice_full_range_returns_all() {
        let out = slice_tokens(&row(), 0, 100);
        let joined: String = out.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(joined, "let abc");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn slice_counts_chars_not_bytes() {
        // "é" is one column but two bytes; "xy" starts at column 1.
        let r = vec![tok("é", TokenKind::Str), tok("xy", TokenKind::Plain)];
        let out = slice_tokens(&r, 1, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "x");
    }

    #[test]
    fn highlight_all_carries_block_comment_across_lines_and_stays_1to1() {
        let syntax = highlight::syntax_for("rs").unwrap();
        let lines: Vec<String> = "/* open\nstill comment\n*/ let x"
            .lines()
            .map(str::to_string)
            .collect();
        let rows = highlight_all(&lines, syntax);
        // One row per line.
        assert_eq!(rows.len(), lines.len());
        // The middle line is entirely inside the block comment.
        assert!(rows[1].iter().all(|t| t.kind == TokenKind::Comment));
        // After the block closes on line 3, `let` is a keyword again.
        assert!(rows[2]
            .iter()
            .any(|t| t.kind == TokenKind::Keyword && t.text == "let"));
    }

    #[test]
    fn highlight_all_pads_empty_file_to_one_row() {
        let rows = highlight_all(&[String::new()], highlight::PLAIN);
        assert_eq!(rows.len(), 1);
    }
}
