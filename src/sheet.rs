// Spreadsheet grid viewer. Two backends behind a `Book`:
//   * xlsx/xlsm  -> streaming reader on a background thread (xlsx.rs), so even
//                   multi-hundred-MB workbooks open instantly and stay
//                   responsive while rows load (capped).
//   * xls/ods/xlsb -> calamine, loaded eagerly (these are typically small).
// The grid renders only the visible window and tracks the cursor.

use crate::xlsx::StreamBook;
use calamine::{open_workbook_auto, Data, Reader};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use std::io;
use std::time::Duration;

const COL_W: u16 = 14;
const GUTTER: u16 = 6;

// ---- calamine-backed eager book (xls/ods/xlsb) ----

struct MemSheet {
    name: String,
    rows: Vec<Vec<String>>,
    ncols: usize,
}

struct MemBook {
    sheets: Vec<MemSheet>,
    cur: usize,
}

fn fmt_cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::Error(e) => format!("#{e:?}"),
        other => other.to_string(),
    }
}

impl MemBook {
    fn open(path: &str) -> Result<Self, String> {
        let mut wb = open_workbook_auto(path).map_err(|e| e.to_string())?;
        let names = wb.sheet_names().to_owned();
        let mut sheets = Vec::new();
        for name in names {
            let range = wb.worksheet_range(&name).map_err(|e| e.to_string())?;
            let rows: Vec<Vec<String>> = range
                .rows()
                .map(|r| r.iter().map(fmt_cell).collect())
                .collect();
            let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            sheets.push(MemSheet { name, rows, ncols });
        }
        if sheets.is_empty() {
            return Err("workbook has no sheets".into());
        }
        Ok(MemBook { sheets, cur: 0 })
    }
}

// ---- unified book ----

enum Book {
    Stream(StreamBook),
    Mem(MemBook),
}

impl Book {
    fn open(path: &str) -> Result<Book, String> {
        let lower = path.to_lowercase();
        if lower.ends_with(".xlsx") || lower.ends_with(".xlsm") {
            Ok(Book::Stream(StreamBook::open(path)?))
        } else {
            Ok(Book::Mem(MemBook::open(path)?))
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Book::Stream(b) => b.names(),
            Book::Mem(b) => b.sheets.iter().map(|s| s.name.clone()).collect(),
        }
    }

    fn selected(&self) -> usize {
        match self {
            Book::Stream(b) => b.selected(),
            Book::Mem(b) => b.cur,
        }
    }

    fn select(&mut self, idx: usize) {
        match self {
            Book::Stream(b) => b.select(idx),
            Book::Mem(b) => {
                if idx < b.sheets.len() {
                    b.cur = idx;
                }
            }
        }
    }

    /// (rows_loaded, ncols, done, capped)
    fn dims(&self) -> (usize, usize, bool, bool) {
        match self {
            Book::Stream(b) => b.dims(),
            Book::Mem(b) => {
                let s = &b.sheets[b.cur];
                (s.rows.len(), s.ncols, true, false)
            }
        }
    }

    fn window(&self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        match self {
            Book::Stream(b) => b.window(r0, r1, c0, c1),
            Book::Mem(b) => {
                let s = &b.sheets[b.cur];
                (r0..r1.min(s.rows.len()))
                    .map(|r| {
                        (c0..c1)
                            .map(|c| s.rows[r].get(c).cloned().unwrap_or_default())
                            .collect()
                    })
                    .collect()
            }
        }
    }

    fn find(&self, query: &str) -> Vec<(usize, usize)> {
        match self {
            Book::Stream(b) => b.find(query),
            Book::Mem(b) => {
                let needle = query.to_ascii_lowercase();
                let s = &b.sheets[b.cur];
                let mut hits = Vec::new();
                for (r, row) in s.rows.iter().enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        if crate::xlsx::contains_ci(cell, &needle) {
                            hits.push((r, c));
                        }
                    }
                }
                hits
            }
        }
    }
}

// ---- app ----

pub struct SheetApp {
    title: String,
    book: Book,
    sel_row: usize,
    sel_col: usize,
    row_off: usize,
    col_off: usize,
    searching: bool,
    query: String,
    matches: Vec<(usize, usize)>,
    match_idx: usize,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let book = match Book::open(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("vellum: {path}: {e}");
            return Ok(());
        }
    };
    let mut app = SheetApp {
        title,
        book,
        sel_row: 0,
        sel_col: 0,
        row_off: 0,
        col_off: 0,
        searching: false,
        query: String::new(),
        matches: Vec::new(),
        match_idx: 0,
    };
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// Non-interactive dump (waits for streaming to finish, then TSV).
pub fn dump(path: &str) -> String {
    let names = match Book::open(path) {
        Ok(b) => b.names(),
        Err(e) => return format!("vellum: {path}: {e}\n"),
    };
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        let mut b = match Book::open(path) {
            Ok(b) => b,
            Err(e) => return format!("vellum: {path}: {e}\n"),
        };
        b.select(i);
        loop {
            if b.dims().2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let (rows, ncols, _, capped) = b.dims();
        out.push_str(&format!("# {name}\n"));
        for row in b.window(0, rows, 0, ncols) {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        if capped {
            out.push_str(&format!("(… truncated at {rows} rows)\n"));
        }
        out.push('\n');
    }
    out
}

fn col_name(mut i: usize) -> String {
    let mut s = String::new();
    loop {
        s.insert(0, (b'A' + (i % 26) as u8) as char);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s
}

impl SheetApp {
    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }
            // While the background loader is still streaming, tick to show new
            // rows; once done, only redraw on input.
            let (_, _, done, _) = self.book.dims();
            let timeout = if done { 1000 } else { 120 };
            if event::poll(Duration::from_millis(timeout))? {
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
            } else if !done {
                dirty = true;
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) -> bool {
        if self.searching {
            return self.key_search(code);
        }
        let (nrows, ncols, _, _) = self.book.dims();
        let maxr = nrows.saturating_sub(1);
        let maxc = ncols.saturating_sub(1);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('j') | KeyCode::Down => self.sel_row = (self.sel_row + 1).min(maxr),
            KeyCode::Char('k') | KeyCode::Up => self.sel_row = self.sel_row.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => self.sel_col = (self.sel_col + 1).min(maxc),
            KeyCode::Char('h') | KeyCode::Left => self.sel_col = self.sel_col.saturating_sub(1),
            KeyCode::PageDown => self.sel_row = (self.sel_row + 20).min(maxr),
            KeyCode::PageUp => self.sel_row = self.sel_row.saturating_sub(20),
            KeyCode::Char('g') | KeyCode::Home => self.sel_row = 0,
            KeyCode::Char('G') | KeyCode::End => self.sel_row = maxr,
            KeyCode::Tab | KeyCode::Char(']') => self.switch(1),
            KeyCode::BackTab | KeyCode::Char('[') => self.switch(-1),
            KeyCode::Char('/') => {
                self.searching = true;
                self.query.clear();
            }
            KeyCode::Char('n') => self.cycle_match(1),
            KeyCode::Char('N') => self.cycle_match(-1),
            _ => {}
        }
        false
    }

    fn key_search(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc => self.searching = false,
            KeyCode::Enter => {
                self.run_search();
                self.searching = false;
            }
            KeyCode::Backspace => {
                self.query.pop();
            }
            KeyCode::Char(c) => self.query.push(c),
            _ => {}
        }
        false
    }

    fn run_search(&mut self) {
        if self.query.is_empty() {
            self.matches.clear();
            return;
        }
        self.matches = self.book.find(&self.query);
        self.match_idx = 0;
        if let Some(&(r, c)) = self.matches.first() {
            self.sel_row = r;
            self.sel_col = c;
        }
    }

    fn cycle_match(&mut self, dir: i32) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len() as i32;
        self.match_idx = (((self.match_idx as i32 + dir) % n) + n) as usize % n as usize;
        let (r, c) = self.matches[self.match_idx];
        self.sel_row = r;
        self.sel_col = c;
    }

    fn switch(&mut self, dir: i32) {
        let names = self.book.names();
        let n = names.len() as i32;
        if n == 0 {
            return;
        }
        let cur = self.book.selected() as i32;
        let next = (((cur + dir) % n) + n) % n;
        self.book.select(next as usize);
        self.sel_row = 0;
        self.sel_col = 0;
        self.row_off = 0;
        self.col_off = 0;
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);
        self.render_tabs(f, chunks[0]);
        self.render_grid(f, chunks[1]);
        self.render_status(f, chunks[2]);
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let names = self.book.names();
        let cur = self.book.selected();
        let mut spans = vec![Span::styled(
            format!(" {} ", self.title),
            Style::default().fg(Color::Rgb(110, 110, 122)),
        )];
        for (i, name) in names.iter().enumerate() {
            let st = if i == cur {
                Style::default()
                    .fg(Color::Rgb(125, 211, 252))
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(Color::Rgb(160, 160, 170))
            };
            spans.push(Span::styled(format!(" {name} "), st));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_grid(&mut self, f: &mut Frame, area: Rect) {
        let (nrows, ncols, _, _) = self.book.dims();
        let body_h = area.height.saturating_sub(1) as usize;
        let vis_cols = ((area.width.saturating_sub(GUTTER)) / COL_W).max(1) as usize;

        if self.sel_row < self.row_off {
            self.row_off = self.sel_row;
        } else if self.sel_row >= self.row_off + body_h {
            self.row_off = self.sel_row + 1 - body_h;
        }
        if self.sel_col < self.col_off {
            self.col_off = self.sel_col;
        } else if self.sel_col >= self.col_off + vis_cols {
            self.col_off = self.sel_col + 1 - vis_cols;
        }

        let col_end = (self.col_off + vis_cols).min(ncols.max(1));
        let row_end = (self.row_off + body_h).min(nrows);
        let win = self.book.window(self.row_off, row_end, self.col_off, col_end);

        let header_style = Style::default()
            .fg(Color::Rgb(125, 211, 252))
            .add_modifier(Modifier::BOLD);
        let gutter_style = Style::default().fg(Color::Rgb(110, 110, 122));

        let mut lines: Vec<Line> = Vec::new();
        let mut hdr = vec![Span::styled(" ".repeat(GUTTER as usize), gutter_style)];
        for c in self.col_off..col_end {
            hdr.push(Span::styled(center(&col_name(c), COL_W as usize), header_style));
        }
        lines.push(Line::from(hdr));

        for (ri, row) in win.iter().enumerate() {
            let r = self.row_off + ri;
            let mut spans = vec![Span::styled(
                format!("{:>width$} ", r + 1, width = GUTTER as usize - 1),
                gutter_style,
            )];
            for (ci, cell) in row.iter().enumerate() {
                let c = self.col_off + ci;
                let mut st = Style::default().fg(Color::Rgb(220, 220, 228));
                if r == self.sel_row && c == self.sel_col {
                    st = st.add_modifier(Modifier::REVERSED);
                }
                spans.push(Span::styled(pad(cell, COL_W as usize), st));
            }
            lines.push(Line::from(spans));
        }

        f.render_widget(Paragraph::new(lines), area);
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        let (nrows, _, done, capped) = self.book.dims();
        let reff = format!("{}{}", col_name(self.sel_col), self.sel_row + 1);
        let val = self
            .book
            .window(self.sel_row, self.sel_row + 1, self.sel_col, self.sel_col + 1)
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .unwrap_or_default();

        let load = if capped {
            format!("{nrows} rows (capped)")
        } else if done {
            format!("{nrows} rows")
        } else {
            format!("{nrows} rows — loading…")
        };

        // Search input takes over the bar while typing.
        if self.searching {
            let loading = if done { "" } else { " (loading…)" };
            let bar = format!("/{}{}    Enter=search  Esc=cancel", self.query, loading);
            f.render_widget(
                Paragraph::new(bar).style(Style::default().fg(Color::Rgb(252, 211, 77))),
                area,
            );
            return;
        }

        let find = if self.matches.is_empty() {
            String::new()
        } else {
            format!("  match {}/{}{}", self.match_idx + 1, self.matches.len(), if done { "" } else { " so far" })
        };
        let hint = "[/] search [n/N]  [hjkl] move  [Tab] sheet  [q] quit";
        let text = format!(" {reff}: {val}    {load}{find}");
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(hint.len() as u16 + 1)])
            .split(area);
        f.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::Rgb(252, 211, 77))),
            chunks[0],
        );
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::Rgb(110, 110, 122))),
            chunks[1],
        );
    }
}

fn center(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w {
        return truncate(s, w);
    }
    let left = (w - len) / 2;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(w - len - left))
}

fn pad(s: &str, w: usize) -> String {
    let content = w.saturating_sub(1);
    let t = truncate(s, content);
    let len = t.chars().count();
    format!("{t}{} ", " ".repeat(content.saturating_sub(len)))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}
