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
use std::fs::File;
use std::io::{self, Read};
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
    capped: bool,
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
        // Bound calamine, which materialises the whole workbook. Prechecking the
        // on-disk size caps a plain .xls outright; for the zip-based .ods/.xlsb
        // it bounds only the COMPRESSED input — calamine 0.35 exposes no
        // decompression limit, so a crafted sub-cap decompression bomb remains a
        // residual documented in ADR 0009.
        let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        if len > crate::util::MAX_DECODE_BYTES as u64 {
            return Err(format!(
                "file too large to open ({} limit)",
                crate::util::human_size(crate::util::MAX_DECODE_BYTES as u64)
            ));
        }
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
        Ok(MemBook {
            sheets,
            cur: 0,
            capped: false,
        })
    }

    /// Build an eager in-memory book from a delimited-text file (csv/tsv).
    ///
    /// Deliberate limitations, documented for future readers:
    ///   * Eager load, but bounded twice (ADR 0009): at most
    ///     `crate::util::MAX_DECODE_BYTES` bytes are read, and the parsed rows are
    ///     capped at `crate::xlsx::ROW_CAP`. Hitting either bound truncates the
    ///     sheet and reports `capped`, so a huge csv opens as an honest prefix
    ///     rather than exhausting memory.
    ///   * `.csv` is comma-only — no semicolon or delimiter auto-detection; `.tsv`
    ///     is tab-only. The delimiter is chosen by the caller from the extension.
    ///   * Decoding is lossy UTF-8 (invalid bytes become U+FFFD).
    ///
    /// An empty file is *not* an error: it opens as a sheet with zero rows. Only a
    /// filesystem read failure returns `Err`.
    fn from_csv(path: &str, delim: char) -> Result<Self, String> {
        // Read at most MAX_DECODE_BYTES (+1 to detect overflow): past the cap we
        // keep the prefix and mark `capped`, mirroring the row cap below rather
        // than erroring, so a huge csv opens truncated-but-honest.
        let mut bytes = Vec::new();
        File::open(path)
            .map_err(|e| e.to_string())?
            .take(crate::util::MAX_DECODE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        let byte_capped = bytes.len() > crate::util::MAX_DECODE_BYTES;
        if byte_capped {
            bytes.truncate(crate::util::MAX_DECODE_BYTES);
        }
        let text = String::from_utf8_lossy(&bytes);
        let mut rows = parse_delimited(&text, delim);
        let row_capped = rows.len() > crate::xlsx::ROW_CAP;
        if row_capped {
            rows.truncate(crate::xlsx::ROW_CAP);
        }
        let capped = byte_capped || row_capped;
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let name = std::path::Path::new(path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Sheet1".to_string());
        Ok(MemBook {
            sheets: vec![MemSheet { name, rows, ncols }],
            cur: 0,
            capped,
        })
    }
}

/// RFC-4180-style parser for delimited text — pure (no IO), so it is unit-tested
/// directly. Fields are separated by `delim` and records by `\n`. A field may be
/// double-quoted; inside quotes `delim`, newlines and `\r` are literal, and a
/// doubled `""` is an escaped quote. A quote is only special at the start of a
/// field. On an unquoted field end a trailing `\r` is stripped, so CRLF line
/// endings are tolerated. A final line without a trailing newline still yields a
/// record; a trailing newline does not add an empty record. Rows may be ragged
/// (differing field counts) and are preserved as-is — the grid tolerates it.
fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_start = true; // at the first char of the current field?
    let mut pending = false; // has a field/record been started but not flushed?

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next(); // escaped literal quote
                    field.push('"');
                } else {
                    in_quotes = false; // closing quote
                }
            } else {
                field.push(c); // delim, newline, \r all literal in quotes
            }
            continue;
        }
        if c == '"' && field_start {
            in_quotes = true;
            field_start = false;
            pending = true;
        } else if c == delim {
            row.push(std::mem::take(&mut field));
            field_start = true;
            pending = true;
        } else if c == '\n' {
            if field.ends_with('\r') {
                field.pop(); // CRLF: drop the record-terminating \r
            }
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
            field_start = true;
            pending = false;
        } else {
            field.push(c);
            field_start = false;
            pending = true;
        }
    }
    // Flush a trailing record with no final newline; an empty input flushes nothing.
    if pending || !row.is_empty() {
        if field.ends_with('\r') {
            field.pop();
        }
        row.push(field);
        rows.push(row);
    }
    rows
}

// ---- unified book ----

/// True for the data-file extensions the DuckDB backend owns (ADR 0016). Kept in
/// one place so `Book::open` and `preview_grid` route identically.
#[cfg(feature = "data")]
fn is_data_ext(lower: &str) -> bool {
    [
        ".parquet", ".pq", ".jsonl", ".ndjson", ".sqlite", ".sqlite3", ".db", ".db3", ".duckdb",
        ".ddb",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
}

enum Book {
    Stream(StreamBook),
    Mem(MemBook),
    // The DuckDB-backed data-file book (ADR 0016) — Parquet, JSONL, SQLite,
    // DuckDB. Feature-gated: without `data` these extensions never reach here.
    #[cfg(feature = "data")]
    Data(crate::data::DataBook),
}

impl Book {
    fn open(path: &str) -> Result<Book, String> {
        let lower = path.to_lowercase();
        if lower.ends_with(".xlsx") || lower.ends_with(".xlsm") {
            Ok(Book::Stream(StreamBook::open(path)?))
        } else if lower.ends_with(".csv") {
            // Tabular text → the eager MemBook; calamine cannot read CSV.
            Ok(Book::Mem(MemBook::from_csv(path, ',')?))
        } else if lower.ends_with(".tsv") {
            Ok(Book::Mem(MemBook::from_csv(path, '\t')?))
        } else {
            // Data files (ADR 0016) route to the DuckDB backend before the final
            // calamine fall-through; without the `data` feature they never match.
            #[cfg(feature = "data")]
            if is_data_ext(&lower) {
                return Ok(Book::Data(crate::data::DataBook::open(path)?));
            }
            Ok(Book::Mem(MemBook::open(path)?))
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Book::Stream(b) => b.names(),
            Book::Mem(b) => b.sheets.iter().map(|s| s.name.clone()).collect(),
            #[cfg(feature = "data")]
            Book::Data(b) => b.names(),
        }
    }

    fn selected(&self) -> usize {
        match self {
            Book::Stream(b) => b.selected(),
            Book::Mem(b) => b.cur,
            #[cfg(feature = "data")]
            Book::Data(b) => b.selected(),
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
            #[cfg(feature = "data")]
            Book::Data(b) => b.select(idx),
        }
    }

    /// (rows_loaded, ncols, done, capped)
    fn dims(&self) -> (usize, usize, bool, bool) {
        match self {
            Book::Stream(b) => b.dims(),
            Book::Mem(b) => {
                let s = &b.sheets[b.cur];
                (s.rows.len(), s.ncols, true, b.capped)
            }
            #[cfg(feature = "data")]
            Book::Data(b) => b.dims(),
        }
    }

    /// Real column names for backends that have them (the data book), so the grid
    /// can show named headers instead of A/B/C; `None` keeps the letter headers
    /// for the spreadsheet backends.
    fn headers(&self) -> Option<Vec<String>> {
        match self {
            Book::Stream(_) | Book::Mem(_) => None,
            #[cfg(feature = "data")]
            Book::Data(b) => Some(b.headers()),
        }
    }

    // `&mut self`: the data book windows lazily and caches its last fetch, so a
    // window read can mutate. The eager/streaming backends ignore the mutability.
    fn window(&mut self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
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
            #[cfg(feature = "data")]
            Book::Data(b) => b.window(r0, r1, c0, c1),
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
            #[cfg(feature = "data")]
            Book::Data(b) => b.find(query),
        }
    }

    /// Whether this backend accepts the `:` SQL prompt — the grid's first
    /// capability that varies by backend (ADR 0016). Only the DuckDB `Data` book
    /// is queryable; the spreadsheet backends are not.
    fn supports_sql(&self) -> bool {
        match self {
            Book::Stream(_) | Book::Mem(_) => false,
            #[cfg(feature = "data")]
            Book::Data(_) => true,
        }
    }

    /// Run a `:` query against the active sheet, replacing its view with the
    /// result (or clearing the override on empty input). Delegates to the `Data`
    /// book; kept total for the other backends, which never reach here because
    /// the key is gated on `supports_sql`. (Without the `data` feature only the
    /// non-queryable arms remain, so `sql` goes unread — allowed explicitly.)
    #[cfg_attr(not(feature = "data"), allow(unused_variables))]
    fn set_sql(&mut self, sql: &str) -> Result<(), String> {
        match self {
            Book::Stream(_) | Book::Mem(_) => Err("this file type is not queryable".into()),
            #[cfg(feature = "data")]
            Book::Data(b) => b.set_sql(sql),
        }
    }

    /// The running `:` query for the active sheet, if any — for the status line's
    /// live-query indicator. `None` for the non-queryable backends.
    fn active_sql(&self) -> Option<String> {
        match self {
            Book::Stream(_) | Book::Mem(_) => None,
            #[cfg(feature = "data")]
            Book::Data(b) => b.active_sql().map(str::to_string),
        }
    }
}

// ---- app ----

pub struct SheetApp {
    title: String,
    path: String,
    book: Book,
    sel_row: usize,
    sel_col: usize,
    row_off: usize,
    col_off: usize,
    searching: bool,
    query: String,
    matches: Vec<(usize, usize)>,
    match_idx: usize,
    // SQL prompt (ADR 0016), mirroring the `/` search prompt above. Only ever
    // active on a `Data` book (the `:` key is gated on `book.supports_sql()`).
    sql_editing: bool,
    sql_input: String,
    // A transient one-line note (a DuckDB error, or "query ok") shown in the
    // status bar until the next navigation or query clears it.
    status_msg: Option<String>,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let book = match Book::open(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sucher: {path}: {e}");
            return Ok(());
        }
    };
    let mut app = SheetApp {
        title,
        path,
        book,
        sel_row: 0,
        sel_col: 0,
        row_off: 0,
        col_off: 0,
        searching: false,
        query: String::new(),
        matches: Vec::new(),
        match_idx: 0,
        sql_editing: false,
        sql_input: String::new(),
        status_msg: None,
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
        Err(e) => return format!("sucher: {path}: {e}\n"),
    };
    let mut out = String::new();
    for (i, name) in names.iter().enumerate() {
        let mut b = match Book::open(path) {
            Ok(b) => b,
            Err(e) => return format!("sucher: {path}: {e}\n"),
        };
        b.select(i);
        loop {
            if b.dims().2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let (rows, ncols, _, capped) = b.dims();
        // Bound the dump uniformly for EVERY backend: the data book reports an
        // uncapped real count, so cap the materialised rows at ROW_CAP here to
        // keep a pipe of a billion-row Parquet from exhausting memory — an honest
        // cap consistent with how the streaming/CSV backends already truncate.
        let dump_rows = rows.min(crate::xlsx::ROW_CAP);
        out.push_str(&format!("# {name}\n"));
        for row in b.window(0, dump_rows, 0, ncols) {
            out.push_str(&row.join("\t"));
            out.push('\n');
        }
        if capped || rows > dump_rows {
            out.push_str(&format!("(… truncated at {dump_rows} rows)\n"));
        }
        out.push('\n');
    }
    out
}

/// First rows/cols of a spreadsheet for the directory preview pane — bounded and
/// synchronous, so it never blocks on the streaming loader. csv/tsv are parsed
/// straight from the file; other formats (xlsx/xls/ods/xlsb) go through calamine
/// for their first worksheet only. Returns `None` on error or an empty sheet.
pub fn preview_grid(path: &str, max_rows: usize, max_cols: usize) -> Option<Vec<Vec<String>>> {
    let lower = path.to_lowercase();
    // Data files (ADR 0016): open the DuckDB book and take its first rows, with
    // the real column names prepended as a header row so the preview is
    // meaningful. Bounded and synchronous — DuckDB reads only the window asked
    // for. Any error (bad file, missing table) degrades to no preview.
    #[cfg(feature = "data")]
    if is_data_ext(&lower) {
        let mut db = crate::data::DataBook::open(path).ok()?;
        let (_, ncols, _, _) = db.dims();
        let cols = ncols.min(max_cols);
        let mut rows: Vec<Vec<String>> = Vec::new();
        rows.push(db.headers().into_iter().take(cols).collect());
        for row in db.window(0, max_rows, 0, cols) {
            rows.push(row);
        }
        rows.truncate(max_rows);
        return if rows.is_empty() { None } else { Some(rows) };
    }
    let mut rows: Vec<Vec<String>> = if lower.ends_with(".csv") || lower.ends_with(".tsv") {
        let delim = if lower.ends_with(".tsv") { '\t' } else { ',' };
        // A preview may legitimately show only a prefix, so read at most
        // MAX_PREVIEW_BYTES and parse what we got rather than erroring (ADR 0009).
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take(crate::util::MAX_PREVIEW_BYTES as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        parse_delimited(&String::from_utf8_lossy(&bytes), delim)
    } else if lower.ends_with(".xlsx") || lower.ends_with(".xlsm") {
        // Bounded, synchronous first-rows read — never materialises the whole
        // sheet the way calamine's `worksheet_range` would (ADR 0009).
        crate::xlsx::preview_rows(path, max_rows, max_cols).ok()?
    } else {
        // xls/ods/xlsb via calamine, which materialises the whole sheet. Precheck
        // the on-disk size: a plain .xls is bounded outright; for the zip-based
        // .ods/.xlsb this bounds only the COMPRESSED input, since calamine 0.35
        // exposes no decompression limit — a sub-cap bomb is a residual noted in
        // ADR 0009.
        let len = std::fs::metadata(path).ok()?.len();
        if len > crate::util::MAX_DECODE_BYTES as u64 {
            return None;
        }
        let mut wb = open_workbook_auto(path).ok()?;
        let name = wb.sheet_names().first()?.clone();
        let range = wb.worksheet_range(&name).ok()?;
        range
            .rows()
            .take(max_rows)
            .map(|r| r.iter().map(fmt_cell).collect())
            .collect()
    };
    rows.truncate(max_rows);
    for r in &mut rows {
        r.truncate(max_cols);
    }
    if rows.is_empty() {
        None
    } else {
        Some(rows)
    }
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
        if self.sql_editing {
            return self.key_sql(code);
        }
        // Any normal-mode key clears a transient status note (a prior error or
        // "query ok"), so it lasts only until the user next does something.
        self.status_msg = None;
        let (nrows, ncols, _, _) = self.book.dims();
        let maxr = nrows.saturating_sub(1);
        let maxc = ncols.saturating_sub(1);
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('x') => crate::util::open_in_native_app(&self.path),
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
            // The SQL prompt is available only on a queryable (Data) book. Seed
            // the editor with the running query so the user edits it in place.
            KeyCode::Char(':') if self.book.supports_sql() => {
                self.sql_editing = true;
                self.sql_input = self.book.active_sql().unwrap_or_default();
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

    /// The `:` SQL prompt, paralleling `key_search`. `Esc` cancels (view
    /// unchanged); `Enter` runs the query — on success the cursor and offsets
    /// reset and matches clear, on a bind/parse error we STAY in the prompt with
    /// the user's text intact so they can fix it. There is no query history.
    fn key_sql(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Esc => self.sql_editing = false,
            KeyCode::Enter => match self.book.set_sql(&self.sql_input) {
                Ok(()) => {
                    // The view was replaced: reset the viewport and drop stale
                    // search hits, which pointed into the previous relation.
                    self.sel_row = 0;
                    self.sel_col = 0;
                    self.row_off = 0;
                    self.col_off = 0;
                    self.matches.clear();
                    self.match_idx = 0;
                    self.status_msg = Some("query ok".into());
                    self.sql_editing = false;
                }
                Err(e) => {
                    // Keep the prompt open with the text so the user can correct
                    // it (or Esc out); the previous view is intact underneath.
                    self.status_msg = Some(e);
                }
            },
            KeyCode::Backspace => {
                self.sql_input.pop();
            }
            KeyCode::Char(c) => self.sql_input.push(c),
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
        let win = self
            .book
            .window(self.row_off, row_end, self.col_off, col_end);

        let header_style = Style::default()
            .fg(Color::Rgb(125, 211, 252))
            .add_modifier(Modifier::BOLD);
        let gutter_style = Style::default().fg(Color::Rgb(110, 110, 122));

        // Data files carry real column names; spreadsheets keep the A/B/C letters.
        let headers = self.book.headers();
        let mut lines: Vec<Line> = Vec::new();
        let mut hdr = vec![Span::styled(" ".repeat(GUTTER as usize), gutter_style)];
        for c in self.col_off..col_end {
            let label = headers
                .as_ref()
                .and_then(|h| h.get(c).cloned())
                .unwrap_or_else(|| col_name(c));
            hdr.push(Span::styled(center(&label, COL_W as usize), header_style));
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

    fn render_status(&mut self, f: &mut Frame, area: Rect) {
        let (nrows, _, done, capped) = self.book.dims();
        // Use the real column name under the cursor when the backend has one
        // (data files), else the synthesised A/B/C letter (spreadsheets).
        let col_label = self
            .book
            .headers()
            .and_then(|h| h.get(self.sel_col).cloned())
            .unwrap_or_else(|| col_name(self.sel_col));
        let reff = format!("{}{}", col_label, self.sel_row + 1);
        let val = self
            .book
            .window(
                self.sel_row,
                self.sel_row + 1,
                self.sel_col,
                self.sel_col + 1,
            )
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

        // SQL prompt takes over the bar while editing (ADR 0016). On a bind/parse
        // error we stay here with `status_msg` set to the DuckDB message, so it is
        // shown in place of the key hint until the query is fixed or cancelled.
        if self.sql_editing {
            let tail = match &self.status_msg {
                Some(m) => format!("    {m}"),
                None => "    Enter=run  Esc=cancel".to_string(),
            };
            let bar = format!(":{}{}", self.sql_input, tail);
            f.render_widget(
                Paragraph::new(bar).style(Style::default().fg(Color::Rgb(252, 211, 77))),
                area,
            );
            return;
        }

        let find = if self.matches.is_empty() {
            String::new()
        } else {
            format!(
                "  match {}/{}{}",
                self.match_idx + 1,
                self.matches.len(),
                if done { "" } else { " so far" }
            )
        };
        let hint = if self.book.supports_sql() {
            "[/] search [n/N]  [:] query  [hjkl] move  [Tab] sheet  [x] open  [q] quit"
        } else {
            "[/] search [n/N]  [hjkl] move  [Tab] sheet  [x] open  [q] quit"
        };
        // A live `:` query is a lens over the raw table: show it (truncated) so the
        // user knows they are not looking at the source, plus any transient note.
        let live = self
            .book
            .active_sql()
            .map(|q| format!("[:{}] ", truncate(&q, 40)))
            .unwrap_or_default();
        let note = match &self.status_msg {
            Some(m) => format!("  {m}"),
            None => String::new(),
        };
        let text = format!(" {live}{reff}: {val}    {load}{find}{note}");
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(hint.len() as u16 + 1),
            ])
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

#[cfg(test)]
mod tests {
    use super::{parse_delimited, preview_grid};
    use std::io::Write;

    fn csv(text: &str) -> Vec<Vec<String>> {
        parse_delimited(text, ',')
    }

    #[test]
    fn preview_grid_reads_and_caps_csv() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sucher-sheettest-{}.csv", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        // 4 rows, 3 columns.
        f.write_all(b"a,b,c\n1,2,3\n4,5,6\n7,8,9\n").unwrap();
        drop(f);

        // Cap to 2 rows and 2 columns.
        let g = preview_grid(path.to_str().unwrap(), 2, 2).expect("some rows");
        assert_eq!(g.len(), 2, "row cap");
        assert!(g.iter().all(|r| r.len() <= 2), "col cap");
        assert_eq!(g[0], vec!["a", "b"]);
        assert_eq!(g[1], vec!["1", "2"]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn preview_grid_reads_and_caps_xlsx() {
        // Routes through the bounded xlsx::preview_rows, not calamine's full
        // materialisation. sample.xlsx sheet1 is 5x4.
        let g = preview_grid("samples/sample.xlsx", 3, 2).expect("some rows");
        assert_eq!(g.len(), 3, "row cap");
        assert!(g.iter().all(|r| r.len() <= 2), "col cap");
        assert_eq!(g[0], vec!["Item", "Qty"]);
    }

    #[test]
    fn simple_rows() {
        assert_eq!(
            csv("a,b,c\nd,e,f\n"),
            vec![vec!["a", "b", "c"], vec!["d", "e", "f"]]
        );
    }

    #[test]
    fn quoted_embedded_delimiter() {
        assert_eq!(csv("\"a,b\",c"), vec![vec!["a,b", "c"]]);
    }

    #[test]
    fn quoted_embedded_newline() {
        assert_eq!(csv("\"a\nb\",c\n"), vec![vec!["a\nb", "c"]]);
    }

    #[test]
    fn escaped_doubled_quote() {
        // Input: "a""b"  ->  a"b
        assert_eq!(csv("\"a\"\"b\""), vec![vec!["a\"b"]]);
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(csv("a,b\r\nc,d\r\n"), vec![vec!["a", "b"], vec!["c", "d"]]);
        // A \r inside quotes stays literal.
        assert_eq!(csv("\"a\r\nb\"\r\n"), vec![vec!["a\r\nb"]]);
    }

    #[test]
    fn trailing_newline_vs_none() {
        // No trailing newline: last line still yields a record.
        assert_eq!(csv("a,b\nc,d"), vec![vec!["a", "b"], vec!["c", "d"]]);
        // Trailing newline: no extra empty record.
        assert_eq!(csv("a,b\n"), vec![vec!["a", "b"]]);
    }

    #[test]
    fn ragged_rows_preserved() {
        assert_eq!(
            csv("a,b,c\nd\ne,f\n"),
            vec![vec!["a", "b", "c"], vec!["d"], vec!["e", "f"]]
        );
    }

    #[test]
    fn empty_field_and_trailing_delimiter() {
        assert_eq!(csv("a,,c"), vec![vec!["a", "", "c"]]);
        assert_eq!(csv("a,"), vec![vec!["a", ""]]);
        // A lone quoted empty field still yields a (single, empty) cell.
        assert_eq!(csv("\"\""), vec![vec![""]]);
    }

    #[test]
    fn empty_input_yields_no_rows() {
        assert_eq!(csv(""), Vec::<Vec<String>>::new());
    }

    #[test]
    fn tab_delimiter_for_tsv() {
        assert_eq!(
            parse_delimited("a\tb\tc\n1\t2\t3\n", '\t'),
            vec![vec!["a", "b", "c"], vec!["1", "2", "3"]]
        );
        // A comma is just data under the tab delimiter.
        assert_eq!(parse_delimited("a,b\tc\n", '\t'), vec![vec!["a,b", "c"]]);
    }
}
