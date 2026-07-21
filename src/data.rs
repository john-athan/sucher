// Data-file backend for the grid viewer (ADR 0016): Parquet, JSONL/NDJSON,
// SQLite, DuckDB. This is the grid's third `Book` shape, alongside the eager
// calamine `MemBook` and the capped-streaming `StreamBook`.
//
// It is NOT one engine, but two native engines behind ONE `Book::Data` seam —
// *native-engine-per-source*, not a hybrid of interchangeable libraries (ADR
// 0016). Each file is read by the engine that actually owns its format:
//
//   * `DuckBook` (embedded DuckDB) — Parquet, JSONL/NDJSON, native DuckDB files.
//     DuckDB is the reference SQL-on-files engine; its `read_parquet` /
//     `read_json_auto` readers and native `ATTACH` cover these three.
//   * `SqliteBook` (rusqlite) — SQLite databases (`.sqlite/.sqlite3/.db/.db3`),
//     read through rusqlite's statically-bundled libsqlite.
//
// Why the split — offline is enforced, not assumed. sucher is a local viewer
// that must never phone home. DuckDB's format readers are loadable extensions it
// will, by default, AUTO-INSTALL over the network on first use, so every DuckDB
// connection runs two pragmas immediately after opening —
//     SET autoinstall_known_extensions = false;  -- never touch the network
//     SET autoload_known_extensions    = false;  -- never even load from the
//                                                 -- on-disk extension cache
// With BOTH false, only the statically-compiled readers can run: `parquet` and
// `json` are compiled INTO libduckdb via the crate's features, and native
// `ATTACH` is core, so all three work with no extension directory and no network
// — while a reader that is not built in fails loudly instead of downloading. CI
// on a clean machine proved DuckDB's SQLite scanner is NOT a static feature (it
// is loadable-only and would fail under these pragmas), so SQLite is read with
// rusqlite's own bundled libsqlite instead — fully offline, no compromise.
//
// The two engines expose the same internal shape, so the grid is unaware which
// one backs a given file:
//
//   * Sheets = tables. Each table of a SQLite/DuckDB database is a grid "sheet";
//     single-relation files (Parquet, JSONL) are one sheet named by the file
//     stem, exposed as a view/relation the `:` prompt can `FROM`.
//   * Schema without executing. DuckDB takes the schema from `DESCRIBE`
//     (`(name, type)`, binds but does not run — instant even on a billion-row
//     Parquet); SQLite reads it from a prepared statement's `column_names()`
//     (SQLite knows the columns at prepare time). Neither materialises the data.
//   * Values as display text. DuckDB reads each cell as `CAST(col AS VARCHAR)` →
//     `Option<String>` (NULL → ""), which also renders ISO dates for free;
//     SQLite reads the raw `ValueRef` and formats it (NULL → "", integers/reals
//     as numbers, text UTF-8-lossy, blobs as `[N bytes]`).
//   * Lazy, uncapped windowing. Both are fast lazy query engines, neither eager
//     (MemBook) nor worth capping (StreamBook). The book windows on demand —
//     `… LIMIT <len> OFFSET <start>` around the visible range, with a prefetch
//     margin cached so ordinary scrolling is a cache hit — and reports the real
//     `COUNT(*)` as its total, so the grid scrolls an arbitrarily large file
//     with no row cap.
//   * A `:` query override. Submitting a query replaces the active sheet's
//     relation with the result (DuckDB SQL on a DuckDB source, SQLite SQL on a
//     `.db`); a parse/bind error leaves the book exactly as it was.
//
// Read-only throughout: DuckDB databases are attached `READ_ONLY`, SQLite files
// opened `SQLITE_OPEN_READ_ONLY`.

use duckdb::Connection;

/// Rows fetched around the visible range on either side of a viewport miss, so
/// ordinary line-by-line scrolling stays a cache hit rather than re-querying.
const MARGIN: usize = 200;

/// Upper bound on search hits, so `find` on a huge relation stays responsive.
/// Search is a convenience; a truncated hit list is acceptable (ADR 0016).
const FIND_CAP: usize = 10_000;

/// The four data-file families, detected from the file extension. Sqlite routes
/// to [`SqliteBook`]; the other three to [`DuckBook`].
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

/// The two read engines behind the one `Book::Data` seam (ADR 0016). DuckDB owns
/// Parquet/JSONL/native-DuckDB; rusqlite owns SQLite. The grid never sees this
/// distinction — [`DataBook`] delegates every call to the active engine.
enum Engine {
    Duck(DuckBook),
    Sqlite(SqliteBook),
}

/// The data-file book the grid holds. A thin wrapper that routes on the file's
/// kind to the engine that owns the format, then forwards every method to it, so
/// the grid's `Book::Data` arm is engine-agnostic (ADR 0016).
pub struct DataBook {
    engine: Engine,
}

impl DataBook {
    /// Open a data file: detect its family, and hand it to the owning engine —
    /// SQLite to [`SqliteBook`], everything else to [`DuckBook`]. A bad/unreadable
    /// file surfaces as a human-readable `Err`, mirroring `MemBook::open`.
    pub fn open(path: &str) -> Result<DataBook, String> {
        let kind =
            detect_kind(path).ok_or_else(|| format!("unrecognised data-file extension: {path}"))?;
        let engine = match kind {
            SourceKind::Sqlite => Engine::Sqlite(SqliteBook::open(path)?),
            _ => Engine::Duck(DuckBook::open(path, kind)?),
        };
        Ok(DataBook { engine })
    }

    /// Sheet (table) names, in registration order.
    pub fn names(&self) -> Vec<String> {
        match &self.engine {
            Engine::Duck(b) => b.names(),
            Engine::Sqlite(b) => b.names(),
        }
    }

    /// Index of the active sheet.
    pub fn selected(&self) -> usize {
        match &self.engine {
            Engine::Duck(b) => b.selected(),
            Engine::Sqlite(b) => b.selected(),
        }
    }

    /// Switch sheets: reload schema + count and clear any `:` override and cache.
    pub fn select(&mut self, idx: usize) {
        match &mut self.engine {
            Engine::Duck(b) => b.select(idx),
            Engine::Sqlite(b) => b.select(idx),
        }
    }

    /// Point the active sheet's reads at the user's `:` query, or clear it on
    /// empty input. A bad query leaves the book exactly as it was (see each
    /// engine's `set_sql`).
    pub fn set_sql(&mut self, sql: &str) -> Result<(), String> {
        match &mut self.engine {
            Engine::Duck(b) => b.set_sql(sql),
            Engine::Sqlite(b) => b.set_sql(sql),
        }
    }

    /// The running `:` query for the active sheet, if any.
    pub fn active_sql(&self) -> Option<&str> {
        match &self.engine {
            Engine::Duck(b) => b.active_sql(),
            Engine::Sqlite(b) => b.active_sql(),
        }
    }

    /// (total_rows, ncols, done, capped). The total is the REAL, uncapped
    /// `COUNT(*)`; `done` is always true (synchronous) and `capped` always false.
    pub fn dims(&self) -> (usize, usize, bool, bool) {
        match &self.engine {
            Engine::Duck(b) => b.dims(),
            Engine::Sqlite(b) => b.dims(),
        }
    }

    /// Real column names for the active sheet — the grid shows these as headers
    /// instead of synthesised A/B/C letters.
    pub fn headers(&self) -> Vec<String> {
        match &self.engine {
            Engine::Duck(b) => b.headers(),
            Engine::Sqlite(b) => b.headers(),
        }
    }

    /// Rows `r0..min(r1, total)`, columns `c0..c1`, as strings, served from the
    /// prefetch cache.
    pub fn window(&mut self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        match &mut self.engine {
            Engine::Duck(b) => b.window(r0, r1, c0, c1),
            Engine::Sqlite(b) => b.window(r0, r1, c0, c1),
        }
    }

    /// Case-insensitive cell search over the active relation, returning
    /// `(row_index, col_index)` pairs (`row_index` 0-based within the relation).
    pub fn find(&self, query: &str) -> Vec<(usize, usize)> {
        match &self.engine {
            Engine::Duck(b) => b.find(query),
            Engine::Sqlite(b) => b.find(query),
        }
    }
}

// ---- DuckDB engine: Parquet / JSONL / native DuckDB ----

/// The DuckDB-backed engine. Owns the connection for its whole lifetime (views
/// and attachments live in it), the list of sheets, and — for the active sheet
/// only — the cached schema, total row count, and the last window.
struct DuckBook {
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

impl DuckBook {
    /// Open a Parquet/JSONL/DuckDB file: connect, enforce offline mode, register
    /// the source (view or ATTACH), build the sheet list, and load the active
    /// sheet's schema + count.
    fn open(path: &str, kind: SourceKind) -> Result<DuckBook, String> {
        let conn = open_conn()?;
        let sheets = register(&conn, kind, path)?;
        if sheets.is_empty() {
            return Err("database has no tables".into());
        }
        let mut book = DuckBook {
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

    fn names(&self) -> Vec<String> {
        self.sheets.iter().map(|s| s.name.clone()).collect()
    }

    fn selected(&self) -> usize {
        self.cur
    }

    /// Switch sheets: reload schema + count and clear the cache. Out-of-range
    /// indices are ignored (the active sheet is unchanged), matching MemBook.
    fn select(&mut self, idx: usize) {
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
    fn set_sql(&mut self, sql: &str) -> Result<(), String> {
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

    fn active_sql(&self) -> Option<&str> {
        self.override_sql.as_deref()
    }

    fn dims(&self) -> (usize, usize, bool, bool) {
        (self.total, self.schema.len(), true, false)
    }

    fn headers(&self) -> Vec<String> {
        self.schema.iter().map(|(n, _)| n.clone()).collect()
    }

    /// Rows `r0..min(r1, total)`, columns `c0..c1`, as strings. Served from the
    /// prefetch cache; a viewport miss fetches `[r0-MARGIN, r1+MARGIN]` (clamped)
    /// in one query. Reads through `CAST(... AS VARCHAR)`; NULL → "".
    fn window(&mut self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        let relation = self.active_relation();
        let conn = &self.conn;
        let schema = &self.schema;
        window_cached(&mut self.cache, self.total, r0, r1, c0, c1, |start, len| {
            fetch_duck(conn, schema, &relation, start, len)
        })
    }

    /// Case-insensitive cell search over the active relation. DuckDB does the
    /// coarse filter: a subquery tags every row with its natural position
    /// (`row_number() OVER () - 1`, computed BEFORE the filter so the index is the
    /// relation's), then an `ILIKE` across every column keeps candidate rows —
    /// bounded by `FIND_CAP`. Rust then confirms the precise column hits with
    /// [`crate::xlsx::contains_ci`], matching the grid's own notion of a match.
    fn find(&self, query: &str) -> Vec<(usize, usize)> {
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

/// Fetch `len` rows starting at `start` as `CAST(... AS VARCHAR)` strings — every
/// column, so the cache can serve any horizontal slice. NULL → "".
fn fetch_duck(
    conn: &Connection,
    schema: &[(String, String)],
    relation: &str,
    start: usize,
    len: usize,
) -> Result<Vec<Vec<String>>, String> {
    let ncols = schema.len();
    if ncols == 0 {
        return Ok(Vec::new());
    }
    let proj = cast_projection(schema);
    let sql = format!("SELECT {proj} FROM ({relation}) LIMIT {len} OFFSET {start}");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
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

/// Open an in-memory DuckDB connection and enforce the offline guarantee
/// immediately — see the module note. Every DuckDB connection sucher opens goes
/// through here. BOTH auto-flags are false: `autoinstall` blocks the network,
/// `autoload` additionally refuses the on-disk extension cache, so only the
/// statically-compiled readers (`parquet`, `json`, core) can run.
fn open_conn() -> Result<Connection, String> {
    let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
    conn.execute_batch(
        "SET autoinstall_known_extensions=false; SET autoload_known_extensions=false;",
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

/// Register the source on `conn` and return its sheets. Parquet/JSONL become one
/// view named by the file stem; a native DuckDB database is attached READ_ONLY as
/// `db` and each of its tables becomes a sheet. (SQLite never reaches here — it is
/// read by [`SqliteBook`], since its DuckDB scanner is a network-only extension.)
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
        SourceKind::DuckDb => {
            conn.execute_batch(&format!("ATTACH {lit} AS db (READ_ONLY);"))
                .map_err(|e| e.to_string())?;
            attached_sheets(conn)
        }
        SourceKind::Sqlite => {
            unreachable!("SQLite is read by SqliteBook, never routed to DuckBook")
        }
    }
}

/// Create a view named by the file stem over `reader` (e.g. `read_parquet('…')`)
/// so the single relation has a stable name the `:` SQL prompt can `FROM`.
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

// ---- SQLite engine: .sqlite / .sqlite3 / .db / .db3 via rusqlite ----

/// The rusqlite-backed engine. Mirrors [`DuckBook`]'s behaviour with SQLite's
/// native library: tables are sheets, columns come from a prepared statement,
/// cells from raw `ValueRef`s, windowing/count/find/override all the same shape.
/// The connection is opened READ-ONLY. rusqlite's statically-bundled libsqlite
/// makes this fully offline — the reason SQLite is not read through DuckDB.
struct SqliteBook {
    conn: rusqlite::Connection,
    sheets: Vec<Sheet>,
    cur: usize,
    /// The `:` prompt's query lens over the ACTIVE sheet, or `None` for the base
    /// table — a transient override cleared whenever the active sheet changes.
    override_sql: Option<String>,
    /// Active relation's column names (SQLite is dynamically typed, so only names
    /// are needed — there is no per-column type string to carry).
    columns: Vec<String>,
    /// Active relation real row count (`COUNT(*)`), cached — powers lazy scroll.
    total: usize,
    cache: Option<WindowCache>,
}

impl SqliteBook {
    /// Open a SQLite file READ-ONLY and build the sheet list (its tables). A
    /// non-SQLite `.db` errors here (or at the first table query) — a
    /// human-readable `Err`, mirroring DuckBook's bad-file behaviour.
    fn open(path: &str) -> Result<SqliteBook, String> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| e.to_string())?;
        let sheets = sqlite_sheets(&conn)?;
        if sheets.is_empty() {
            return Err("database has no tables".into());
        }
        let mut book = SqliteBook {
            conn,
            sheets,
            cur: 0,
            override_sql: None,
            columns: Vec::new(),
            total: 0,
            cache: None,
        };
        book.load_active()?;
        Ok(book)
    }

    /// The relation every read wraps in a subquery: the `:` override if set, else
    /// the active sheet's base relation (`SELECT * FROM "<table>"`).
    fn active_relation(&self) -> String {
        match &self.override_sql {
            Some(sql) => sql.clone(),
            None => self.sheets[self.cur].relation.clone(),
        }
    }

    /// (Re)load the active relation's columns and row count, dropping any stale
    /// window. Preparing `… LIMIT 0` binds the relation without executing it, so
    /// this doubles as the point a malformed override/file first errors.
    fn load_active(&mut self) -> Result<(), String> {
        let relation = self.active_relation();
        self.columns = sqlite_columns(&self.conn, &relation)?;
        self.total = sqlite_count(&self.conn, &relation)?;
        self.cache = None;
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        self.sheets.iter().map(|s| s.name.clone()).collect()
    }

    fn selected(&self) -> usize {
        self.cur
    }

    /// Switch sheets: drop the `:` override (the tab is the source), reload the
    /// columns + count, clear the cache. Out-of-range indices are ignored.
    fn select(&mut self, idx: usize) {
        if idx < self.sheets.len() && idx != self.cur {
            self.cur = idx;
            self.override_sql = None;
            let _ = self.load_active();
        }
    }

    /// Point the active sheet at the `:` query, or clear it on empty input. On a
    /// prepare (syntax/bind) error, restore and reload the previous override so
    /// the book is left exactly as it was, then surface rusqlite's error.
    fn set_sql(&mut self, sql: &str) -> Result<(), String> {
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
                self.override_sql = prev;
                let _ = self.load_active();
                Err(e)
            }
        }
    }

    fn active_sql(&self) -> Option<&str> {
        self.override_sql.as_deref()
    }

    fn dims(&self) -> (usize, usize, bool, bool) {
        (self.total, self.columns.len(), true, false)
    }

    fn headers(&self) -> Vec<String> {
        self.columns.clone()
    }

    /// Rows `r0..min(r1, total)`, columns `c0..c1`, as strings. Same prefetch
    /// cache as DuckBook; a viewport miss fetches `[r0-MARGIN, r1+MARGIN]`.
    fn window(&mut self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        let relation = self.active_relation();
        let conn = &self.conn;
        let ncols = self.columns.len();
        window_cached(&mut self.cache, self.total, r0, r1, c0, c1, |start, len| {
            fetch_sqlite(conn, ncols, &relation, start, len)
        })
    }

    /// Case-insensitive cell search, mirroring DuckBook: a subquery numbers rows
    /// (`row_number() OVER () - 1`) before a `LIKE`-across-all-columns prefilter
    /// (bundled SQLite has window functions), capped at `FIND_CAP`, then Rust
    /// confirms the precise `(row, col)` hits with [`crate::xlsx::contains_ci`].
    fn find(&self, query: &str) -> Vec<(usize, usize)> {
        let ncols = self.columns.len();
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
                // Column c+1 is `CAST("col" AS TEXT)` → Text or Null; read the raw
                // ref and confirm the match in Rust.
                let Ok(cell) = row.get_ref(c + 1) else {
                    continue;
                };
                let text = fmt_value_ref(cell);
                if crate::xlsx::contains_ci(&text, &needle) {
                    hits.push((r, c));
                    if hits.len() >= FIND_CAP {
                        return hits;
                    }
                }
            }
        }
        hits
    }

    /// Build the find query: `__sucher_rn` + every column CAST to TEXT, over a
    /// subquery that numbers rows before a `LIKE`-across-all-columns filter.
    fn find_sql(&self, query: &str) -> Option<String> {
        if self.columns.is_empty() {
            return None;
        }
        let rel = self.active_relation();
        let pattern = quote_literal(&format!("%{}%", escape_like(query)));
        let mut proj = String::from("__sucher_rn");
        let mut filter = String::new();
        for (i, name) in self.columns.iter().enumerate() {
            let cast = format!("CAST({} AS TEXT)", quote_ident(name));
            proj.push_str(", ");
            proj.push_str(&cast);
            if i > 0 {
                filter.push_str(" OR ");
            }
            // ESCAPE '\' matches `escape_like`, so literal % / _ stay literal.
            // SQLite's LIKE is ASCII case-insensitive — the coarse prefilter.
            filter.push_str(&format!("({cast} LIKE {pattern} ESCAPE '\\')"));
        }
        Some(format!(
            "SELECT {proj} FROM \
             (SELECT (row_number() OVER () - 1) AS __sucher_rn, * FROM ({rel})) \
             WHERE {filter} LIMIT {FIND_CAP}"
        ))
    }
}

/// List the SQLite database's tables as sheets, excluding SQLite's internal
/// `sqlite_%` tables, in name order. Each table's base relation is `SELECT *`.
fn sqlite_sheets(conn: &rusqlite::Connection) -> Result<Vec<Sheet>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY 1",
        )
        .map_err(|e| e.to_string())?;
    let names = stmt
        .query_map([], |row| row.get::<usize, String>(0))
        .map_err(|e| e.to_string())?;
    let mut sheets = Vec::new();
    for name in names {
        let name = name.map_err(|e| e.to_string())?;
        let relation = format!("SELECT * FROM {}", quote_ident(&name));
        sheets.push(Sheet { name, relation });
    }
    Ok(sheets)
}

/// The active relation's column names, from a prepared `… LIMIT 0` (SQLite knows
/// the columns at prepare time — no execution). Also validates the relation's SQL.
fn sqlite_columns(conn: &rusqlite::Connection, relation: &str) -> Result<Vec<String>, String> {
    let stmt = conn
        .prepare(&format!("SELECT * FROM ({relation}) LIMIT 0"))
        .map_err(|e| e.to_string())?;
    Ok(stmt
        .column_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect())
}

/// One-shot real row count of a relation.
fn sqlite_count(conn: &rusqlite::Connection, relation: &str) -> Result<usize, String> {
    let n: i64 = conn
        .query_row(&format!("SELECT count(*) FROM ({relation})"), [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    Ok(n.max(0) as usize)
}

/// Fetch `len` rows starting at `start`, reading each cell from its raw
/// `ValueRef` — every column, so the cache can serve any horizontal slice.
fn fetch_sqlite(
    conn: &rusqlite::Connection,
    ncols: usize,
    relation: &str,
    start: usize,
    len: usize,
) -> Result<Vec<Vec<String>>, String> {
    if ncols == 0 {
        return Ok(Vec::new());
    }
    let sql = format!("SELECT * FROM ({relation}) LIMIT {len} OFFSET {start}");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut line = Vec::with_capacity(ncols);
        for i in 0..ncols {
            let vr = row.get_ref(i).map_err(|e| e.to_string())?;
            line.push(fmt_value_ref(vr));
        }
        out.push(line);
    }
    Ok(out)
}

/// Format a SQLite `ValueRef` as a display cell: NULL → "", integers/reals as
/// numbers (reals via [`fmt_real`], matching the sheet's numeric rendering),
/// text UTF-8-lossy, blobs as `[N bytes]`.
fn fmt_value_ref(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => fmt_real(f),
        ValueRef::Text(b) => String::from_utf8_lossy(b).into_owned(),
        ValueRef::Blob(b) => format!("[{} bytes]", b.len()),
    }
}

/// Render an `f64` cell like the spreadsheet backends' `fmt_cell`: a whole number
/// in range prints without a decimal point, otherwise Rust's default `f64` text.
fn fmt_real(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

// ---- shared: windowing + SQL text helpers ----

/// True when the cache holds every row in `[r0, r1)`.
fn cache_covers(cache: &Option<WindowCache>, r0: usize, r1: usize) -> bool {
    matches!(cache, Some(c) if c.start <= r0 && r1 <= c.start + c.rows.len())
}

/// The windowing loop shared by both engines: clamp to `total`, serve `[r0, r1) ×
/// [c0, c1)` from the prefetch cache, and on a miss fetch `[r0-MARGIN, r1+MARGIN]`
/// (clamped) via the engine's own `fetch` closure and cache it — so ordinary
/// scrolling is a cache hit. A fetch error yields a blank window (the next frame
/// retries) rather than aborting the UI. The engines differ only in `fetch`.
fn window_cached(
    cache: &mut Option<WindowCache>,
    total: usize,
    r0: usize,
    r1: usize,
    c0: usize,
    c1: usize,
    fetch: impl FnOnce(usize, usize) -> Result<Vec<Vec<String>>, String>,
) -> Vec<Vec<String>> {
    let r1 = r1.min(total);
    if r0 >= r1 {
        return Vec::new();
    }
    if !cache_covers(cache, r0, r1) {
        let start = r0.saturating_sub(MARGIN);
        let len = ((r1 - r0) + 2 * MARGIN).min(total.saturating_sub(start));
        match fetch(start, len) {
            Ok(rows) => *cache = Some(WindowCache { start, rows }),
            Err(_) => {
                *cache = None;
                return Vec::new();
            }
        }
    }
    let cache = cache.as_ref().expect("cache populated above");
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

/// Escape a user query for use inside a `… LIKE '%<pat>%' ESCAPE '\'` pattern:
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

    #[test]
    fn fmt_value_ref_formats_each_type() {
        use rusqlite::types::ValueRef;
        assert_eq!(fmt_value_ref(ValueRef::Null), "");
        assert_eq!(fmt_value_ref(ValueRef::Integer(42)), "42");
        assert_eq!(fmt_value_ref(ValueRef::Real(1.5)), "1.5");
        // A whole-valued real prints without a decimal point (like fmt_cell).
        assert_eq!(fmt_value_ref(ValueRef::Real(3.0)), "3");
        assert_eq!(fmt_value_ref(ValueRef::Text(b"h\xC3\xA9llo")), "héllo");
        // Invalid UTF-8 is lossy (U+FFFD), never a panic.
        assert_eq!(fmt_value_ref(ValueRef::Text(b"a\xFFb")), "a\u{FFFD}b");
        assert_eq!(fmt_value_ref(ValueRef::Blob(&[1, 2, 3, 4])), "[4 bytes]");
    }

    // ---- integration (feature-gated release build) ----
    //
    // Fixtures are created IN the test — Parquet/DuckDB with DuckDB itself, SQLite
    // with rusqlite (NOT DuckDB's network-only SQLite scanner) — so the tests are
    // hermetic and prove each engine's round trip end to end.

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

    /// Create a real SQLite fixture with rusqlite: two user tables (`items`,
    /// `tags`) plus SQLite's internal `sqlite_sequence` (via AUTOINCREMENT), so
    /// the tests exercise the `sqlite_%` exclusion. `items` has a NULL cell.
    fn make_sqlite() -> std::path::PathBuf {
        let path = scratch_path("sq", "db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE items (id INTEGER PRIMARY KEY AUTOINCREMENT, \
                                     label TEXT, note TEXT); \
                 INSERT INTO items (id, label, note) VALUES (1, 'apple', 'x'); \
                 INSERT INTO items (id, label, note) VALUES (2, 'pear', NULL); \
                 CREATE TABLE tags (name TEXT); \
                 INSERT INTO tags VALUES ('fresh');",
            )
            .unwrap();
        } // connection closed (dropped) before we reopen READ-ONLY
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
        let path = make_sqlite();
        let p = path.to_str().unwrap();

        let mut book = DataBook::open(p).expect("open sqlite");
        // One sheet per user table, name-ordered; SQLite's internal
        // `sqlite_sequence` (created by AUTOINCREMENT) is excluded.
        assert_eq!(book.names(), vec!["items", "tags"]);

        book.select(0);
        // dims: 2 rows × 3 cols (id, label, note), synchronous, uncapped.
        assert_eq!(book.dims(), (2, 3, true, false));
        // Real column names read from the prepared statement.
        assert_eq!(book.headers(), vec!["id", "label", "note"]);

        // window: an integer, text, and a NULL → "".
        let win = book.window(0, 2, 0, 3);
        assert_eq!(win[0], vec!["1", "apple", "x"]);
        assert_eq!(win[1], vec!["2", "pear", ""]); // NULL → ""

        // find locates a known cell (col 1 = label), case-insensitively.
        assert_eq!(book.find("APPLE"), vec![(0, 1)]);
        assert!(book.find("zzznope").is_empty());

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn sqlite_set_sql_override_revert_and_restore() {
        let path = make_sqlite();
        let p = path.to_str().unwrap();
        let mut book = DataBook::open(p).expect("open sqlite");

        // Baseline: raw `items` table.
        assert_eq!(book.dims(), (2, 3, true, false));
        assert_eq!(book.active_sql(), None);

        // A valid override replaces the entire view (SQLite SQL, no `db.` prefix).
        let q = "SELECT count(*) AS n FROM items";
        book.set_sql(q).expect("ok");
        assert_eq!(book.dims(), (1, 1, true, false));
        assert_eq!(book.headers(), vec!["n"]);
        assert_eq!(book.window(0, 1, 0, 1), vec![vec!["2"]]);
        assert_eq!(book.active_sql(), Some(q));

        // Empty input reverts to the base table.
        book.set_sql("").expect("revert ok");
        assert_eq!(book.dims(), (2, 3, true, false));
        assert_eq!(book.headers(), vec!["id", "label", "note"]);
        assert_eq!(book.active_sql(), None);

        // Restore-on-error: install a good override, then a bad one must fail and
        // leave the book exactly as it was.
        let good = "SELECT id, label FROM items WHERE id >= 2";
        book.set_sql(good).expect("ok");
        assert_eq!(book.dims(), (1, 2, true, false));
        let before = book.window(0, 1, 0, 2);
        let err = book
            .set_sql("SELECT nonexistent_col FROM items")
            .expect_err("bad query errors");
        assert!(!err.is_empty(), "a human-readable SQLite error is returned");
        assert_eq!(book.dims(), (1, 2, true, false));
        assert_eq!(book.headers(), vec!["id", "label"]);
        assert_eq!(book.window(0, 1, 0, 2), before);
        assert_eq!(book.active_sql(), Some(good));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn sqlite_select_clears_override() {
        let path = make_sqlite();
        let p = path.to_str().unwrap();
        let mut book = DataBook::open(p).expect("open sqlite");

        // Override on sheet 0 (items)…
        book.set_sql("SELECT count(*) AS n FROM items").expect("ok");
        assert_eq!(book.active_sql(), Some("SELECT count(*) AS n FROM items"));

        // …is dropped when switching to another sheet: the tab is the source.
        book.select(1); // tags
        assert_eq!(book.active_sql(), None);
        assert_eq!(book.dims(), (1, 1, true, false));
        assert_eq!(book.headers(), vec!["name"]);
        assert_eq!(book.window(0, 1, 0, 1), vec![vec!["fresh"]]);

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

    /// Large-Parquet benchmark, ignored by default (mirrors the `big_xlsx` one).
    /// Point it at a real file to prove the lazy-windowing claim:
    ///   `SUCHER_BIG_PARQUET=/path/big.parquet cargo test --release big_parquet -- --ignored --nocapture`
    /// Open (schema + COUNT only) and a window near the END must both be fast —
    /// neither materialises the file, so wall-clock is independent of row count.
    #[test]
    #[ignore = "set SUCHER_BIG_PARQUET to a large .parquet to run"]
    fn big_parquet_opens_and_scrolls_lazily() {
        let Ok(path) = std::env::var("SUCHER_BIG_PARQUET") else {
            return;
        };
        let t = std::time::Instant::now();
        let mut book = DataBook::open(&path).expect("open big parquet");
        let (total, ncols, _, _) = book.dims();
        eprintln!(
            "open {total} rows × {ncols} cols in {} ms",
            t.elapsed().as_millis()
        );
        // A window at the far end: lazy LIMIT/OFFSET, not a full scan into memory.
        let t2 = std::time::Instant::now();
        let start = total.saturating_sub(50);
        let win = book.window(start, total, 0, ncols.min(8));
        eprintln!(
            "window @{start} ({} rows) in {} ms",
            win.len(),
            t2.elapsed().as_millis()
        );
        assert!(!win.is_empty());
    }

    #[test]
    fn offline_reads_parquet_without_network() {
        // The core offline guarantee: a FRESH connection carrying only the two
        // both-false pragmas reads a Parquet purely from the statically-compiled
        // build. To prove the static path even on a CONTAMINATED dev machine
        // (whose `~/.duckdb/extensions` may already hold a network-installed
        // parquet extension), we point `extension_directory` at a fresh EMPTY dir
        // — so only a built-in reader can possibly satisfy the query.
        let path = make_parquet();
        let p = path.to_str().unwrap();

        let mut extdir = std::env::temp_dir();
        extdir.push(format!(
            "sucher-empty-extdir-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&extdir).unwrap();

        let conn = open_conn().unwrap();
        conn.execute_batch(&format!(
            "SET extension_directory={};",
            quote_literal(extdir.to_str().unwrap())
        ))
        .expect("set empty extension_directory");

        let n: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM read_parquet({})", quote_literal(p)),
                [],
                |r| r.get(0),
            )
            .expect("read_parquet must work offline from the static build");
        assert_eq!(n, 3);

        std::fs::remove_file(path).ok();
        std::fs::remove_dir_all(extdir).ok();
    }
}
