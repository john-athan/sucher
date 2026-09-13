// Runtime DuckDB backend (ADR 0022), loaded with `dlopen` rather than linked.
//
// DuckDB statically linked is ~38 MB of the binary, and macOS charges for the
// whole image at `exec` rather than for the pages that execute: it cost ~570 ms
// on every start, including `sucher note.md`, for an engine most runs never
// touch. Loading it at runtime only helps if the bytes are OUT of the executable,
// so the library is a real file resolved at first use, exactly like libpdfium
// (`pdfium.rs`), and `data.rs` is the only caller.
//
// This is a hand-written binding because no crate offers one: the `duckdb` crate
// links `libduckdb-sys` statically, and `loadable-extension` is the inverse
// (building extensions that DuckDB loads). The surface is small enough to make
// that cheap. `data.rs` already reads every value through `CAST(... AS VARCHAR)`
// (the grid renders strings), and every result set it asks for is bounded, by
// `LIMIT`, by `FIND_CAP`, or by being schema metadata. So this binding needs one
// value accessor and can materialise results eagerly, which removes the whole
// streaming/statement/lifetime layer a general binding would need.
//
// Absent library → `Connection::open_in_memory` returns `Err`, and the data-file
// viewer surfaces that as a normal open failure, the way an unreadable file does.

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::OnceLock;

/// `idx_t`, DuckDB's index type.
type Idx = u64;
/// `duckdb_state`: 0 is `DuckDBSuccess`, anything else is an error.
type State = i32;

/// `duckdb_result` from `duckdb.h`. NOT opaque: the caller allocates it and the
/// library writes into it, so the layout has to match. Six pointer-width fields,
/// five of them deprecated accessors we never read directly (we call
/// `duckdb_column_count` / `duckdb_row_count` / `duckdb_result_error` instead).
#[repr(C)]
struct RawResult {
    deprecated_column_count: Idx,
    deprecated_row_count: Idx,
    deprecated_rows_changed: Idx,
    deprecated_columns: *mut c_void,
    deprecated_error_message: *mut c_char,
    internal_data: *mut c_void,
}

impl RawResult {
    fn zeroed() -> RawResult {
        RawResult {
            deprecated_column_count: 0,
            deprecated_row_count: 0,
            deprecated_rows_changed: 0,
            deprecated_columns: std::ptr::null_mut(),
            deprecated_error_message: std::ptr::null_mut(),
            internal_data: std::ptr::null_mut(),
        }
    }
}

/// The resolved library and the entry points we use. Held for the life of the
/// process: every handle below borrows from it, and unloading it while a
/// connection is open would be a use-after-free.
struct Api {
    // Keeps the dylib mapped. Never read directly; dropping it would unload the
    // code the function pointers below point into.
    _lib: libloading::Library,
    open: unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> State,
    close: unsafe extern "C" fn(*mut *mut c_void),
    connect: unsafe extern "C" fn(*mut c_void, *mut *mut c_void) -> State,
    disconnect: unsafe extern "C" fn(*mut *mut c_void),
    query: unsafe extern "C" fn(*mut c_void, *const c_char, *mut RawResult) -> State,
    destroy_result: unsafe extern "C" fn(*mut RawResult),
    result_error: unsafe extern "C" fn(*mut RawResult) -> *const c_char,
    column_count: unsafe extern "C" fn(*mut RawResult) -> Idx,
    row_count: unsafe extern "C" fn(*mut RawResult) -> Idx,
    value_varchar: unsafe extern "C" fn(*mut RawResult, Idx, Idx) -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_void),
}

// The library is loaded once and never unloaded, and the DuckDB C API is
// thread-safe for distinct connections. `Connection` itself stays !Send (it holds
// raw handles), which is all `data.rs` needs: a `DataBook` lives on the UI thread.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Option<&'static Api> {
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    let path = resolve_library_path()?;
    // SAFETY: `dlopen` of a path we resolved, then one `dlsym` per entry point.
    // Each signature is transcribed from `duckdb.h` and pinned by the version
    // `build.rs` fetches; a missing symbol fails the whole load rather than
    // leaving a half-bound Api.
    unsafe {
        let lib = libloading::Library::new(&path).ok()?;
        macro_rules! sym {
            ($name:literal) => {{
                let s: libloading::Symbol<_> = lib.get($name).ok()?;
                *s
            }};
        }
        let api = Api {
            open: sym!(b"duckdb_open"),
            close: sym!(b"duckdb_close"),
            connect: sym!(b"duckdb_connect"),
            disconnect: sym!(b"duckdb_disconnect"),
            query: sym!(b"duckdb_query"),
            destroy_result: sym!(b"duckdb_destroy_result"),
            result_error: sym!(b"duckdb_result_error"),
            column_count: sym!(b"duckdb_column_count"),
            row_count: sym!(b"duckdb_row_count"),
            value_varchar: sym!(b"duckdb_value_varchar"),
            free: sym!(b"duckdb_free"),
            _lib: lib,
        };
        Some(api)
    }
}

/// The library file name for this platform.
fn lib_file_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "libduckdb.dylib"
    }
    #[cfg(target_os = "windows")]
    {
        "duckdb.dll"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "libduckdb.so"
    }
}

/// Locate libduckdb, in priority order:
///
/// 1. `$SUCHER_DUCKDB_LIB` (explicit full path, used for dev / overrides),
/// 2. beside the running executable (where `build.rs` stages it and a packager
///    installs it),
/// 3. one directory up, which is where a `cargo test` binary sits relative to the
///    profile directory the staged copy lands in (`target/debug/deps/…`),
/// 4. common system library directories.
///
/// Returns `None` if not found; the caller then reports the data file as
/// unopenable. Mirrors `pdfium::resolve_library_path`.
fn resolve_library_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SUCHER_DUCKDB_LIB") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let file = lib_file_name();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            dirs.push(d.to_path_buf());
            if let Some(up) = d.parent() {
                dirs.push(up.to_path_buf());
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        dirs.push(PathBuf::from("/opt/homebrew/lib"));
        dirs.push(PathBuf::from("/usr/local/lib"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        dirs.push(PathBuf::from("/usr/local/lib"));
        dirs.push(PathBuf::from("/usr/lib"));
    }
    dirs.into_iter()
        .map(|d| d.join(file))
        .find(|cand| cand.is_file())
}

/// The message shown when the library is missing. Names the two ways to fix it,
/// because "cannot open file.parquet" alone would look like a corrupt file.
fn missing_library() -> String {
    format!(
        "{} not found; install it beside the sucher binary or set SUCHER_DUCKDB_LIB",
        lib_file_name()
    )
}

/// One materialised result: every cell already converted to text, `None` for SQL
/// NULL. Bounded by construction, see the module note.
#[derive(Debug)]
pub struct Table {
    rows: Vec<Vec<Option<String>>>,
}

impl Table {
    pub fn rows(&self) -> &[Vec<Option<String>>] {
        &self.rows
    }

    /// Cell `(row, col)` as text, `""` for NULL and for out-of-range indices, so
    /// callers that know their own query's shape need no bounds dance.
    pub fn text(&self, row: usize, col: usize) -> String {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .and_then(|c| c.clone())
            .unwrap_or_default()
    }

    /// Cell `(row, col)` parsed as an integer, `0` when absent or unparseable.
    /// Used for `count(*)` and `row_number()`, which DuckDB renders as digits
    /// through the same VARCHAR conversion as everything else.
    pub fn int(&self, row: usize, col: usize) -> i64 {
        self.text(row, col).trim().parse().unwrap_or(0)
    }
}

/// An in-memory DuckDB database and its one connection.
///
/// Not `Send`: the handles belong to the thread that made them, which is all
/// `data.rs` needs (a `DataBook` lives on the UI thread). Closed on drop.
pub struct Connection {
    db: *mut c_void,
    con: *mut c_void,
}

impl Connection {
    /// Open an in-memory database. `Err` when the library is missing, which is
    /// how a build without the sidecar reports that data files cannot be opened.
    pub fn open_in_memory() -> Result<Connection, String> {
        let api = api().ok_or_else(missing_library)?;
        let mut db: *mut c_void = std::ptr::null_mut();
        let mut con: *mut c_void = std::ptr::null_mut();
        // SAFETY: a NULL path means in-memory; both out-params are written only
        // on success, and we bail before using them otherwise.
        unsafe {
            if (api.open)(std::ptr::null(), &mut db) != 0 {
                return Err("could not open an in-memory DuckDB database".into());
            }
            if (api.connect)(db, &mut con) != 0 {
                (api.close)(&mut db);
                return Err("could not connect to the in-memory DuckDB database".into());
            }
        }
        Ok(Connection { db, con })
    }

    /// Run one or more `;`-separated statements for effect, discarding results.
    /// `duckdb_query` executes every statement in the string and returns the last
    /// one's result, so no splitting is needed (and none is done: splitting on
    /// `;` would cut a path or a pattern literal in half). The offline guarantee
    /// rides on this, `open_conn` sends both `SET` pragmas as one batch, so
    /// `execute_batch_runs_every_statement` asserts it rather than assuming it.
    pub fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.query(sql).map(|_| ())
    }

    /// Run one statement and materialise the whole result as text.
    pub fn query(&self, sql: &str) -> Result<Table, String> {
        let api = api().ok_or_else(missing_library)?;
        let c_sql = CString::new(sql).map_err(|_| "query contains a NUL byte".to_string())?;
        let mut raw = RawResult::zeroed();
        // SAFETY: `raw` matches `duckdb_result`'s layout and outlives every call
        // below; it is destroyed on both the success and the error path. Each
        // `value_varchar` pointer is owned by us and freed with `duckdb_free`.
        unsafe {
            let state = (api.query)(self.con, c_sql.as_ptr(), &mut raw);
            if state != 0 {
                let p = (api.result_error)(&mut raw);
                let msg = if p.is_null() {
                    "DuckDB query failed".to_string()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                };
                (api.destroy_result)(&mut raw);
                return Err(msg);
            }
            let ncols = (api.column_count)(&mut raw) as usize;
            let nrows = (api.row_count)(&mut raw) as usize;
            let mut rows = Vec::with_capacity(nrows);
            for r in 0..nrows {
                let mut row = Vec::with_capacity(ncols);
                for c in 0..ncols {
                    let p = (api.value_varchar)(&mut raw, c as Idx, r as Idx);
                    if p.is_null() {
                        // NULL, and also any value DuckDB declines to convert.
                        row.push(None);
                    } else {
                        row.push(Some(CStr::from_ptr(p).to_string_lossy().into_owned()));
                        (api.free)(p as *mut c_void);
                    }
                }
                rows.push(row);
            }
            (api.destroy_result)(&mut raw);
            Ok(Table { rows })
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let Some(api) = api() else { return };
        // SAFETY: both handles were produced by `open_in_memory` and are dropped
        // exactly once; DuckDB tolerates a null pointer here.
        unsafe {
            (api.disconnect)(&mut self.con);
            (api.close)(&mut self.db);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_file_name_matches_platform() {
        let name = lib_file_name();
        #[cfg(target_os = "macos")]
        assert_eq!(name, "libduckdb.dylib");
        #[cfg(target_os = "windows")]
        assert_eq!(name, "duckdb.dll");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(name, "libduckdb.so");
    }

    #[test]
    fn missing_library_message_names_both_remedies() {
        let m = missing_library();
        assert!(m.contains(lib_file_name()));
        assert!(m.contains("SUCHER_DUCKDB_LIB"));
    }

    #[test]
    fn raw_result_matches_the_c_layout() {
        // Six pointer-width fields in `duckdb.h`. The library writes into this
        // struct, so a mismatch would be memory corruption, not a type error.
        assert_eq!(
            std::mem::size_of::<RawResult>(),
            6 * std::mem::size_of::<usize>()
        );
    }

    /// Everything below needs the real library. It is required rather than
    /// skipped, because `data.rs`'s own tests already fail hard without it, and a
    /// test that passes vacuously when the library goes missing is worse than one
    /// that says so.
    fn conn() -> Connection {
        Connection::open_in_memory().expect("libduckdb must be staged beside the test binary")
    }

    #[test]
    fn query_materialises_values_and_nulls() {
        let c = conn();
        let t = c
            .query("SELECT 1 AS a, 'two' AS b, NULL AS c")
            .expect("query");
        assert_eq!(t.rows().len(), 1);
        assert_eq!(t.rows()[0].len(), 3);
        assert_eq!(t.text(0, 0), "1");
        assert_eq!(t.text(0, 1), "two");
        assert_eq!(t.text(0, 2), ""); // NULL reads as empty
        assert_eq!(t.rows()[0][2], None); // and is distinguishable from ""
        assert_eq!(t.int(0, 0), 1);
    }

    #[test]
    fn out_of_range_reads_are_empty_not_panics() {
        let c = conn();
        let t = c.query("SELECT 1").expect("query");
        assert_eq!(t.text(99, 0), "");
        assert_eq!(t.text(0, 99), "");
        assert_eq!(t.int(99, 99), 0);
    }

    #[test]
    fn a_bad_query_returns_duckdbs_own_message() {
        let c = conn();
        let err = c.query("SELECT nonexistent_col").expect_err("must fail");
        assert!(!err.is_empty());
    }

    #[test]
    fn execute_batch_runs_every_statement() {
        // Not a formality: `open_conn` sets both offline pragmas in one batch, so
        // a DuckDB that ran only the first statement would silently leave
        // `autoload_known_extensions` on and break the offline guarantee.
        let c = conn();
        c.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (7);")
            .expect("batch");
        let got = c.query("SELECT x FROM t").expect("read back");
        assert_eq!(got.int(0, 0), 7, "the second statement ran");

        c.execute_batch(
            "SET autoinstall_known_extensions=false; SET autoload_known_extensions=false;",
        )
        .expect("pragmas");
        let flag = c
            .query("SELECT current_setting('autoload_known_extensions')")
            .expect("read flag");
        assert_eq!(flag.text(0, 0), "false", "the second SET took effect");
    }

    #[test]
    fn a_semicolon_inside_a_literal_survives() {
        // Nothing splits on `;`, so a literal containing one is not cut in half.
        let c = conn();
        let t = c.query("SELECT 'a;b' AS s").expect("query");
        assert_eq!(t.text(0, 0), "a;b");
    }
}
