// Data-file backend for the grid viewer (ADR 0016): Parquet, JSONL/NDJSON,
// SQLite, DuckDB — all read through ONE embedded DuckDB, the reference
// SQL-on-files engine. This is the grid's third `Book` shape, alongside the
// eager calamine `MemBook` and the capped-streaming `StreamBook`.
//
// Three design decisions carry this module; each is deliberate, not incidental:
//
//   * Offline is enforced, not assumed. DuckDB's format readers are loadable
//     extensions that DuckDB will, by default, AUTO-INSTALL over the network on
//     first use. sucher is a local viewer that must never phone home, so every
//     connection runs two pragmas immediately after opening —
//       SET autoinstall_known_extensions = false;  -- never touch the network
//       SET autoload_known_extensions   = true;    -- load the bundled ones
//     `autoinstall = false` is the offline guarantee in one line: a reader that
//     is somehow not statically bundled fails loudly instead of downloading.
//     The spike (ADR 0016) proved every reader we need is in the `bundled`
//     build, so read_parquet / read_json_auto / the SQLite scanner / native
//     DuckDB ATTACH all load from the binary, not the wire.
//
//   * Schema without executing, values via CAST. The grid is a grid of strings.
//     Rather than mirror DuckDB's logical-type lattice in Rust (fragile — the
//     spike hit internal panics reading typed values via `ValueRef`), we take
//     the schema from `DESCRIBE <relation>` (which binds but does NOT run the
//     query — instant even on a billion-row Parquet) and read each cell as
//     `CAST(col AS VARCHAR)` → `Option<String>` (NULL → None → ""). DuckDB's
//     canonical text rendering gives correct ISO dates/timestamps for free.
//
//   * Lazy, uncapped windowing. DuckDB is a fast lazy query engine, neither
//     eager (MemBook) nor worth capping (StreamBook), so forcing it into either
//     mold would be a workaround. The `Data` book windows on demand —
//     `… LIMIT <len> OFFSET <start>` around the visible range, with a prefetch
//     margin cached so ordinary scrolling is a cache hit — and reports the real
//     `COUNT(*)` as its row total. The grid can therefore scroll an arbitrarily
//     large file with no row cap: a genuine improvement over the xlsx/CSV paths,
//     and the shape that fits the engine instead of fighting it.
//
// Read-only throughout: databases are attached `READ_ONLY`. Multi-relation
// sources (SQLite/DuckDB) map each table onto a grid "sheet"; single-relation
// files (Parquet, JSONL) are one sheet named by the file stem, exposed as a view.

use duckdb::Connection;

/// Rows fetched around the visible range on either side of a viewport miss, so
/// ordinary line-by-line scrolling stays a cache hit rather than re-querying.
const MARGIN: usize = 200;

/// Upper bound on search hits, so `find` on a huge relation stays responsive.
/// Search is a convenience; a truncated hit list is acceptable (ADR 0016).
const FIND_CAP: usize = 10_000;

/// The four data-file families, detected from the file extension. Each maps to a
/// distinct DuckDB registration path (a view over a reader, or an ATTACH).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SourceKind {
    Parquet,
    Json,
    Sqlite,
    DuckDb,
}

/// One grid "sheet": a table (SQLite/DuckDB) or the single view over a
/// Parquet/JSONL file. `relation` is a SQL statement the window/count/describe
/// queries wrap in a subquery — `SELECT * FROM "<name>"` or `SELECT * FROM db."<table>"`.
struct Sheet {
    name: String,
    relation: String,
}

/// A window of consecutive rows (all columns) cached around the last viewport, so
/// the common case — scrolling within the prefetched margin — needs no query.
struct WindowCache {
    /// 0-based index of the first cached row within the active relation.
    start: usize,
    /// Fully-materialised cells (every column) for `start .. start + rows.len()`.
    rows: Vec<Vec<String>>,
}

/// The DuckDB-backed book. Owns the connection for its whole lifetime (views and
/// attachments live in it), the list of sheets, and — for the active sheet only —
/// the cached schema, total row count, and the last window.
pub struct DataBook {
    conn: Connection,
    sheets: Vec<Sheet>,
    cur: usize,
    /// The `:` prompt's query lens over the ACTIVE sheet, or `None` to show the
    /// base table/view. The tabs ARE the source relations; this is a transient
    /// override on the current one, cleared whenever the active sheet changes.
    override_sql: Option<String>,
    /// Active sheet schema: `(column_name, column_type)` from `DESCRIBE`.
    schema: Vec<(String, String)>,
    /// Active sheet real row count (`COUNT(*)`), cached — this powers lazy scroll.
    total: usize,
    cache: Option<WindowCache>,
}

impl DataBook {
    /// Open a data file: detect its family from the extension, connect, enforce
    /// offline mode, register the source (view or ATTACH), build the sheet list,
    /// and load the active sheet's schema + count. A bad/unreadable file surfaces
    /// as a human-readable `Err` (DuckDB's bind error), mirroring `MemBook::open`.
    pub fn open(path: &str) -> Result<DataBook, String> {
        let kind =
            detect_kind(path).ok_or_else(|| format!("unrecognised data-file extension: {path}"))?;
        let conn = open_conn()?;
        let sheets = register(&conn, kind, path)?;
        if sheets.is_empty() {
            return Err("database has no tables".into());
        }
        let mut book = DataBook {
            conn,
            sheets,
            cur: 0,
            override_sql: None,
            schema: Vec::new(),
            total: 0,
            cache: None,
        };
        book.load_active()?;
        Ok(book)
    }

    /// The relation every read (schema, count, window, find) wraps in a subquery:
    /// the user's `:` override when one is set, else the active sheet's base
    /// relation. Routing all reads through here is what makes an override replace
    /// the entire view — schema, dims, cells, and search — with the query result.
    fn active_relation(&self) -> String {
        match &self.override_sql {
            Some(sql) => sql.clone(),
            None => self.sheets[self.cur].relation.clone(),
        }
    }

    /// (Re)load the active sheet's schema and row count, and drop any stale
    /// window. `DESCRIBE` binds the relation without executing it, so this also
    /// doubles as the point where a malformed file first errors.
    fn load_active(&mut self) -> Result<(), String> {
        let relation = self.active_relation();
        self.schema = describe(&self.conn, &relation)?;
        self.total = count(&self.conn, &relation)?;
        self.cache = None;
        Ok(())
    }

    /// Sheet (table) names, in registration order.
    pub fn names(&self) -> Vec<String> {
        self.sheets.iter().map(|s| s.name.clone()).collect()
    }

    /// Index of the active sheet.
    pub fn selected(&self) -> usize {
        self.cur
    }

    /// Switch sheets: reload schema + count and clear the cache. Out-of-range
    /// indices are ignored (the active sheet is unchanged), matching MemBook.
    pub fn select(&mut self, idx: usize) {
        if idx < self.sheets.len() && idx != self.cur {
            self.cur = idx;
            // Switching tabs returns to the base table. The tabs ARE the source
            // relations, and the `:` prompt is only a transient query lens over
            // the current one — moving to a different source drops that lens.
            self.override_sql = None;
            // A reload failure leaves the book pointed at the new sheet with an
            // empty schema/zero count — the grid renders an empty sheet rather
            // than panicking; the previous sheet's data is already dropped.
            let _ = self.load_active();
        }
    }

    /// Point the active sheet's reads at the user's `:` query (a "lens" over the
    /// base relation), or clear it. Trimmed-empty input reverts to the base table.
    ///
    /// The bind/parse error path is the delicate one: a bad query must leave the
    /// book EXACTLY as it was so the grid keeps rendering the prior result. We
    /// stash the previous override, tentatively install the new one, and reload;
    /// on failure we restore the old override and reload it back, then surface
    /// DuckDB's error. On success the new schema/count are live and the stale
    /// window cache is dropped by `load_active`.
    pub fn set_sql(&mut self, sql: &str) -> Result<(), String> {
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            self.override_sql = None;
            self.load_active()?;
            return Ok(());
        }
        let prev = self.override_sql.take();
        self.override_sql = Some(trimmed.to_string());
        match self.load_active() {
            Ok(()) => Ok(()),
            Err(e) => {
                // Restore the previous lens and reload it so the schema/count/
                // cache describe the prior result again — the book is untouched.
                self.override_sql = prev;
                let _ = self.load_active();
                Err(e)
            }
        }
    }

    /// The running `:` query for the active sheet, if any — for the status line's
    /// "a query is live" indicator. `None` means the raw base table is shown.
    pub fn active_sql(&self) -> Option<&str> {
        self.override_sql.as_deref()
    }

    /// (total_rows, ncols, done, capped). Unlike the other backends the total is
    /// the REAL, uncapped `COUNT(*)` (there is no row cap here); `done` is always
    /// true (reads are synchronous) and `capped` always false.
    pub fn dims(&self) -> (usize, usize, bool, bool) {
        (self.total, self.schema.len(), true, false)
    }

    /// Real column names for the active sheet (from the schema) — the grid shows
    /// these as headers instead of synthesised A/B/C letters.
    pub fn headers(&self) -> Vec<String> {
        self.schema.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Rows `r0..min(r1, total)`, columns `c0..c1`, as strings. Served from the
    /// prefetch cache; a viewport miss fetches `[r0-MARGIN, r1+MARGIN]` (clamped)
    /// in one query and caches it, so line-by-line scrolling is a cache hit.
    pub fn window(&mut self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        let r1 = r1.min(self.total);
        if r0 >= r1 {
            return Vec::new();
        }
        if !self.cache_covers(r0, r1) {
            let start = r0.saturating_sub(MARGIN);
            let len = ((r1 - r0) + 2 * MARGIN).min(self.total.saturating_sub(start));
            match self.fetch(start, len) {
                Ok(rows) => self.cache = Some(WindowCache { start, rows }),
                Err(_) => {
                    // A transient query failure yields a blank window rather than
                    // aborting the UI; the next frame retries.
                    self.cache = None;
                    return Vec::new();
                }
            }
        }
        let cache = self.cache.as_ref().expect("cache populated above");
        (r0..r1)
            .map(|r| {
                let row = cache.rows.get(r - cache.start);
                (c0..c1)
                    .map(|c| {
                        row.and_then(|cells| cells.get(c))
                            .cloned()
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect()
    }

    /// True when the cache holds every row in `[r0, r1)`.
    fn cache_covers(&self, r0: usize, r1: usize) -> bool {
        matches!(&self.cache, Some(c) if c.start <= r0 && r1 <= c.start + c.rows.len())
    }

    /// Fetch `len` rows starting at `start` as `CAST(... AS VARCHAR)` strings —
    /// every column, so the cache can serve any horizontal slice. NULL → "".
    fn fetch(&self, start: usize, len: usize) -> Result<Vec<Vec<String>>, String> {
        let ncols = self.schema.len();
        if ncols == 0 {
            return Ok(Vec::new());
        }
        let proj = cast_projection(&self.schema);
        let rel = self.active_relation();
        let sql = format!("SELECT {proj} FROM ({rel}) LIMIT {len} OFFSET {start}");
        let mut stmt = self.conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut line = Vec::with_capacity(ncols);
            for c in 0..ncols {
                let v: Option<String> = row.get(c).map_err(|e| e.to_string())?;
                line.push(v.unwrap_or_default());
            }
            out.push(line);
        }
        Ok(out)
    }

    /// Case-insensitive cell search over the active relation, returning
    /// `(row_index, col_index)` pairs (`row_index` 0-based within the relation).
    ///
    /// DuckDB does the coarse filter: a subquery tags every row with its natural
    /// position (`row_number() OVER () - 1`, computed BEFORE the filter so the
    /// index is the relation's, not the filtered set's), then an `ILIKE` across
    /// every column keeps only candidate rows — bounded by `FIND_CAP`. Rust then
    /// confirms the precise column hits with [`crate::xlsx::contains_ci`], so the
    /// result matches the grid's own notion of a match exactly.
    pub fn find(&self, query: &str) -> Vec<(usize, usize)> {
        let ncols = self.schema.len();
        if ncols == 0 || query.is_empty() {
            return Vec::new();
        }
        let needle = query.to_ascii_lowercase();
        let sql = match self.find_sql(query) {
            Some(s) => s,
            None => return Vec::new(),
        };
        let mut hits = Vec::new();
        let Ok(mut stmt) = self.conn.prepare(&sql) else {
            return hits;
        };
        let Ok(mut rows) = stmt.query([]) else {
            return hits;
        };
        while let Ok(Some(row)) = rows.next() {
            let Ok(rn) = row.get::<usize, i64>(0) else {
                continue;
            };
            let r = rn.max(0) as usize;
            for c in 0..ncols {
                let cell: Option<String> = row.get(c + 1).unwrap_or(None);
                if let Some(text) = cell {
                    if crate::xlsx::contains_ci(&text, &needle) {
                        hits.push((r, c));
                        if hits.len() >= FIND_CAP {
                            return hits;
                        }
                    }
                }
            }
        }
        hits
    }

    /// Build the find query: `__sucher_rn` + every column CAST to VARCHAR, over a
    /// subquery that numbers rows before an `ILIKE`-across-all-columns filter.
    /// Returns `None` only for the degenerate empty-schema case.
    fn find_sql(&self, query: &str) -> Option<String> {
        if self.schema.is_empty() {
            return None;
        }
        let rel = self.active_relation();
        let pattern = quote_literal(&format!("%{}%", escape_like(query)));
        let mut proj = String::from("__sucher_rn");
        let mut filter = String::new();
        for (i, (name, _)) in self.schema.iter().enumerate() {
            let cast = format!("CAST({} AS VARCHAR)", quote_ident(name));
            proj.push_str(", ");
            proj.push_str(&cast);
            if i > 0 {
                filter.push_str(" OR ");
            }
            // ESCAPE '\' — a lone backslash (DuckDB strings are not C-escaped),
            // matching `escape_like`, so literal % / _ in the query stay literal.
            filter.push_str(&format!("({cast} ILIKE {pattern} ESCAPE '\\')"));
        }
        Some(format!(
            "SELECT {proj} FROM \
             (SELECT (row_number() OVER () - 1) AS __sucher_rn, * FROM ({rel})) \
             WHERE {filter} LIMIT {FIND_CAP}"
        ))
    }
}

/// Open an in-memory connection and enforce the offline guarantee immediately —
/// see the module note. Every connection sucher opens goes through here.
fn open_conn() -> Result<Connection, String> {
    let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    conn.execute_batch(
        "SET autoinstall_known_extensions=false; SET autoload_known_extensions=true;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Register the source on `conn` and return its sheets. Parquet/JSONL become one
/// view named by the file stem; SQLite/DuckDB are attached READ_ONLY as `db` and
/// each of their tables becomes a sheet.
fn register(conn: &Connection, kind: SourceKind, path: &str) -> Result<Vec<Sheet>, String> {
    let lit = quote_literal(path);
    match kind {
        SourceKind::Parquet => Ok(vec![single_view(
            conn,
            path,
            &format!("read_parquet({lit})"),
        )?]),
        SourceKind::Json => Ok(vec![single_view(
            conn,
            path,
            &format!("read_json_auto({lit})"),
        )?]),
        SourceKind::Sqlite => {
            conn.execute_batch(&format!("ATTACH {lit} AS db (TYPE SQLITE, READ_ONLY);"))
                .map_err(|e| e.to_string())?;
            attached_sheets(conn)
        }
        SourceKind::DuckDb => {
            conn.execute_batch(&format!("ATTACH {lit} AS db (READ_ONLY);"))
                .map_err(|e| e.to_string())?;
            attached_sheets(conn)
        }
    }
}

/// Create a view named by the file stem over `reader` (e.g. `read_parquet('…')`)
/// so the single relation has a stable name the (future) SQL prompt can `FROM`.
fn single_view(conn: &Connection, path: &str, reader: &str) -> Result<Sheet, String> {
    let stem = file_stem(path);
    let ident = quote_ident(&stem);
    conn.execute_batch(&format!("CREATE VIEW {ident} AS SELECT * FROM {reader};"))
        .map_err(|e| e.to_string())?;
    Ok(Sheet {
        name: stem,
        relation: format!("SELECT * FROM {ident}"),
    })
}

/// List the tables/views of the attached `db` database as sheets, in name order.
fn attached_sheets(conn: &Connection) -> Result<Vec<Sheet>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_catalog='db' ORDER BY 1",
        )
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut sheets = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let name: String = row.get(0).map_err(|e| e.to_string())?;
        let relation = format!("SELECT * FROM db.{}", quote_ident(&name));
        sheets.push(Sheet { name, relation });
    }
    Ok(sheets)
}

/// `DESCRIBE <relation>` → `(column_name, column_type)` pairs. Binds but does not
/// execute the relation, so it is instant even on huge files.
fn describe(conn: &Connection, relation: &str) -> Result<Vec<(String, String)>, String> {
    let mut stmt = conn
        .prepare(&format!("DESCRIBE {relation}"))
        .map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut schema = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let name: String = row.get(0).map_err(|e| e.to_string())?;
        let ty: String = row.get(1).map_err(|e| e.to_string())?;
        schema.push((name, ty));
    }
    Ok(schema)
}

/// One-shot real row count of a relation.
fn count(conn: &Connection, relation: &str) -> Result<usize, String> {
    let n: i64 = conn
        .query_row(&format!("SELECT count(*) FROM ({relation})"), [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    Ok(n.max(0) as usize)
}

/// Detect the data-file family from the (case-insensitive) extension. PURE.
fn detect_kind(path: &str) -> Option<SourceKind> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    match ext.as_str() {
        "parquet" | "pq" => Some(SourceKind::Parquet),
        "jsonl" | "ndjson" => Some(SourceKind::Json),
        "sqlite" | "sqlite3" | "db" | "db3" => Some(SourceKind::Sqlite),
        "duckdb" | "ddb" => Some(SourceKind::DuckDb),
        _ => None,
    }
}

/// The file stem used to name a single-relation view. Empty/absent stems fall
/// back to `data` so the identifier is always valid. PURE.
fn file_stem(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "data".to_string())
}

/// Quote a SQL identifier: wrap in double quotes, doubling any embedded `"`. PURE.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Quote a SQL string literal: wrap in single quotes, doubling any embedded `'`.
/// PURE.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The projection list `CAST("c0" AS VARCHAR), CAST("c1" AS VARCHAR), …` for a
/// schema — the NULL-safe, panic-free way to read any column as display text. PURE.
fn cast_projection(schema: &[(String, String)]) -> String {
    schema
        .iter()
        .map(|(name, _)| format!("CAST({} AS VARCHAR)", quote_ident(name)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Escape a user query for use inside a `… ILIKE '%<pat>%' ESCAPE '\'` pattern:
/// the LIKE metacharacters `%` and `_` (and the escape `\` itself) are prefixed
/// with a backslash so they match literally rather than as wildcards. PURE.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure helpers (no DB) ----

    #[test]
    fn quote_ident_doubles_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        // A name that is a SQL keyword or contains a dot is safely quoted whole.
        assert_eq!(quote_ident("select.from"), "\"select.from\"");
    }

    #[test]
    fn quote_literal_doubles_apostrophes() {
        assert_eq!(quote_literal("/tmp/a.parquet"), "'/tmp/a.parquet'");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn cast_projection_builds_varchar_list() {
        let schema = vec![
            ("id".to_string(), "INTEGER".to_string()),
            ("na\"me".to_string(), "VARCHAR".to_string()),
        ];
        assert_eq!(
            cast_projection(&schema),
            "CAST(\"id\" AS VARCHAR), CAST(\"na\"\"me\" AS VARCHAR)"
        );
        assert_eq!(cast_projection(&[]), "");
    }

    #[test]
    fn detect_kind_from_extension() {
        assert_eq!(detect_kind("a.parquet"), Some(SourceKind::Parquet));
        assert_eq!(detect_kind("a.PQ"), Some(SourceKind::Parquet));
        assert_eq!(detect_kind("events.jsonl"), Some(SourceKind::Json));
        assert_eq!(detect_kind("events.ndjson"), Some(SourceKind::Json));
        assert_eq!(detect_kind("app.db"), Some(SourceKind::Sqlite));
        assert_eq!(detect_kind("app.sqlite3"), Some(SourceKind::Sqlite));
        assert_eq!(detect_kind("store.duckdb"), Some(SourceKind::DuckDb));
        assert_eq!(detect_kind("store.ddb"), Some(SourceKind::DuckDb));
        assert_eq!(detect_kind("notes.txt"), None);
        assert_eq!(detect_kind("noext"), None);
    }

    #[test]
    fn file_stem_falls_back() {
        assert_eq!(file_stem("/data/sales.parquet"), "sales");
        assert_eq!(file_stem("events.jsonl"), "events");
        // A leading-dot name is itself the stem (no extension split), and quotes
        // fine as an identifier.
        assert_eq!(file_stem("/x/.parquet"), ".parquet");
        // A path with no file name at all falls back to a valid default.
        assert_eq!(file_stem(""), "data");
        assert_eq!(file_stem("/"), "data");
    }

    #[test]
    fn escape_like_escapes_wildcards() {
        assert_eq!(escape_like("plain"), "plain");
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    }

    // ---- integration (feature-gated release build, real DuckDB) ----
    //
    // Fixtures are created IN the test with DuckDB itself, so the tests are
    // hermetic and prove the round trip end to end.

    /// A unique scratch path with the given extension, in the OS temp dir.
    fn scratch_path(tag: &str, ext: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "sucher-data-{tag}-{}-{}.{ext}",
            std::process::id(),
            // A counter to disambiguate fixtures within one test run.
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Write a small Parquet fixture with a NULL and a DATE via a scratch conn.
    fn make_parquet() -> std::path::PathBuf {
        let path = scratch_path("pq", "parquet");
        let conn = open_conn().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT * FROM (VALUES \
               (1, 'alpha', DATE '2020-01-02', 'x'), \
               (2, 'beta',  DATE '2021-06-15', NULL), \
               (3, 'gamma', DATE '2022-12-31', 'z')) \
             t(id, name, d, note)) TO {} (FORMAT PARQUET);",
            quote_literal(path.to_str().unwrap())
        ))
        .unwrap();
        path
    }

    #[test]
    fn parquet_dims_headers_window_find() {
        let path = make_parquet();
        let p = path.to_str().unwrap();
        let mut book = DataBook::open(p).expect("open parquet");

        // One sheet, named by the file stem.
        assert_eq!(book.names().len(), 1);

        // Real, uncapped count; synchronous; not capped.
        let (total, ncols, done, capped) = book.dims();
        assert_eq!(total, 3);
        assert_eq!(ncols, 4);
        assert!(done);
        assert!(!capped);

        // Real column names, not A/B/C.
        assert_eq!(book.headers(), vec!["id", "name", "d", "note"]);

        // Values: ISO date rendered by DuckDB, NULL → "".
        let win = book.window(0, 3, 0, 4);
        assert_eq!(win[0], vec!["1", "alpha", "2020-01-02", "x"]);
        assert_eq!(win[1], vec!["2", "beta", "2021-06-15", ""]); // NULL → ""
        assert_eq!(win[2][2], "2022-12-31");

        // Case-insensitive find locates a known cell (col 1 = name).
        let hits = book.find("BETA");
        assert_eq!(hits, vec![(1, 1)]);
        // A miss is empty.
        assert!(book.find("zzznotfoundzzz").is_empty());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn set_sql_overrides_view_and_reverts() {
        let path = make_parquet();
        let p = path.to_str().unwrap();
        let mut book = DataBook::open(p).expect("open parquet");
        // The single view is named by the file stem; query it by that name.
        let view = quote_ident(&file_stem(p));

        // Baseline: the raw view is 3 rows × 4 cols, no live query.
        assert_eq!(book.dims(), (3, 4, true, false));
        assert_eq!(book.active_sql(), None);

        // A valid query replaces the ENTIRE view: schema, dims, cells all reflect
        // the result — 1 row / 1 col named `n`, holding the count.
        let q = format!("SELECT count(*) AS n FROM {view}");
        book.set_sql(&q).expect("ok");
        assert_eq!(book.dims(), (1, 1, true, false));
        assert_eq!(book.headers(), vec!["n"]);
        assert_eq!(book.window(0, 1, 0, 1), vec![vec!["3"]]);
        assert_eq!(book.active_sql(), Some(q.as_str()));

        // Search runs over the CURRENT (override) view, not the base table.
        assert_eq!(book.find("3"), vec![(0, 0)]);

        // Empty input clears the override and returns to the base table.
        book.set_sql("").expect("revert ok");
        assert_eq!(book.dims(), (3, 4, true, false));
        assert_eq!(book.headers(), vec!["id", "name", "d", "note"]);
        assert_eq!(book.active_sql(), None);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn set_sql_error_restores_previous_view() {
        let path = make_parquet();
        let p = path.to_str().unwrap();
        let mut book = DataBook::open(p).expect("open parquet");
        let view = quote_ident(&file_stem(p));

        // Install a good override first, so "previous" is a non-trivial state.
        let good = format!("SELECT id, name FROM {view} WHERE id >= 2");
        book.set_sql(&good).expect("ok");
        assert_eq!(book.dims(), (2, 2, true, false));
        assert_eq!(book.headers(), vec!["id", "name"]);
        let before = book.window(0, 2, 0, 2);

        // A bad query (bind error) must FAIL and leave the book exactly as it was.
        let err = book
            .set_sql(&format!("SELECT nonexistent_col FROM {view}"))
            .expect_err("bad query errors");
        assert!(!err.is_empty(), "a human-readable DuckDB error is returned");

        // Restore-on-error guarantee: schema/dims/cells are the previous result.
        assert_eq!(book.dims(), (2, 2, true, false));
        assert_eq!(book.headers(), vec!["id", "name"]);
        assert_eq!(book.window(0, 2, 0, 2), before);
        assert_eq!(book.active_sql(), Some(good.as_str()));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn sqlite_tables_become_sheets() {
        let path = scratch_path("sq", "db");
        let p = path.to_str().unwrap();
        // Create a real SQLite file by attaching it read-WRITE via DuckDB's
        // SQLite scanner (which is statically bundled), then detach.
        {
            let conn = open_conn().unwrap();
            conn.execute_batch(&format!(
                "ATTACH {} AS s (TYPE SQLITE); \
                 CREATE TABLE s.items (id INTEGER, label VARCHAR); \
                 INSERT INTO s.items VALUES (1, 'apple'), (2, 'pear'); \
                 CREATE TABLE s.tags (name VARCHAR); \
                 INSERT INTO s.tags VALUES ('fresh'); \
                 DETACH s;",
                quote_literal(p)
            ))
            .unwrap();
        }

        let mut book = DataBook::open(p).expect("open sqlite");
        // One sheet per table, name-ordered.
        assert_eq!(book.names(), vec!["items", "tags"]);

        book.select(0);
        assert_eq!(book.dims(), (2, 2, true, false));
        assert_eq!(book.headers(), vec!["id", "label"]);
        assert_eq!(book.window(0, 2, 0, 2)[1], vec!["2", "pear"]);
        assert_eq!(book.find("apple"), vec![(0, 1)]);

        // An override on this sheet…
        book.set_sql("SELECT count(*) AS n FROM db.items")
            .expect("ok");
        assert_eq!(book.dims(), (1, 1, true, false));
        assert_eq!(
            book.active_sql(),
            Some("SELECT count(*) AS n FROM db.items")
        );

        // …is dropped when switching to another sheet: the tab is the source.
        book.select(1);
        assert_eq!(book.active_sql(), None);
        assert_eq!(book.dims(), (1, 1, true, false));
        assert_eq!(book.headers(), vec!["name"]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn window_prefetch_cache_serves_scroll() {
        // A relation larger than the prefetch margin, to exercise the cache path.
        let path = scratch_path("big", "parquet");
        let p = path.to_str().unwrap();
        let conn = open_conn().unwrap();
        conn.execute_batch(&format!(
            "COPY (SELECT i AS id, ('row' || i) AS name FROM range(5000) t(i)) \
             TO {} (FORMAT PARQUET);",
            quote_literal(p)
        ))
        .unwrap();
        drop(conn);

        let mut book = DataBook::open(p).expect("open");
        assert_eq!(book.dims().0, 5000);
        // First viewport.
        let a = book.window(100, 110, 0, 2);
        assert_eq!(a[0], vec!["100", "row100"]);
        // Scroll one row — still inside the cached margin (a cache hit), correct.
        let b = book.window(101, 111, 0, 2);
        assert_eq!(b[0], vec!["101", "row101"]);
        // Jump far — a fresh fetch, still correct.
        let c = book.window(4990, 5000, 0, 2);
        assert_eq!(
            c.last().unwrap(),
            &vec!["4999".to_string(), "row4999".to_string()]
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn offline_reads_parquet_without_network() {
        // The core offline guarantee: a FRESH connection carrying only the two
        // pragmas (autoinstall=false) reads a Parquet purely from the bundled
        // build. If parquet were a network-installed extension this would fail.
        let path = make_parquet();
        let p = path.to_str().unwrap();
        let conn = open_conn().unwrap();
        let n: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM read_parquet({})", quote_literal(p)),
                [],
                |r| r.get(0),
            )
            .expect("read_parquet must work offline from the bundled build");
        assert_eq!(n, 3);
        std::fs::remove_file(path).ok();
    }
}
