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
// `LIMIT`, by `FIND_CAP`, or by being schema metadata. So this binding can
// materialise results eagerly, which removes the whole streaming/statement/
// lifetime layer a general binding would need.
//
// Results are read through the data chunk API (`duckdb_fetch_chunk`), because the
// row/cell accessors (`duckdb_row_count`, `duckdb_value_varchar`) are deprecated
// since 1.0 and return empty strings on 2.0. No chunk-level call renders a cell
// as text (DuckDB's own rendering needs a `duckdb_value`, which cannot be made
// from a vector), so cells are decoded here, for the types a text grid query
// yields: VARCHAR, booleans, integers and bare NULL. Anything else is an error that names
// the column, never a silently different rendering; callers cast to VARCHAR.
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
/// five of them deprecated fields we never read directly (we call
/// `duckdb_column_count` / `duckdb_result_error` / `duckdb_fetch_chunk` instead).
/// The layout is identical in DuckDB 1.5.5 and 2.0.0. `Copy` because
/// `duckdb_fetch_chunk` takes the struct by value.
#[repr(C)]
#[derive(Clone, Copy)]
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

/// `duckdb_string_t`: 16 bytes, inline or pointer form. Opaque here because only
/// `duckdb_string_t_length` / `duckdb_string_t_data` look inside it.
#[repr(C)]
struct StringT {
    a: u64,
    b: u64,
}

/// `duckdb_type` ids from `duckdb.h`, the same in 1.x and 2.x.
mod ty {
    pub const BOOLEAN: i32 = 1;
    pub const TINYINT: i32 = 2;
    pub const SMALLINT: i32 = 3;
    pub const INTEGER: i32 = 4;
    pub const BIGINT: i32 = 5;
    pub const UTINYINT: i32 = 6;
    pub const USMALLINT: i32 = 7;
    pub const UINTEGER: i32 = 8;
    pub const UBIGINT: i32 = 9;
    pub const HUGEINT: i32 = 16;
    pub const VARCHAR: i32 = 17;
    pub const UHUGEINT: i32 = 32;
    /// The type of a bare `NULL`; every row is NULL, there is no data to read.
    pub const SQLNULL: i32 = 36;
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
    column_type: unsafe extern "C" fn(*mut RawResult, Idx) -> i32,
    fetch_chunk: unsafe extern "C" fn(RawResult) -> *mut c_void,
    destroy_chunk: unsafe extern "C" fn(*mut *mut c_void),
    chunk_size: unsafe extern "C" fn(*mut c_void) -> Idx,
    chunk_vector: unsafe extern "C" fn(*mut c_void, Idx) -> *mut c_void,
    vector_data: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    vector_validity: unsafe extern "C" fn(*mut c_void) -> *mut u64,
    row_is_valid: unsafe extern "C" fn(*mut u64, Idx) -> bool,
    string_length: unsafe extern "C" fn(StringT) -> u32,
    string_data: unsafe extern "C" fn(*mut StringT) -> *const c_char,
}

// The library is loaded once and never unloaded, and the DuckDB C API is
// thread-safe for distinct connections. `Connection` itself stays !Send (it holds
// raw handles), which is all `data.rs` needs: a `DataBook` lives on the UI thread.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load).as_ref().map_err(|e| e.clone())
}

/// Whether a `duckdb_library_version()` string ("v1.5.5") is one whose C ABI this
/// file transcribes. `duckdb_result` is caller-allocated, so a library from
/// another major version can change its layout and return garbage without any
/// error. 1.x and 2.x share the layout and the chunk API; the deprecated cell
/// accessors that 2.0 broke are not used, so both are accepted.
fn version_is_supported(v: &str) -> bool {
    let v = v.strip_prefix('v').unwrap_or(v);
    v.starts_with("1.") || v.starts_with("2.")
}

fn load() -> Result<Api, String> {
    let path = resolve_library_path().ok_or_else(missing_library)?;
    // SAFETY: `dlopen` of a path we resolved, then one `dlsym` per entry point.
    // Each signature is transcribed from `duckdb.h` and pinned by the version
    // `build.rs` fetches; a missing symbol fails the whole load rather than
    // leaving a half-bound Api.
    unsafe {
        let unusable = || format!("{} is not a usable DuckDB library", path.display());
        let lib = libloading::Library::new(&path).map_err(|_| unusable())?;
        macro_rules! sym {
            ($name:literal) => {{
                let s: libloading::Symbol<_> = lib.get($name).map_err(|_| unusable())?;
                *s
            }};
        }
        let version: unsafe extern "C" fn() -> *const c_char = sym!(b"duckdb_library_version");
        let vp = version();
        let found = if vp.is_null() {
            String::new()
        } else {
            CStr::from_ptr(vp).to_string_lossy().into_owned()
        };
        if !version_is_supported(&found) {
            return Err(format!(
                "{} is DuckDB {}, but sucher needs 1.x or 2.x; set SUCHER_DUCKDB_LIB to a 1.x or 2.x libduckdb",
                path.display(),
                if found.is_empty() {
                    "of unknown version"
                } else {
                    &found
                }
            ));
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
            column_type: sym!(b"duckdb_column_type"),
            fetch_chunk: sym!(b"duckdb_fetch_chunk"),
            destroy_chunk: sym!(b"duckdb_destroy_data_chunk"),
            chunk_size: sym!(b"duckdb_data_chunk_get_size"),
            chunk_vector: sym!(b"duckdb_data_chunk_get_vector"),
            vector_data: sym!(b"duckdb_vector_get_data"),
            vector_validity: sym!(b"duckdb_vector_get_validity"),
            row_is_valid: sym!(b"duckdb_validity_row_is_valid"),
            string_length: sym!(b"duckdb_string_t_length"),
            string_data: sym!(b"duckdb_string_t_data"),
            _lib: lib,
        };
        Ok(api)
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
/// 4. `libexec` beside the real (symlink-resolved) executable, where the
///    Homebrew formula puts its pinned copy, ahead of Homebrew's own `duckdb`
///    in `lib/libduckdb.dylib`,
/// 5. common system library directories.
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
        if let Some(keg) = std::fs::canonicalize(&exe).ok().and_then(|real| {
            real.parent()
                .and_then(|d| d.parent())
                .map(|k| k.join("libexec"))
        }) {
            dirs.push(keg);
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
    /// Used for `count(*)` and `row_number()`.
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
        let api = api()?;
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
        // Not `query`: a batch's last statement may yield a type `query` does not
        // decode (nobody reads it), and that must not fail the batch.
        self.run(sql, false).map(|_| ())
    }

    /// Run one statement and materialise the whole result as text.
    pub fn query(&self, sql: &str) -> Result<Table, String> {
        self.run(sql, true)
    }

    fn run(&self, sql: &str, read: bool) -> Result<Table, String> {
        let api = api()?;
        let c_sql = CString::new(sql).map_err(|_| "query contains a NUL byte".to_string())?;
        let mut raw = RawResult::zeroed();
        // SAFETY: `raw` matches `duckdb_result`'s layout and outlives every call
        // below; it is destroyed on every path. Chunks are owned by us and
        // destroyed after each read; vector and string pointers are borrowed from
        // the chunk and copied out before it goes.
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
            if !read {
                (api.destroy_result)(&mut raw);
                return Ok(Table { rows: Vec::new() });
            }
            let ncols = (api.column_count)(&mut raw) as usize;
            let types: Vec<i32> = (0..ncols)
                .map(|c| (api.column_type)(&mut raw, c as Idx))
                .collect();
            if let Some((c, t)) = types.iter().enumerate().find(|(_, t)| !decodable(**t)) {
                (api.destroy_result)(&mut raw);
                return Err(format!(
                    "column {c} has DuckDB type id {t}, which sucher does not decode; \
                     CAST it to VARCHAR"
                ));
            }
            let mut rows: Vec<Vec<Option<String>>> = Vec::new();
            loop {
                let mut chunk = (api.fetch_chunk)(raw);
                if chunk.is_null() {
                    break;
                }
                let n = (api.chunk_size)(chunk) as usize;
                let first = rows.len();
                rows.resize_with(first + n, || Vec::with_capacity(ncols));
                for (c, &t) in types.iter().enumerate() {
                    if t == ty::SQLNULL {
                        for row in &mut rows[first..] {
                            row.push(None);
                        }
                        continue;
                    }
                    let vec = (api.chunk_vector)(chunk, c as Idx);
                    let data = (api.vector_data)(vec) as *mut u8;
                    let validity = (api.vector_validity)(vec);
                    for r in 0..n {
                        // A null validity mask means every row is valid.
                        let valid = validity.is_null() || (api.row_is_valid)(validity, r as Idx);
                        rows[first + r].push(if valid {
                            Some(cell(api, t, data, r))
                        } else {
                            None
                        });
                    }
                }
                (api.destroy_chunk)(&mut chunk);
            }
            (api.destroy_result)(&mut raw);
            Ok(Table { rows })
        }
    }
}

fn decodable(t: i32) -> bool {
    matches!(
        t,
        ty::BOOLEAN
            | ty::TINYINT
            | ty::SMALLINT
            | ty::INTEGER
            | ty::BIGINT
            | ty::UTINYINT
            | ty::USMALLINT
            | ty::UINTEGER
            | ty::UBIGINT
            | ty::HUGEINT
            | ty::UHUGEINT
            | ty::VARCHAR
            | ty::SQLNULL
    )
}

/// Render row `r` of a flat vector of type `t` the way DuckDB's VARCHAR cast
/// does. `t` has passed `decodable`.
///
/// # Safety
/// `data` must be the data pointer of a vector of type `t` with more than `r` rows.
unsafe fn cell(api: &Api, t: i32, data: *mut u8, r: usize) -> String {
    macro_rules! at {
        ($ty:ty) => {
            data.cast::<$ty>().add(r).read_unaligned()
        };
    }
    match t {
        ty::BOOLEAN => (if at!(u8) != 0 { "true" } else { "false" }).to_string(),
        ty::TINYINT => at!(i8).to_string(),
        ty::SMALLINT => at!(i16).to_string(),
        ty::INTEGER => at!(i32).to_string(),
        ty::BIGINT => at!(i64).to_string(),
        ty::UTINYINT => at!(u8).to_string(),
        ty::USMALLINT => at!(u16).to_string(),
        ty::UINTEGER => at!(u32).to_string(),
        ty::UBIGINT => at!(u64).to_string(),
        // `duckdb_hugeint` is { lower: u64, upper: i64 }.
        ty::HUGEINT => {
            let lo = at!(u64) as u128;
            let hi = data.cast::<i64>().add(r * 2 + 1).read_unaligned();
            ((hi as i128) << 64 | lo as i128).to_string()
        }
        ty::UHUGEINT => {
            let lo = at!(u64) as u128;
            let hi = data.cast::<u64>().add(r * 2 + 1).read_unaligned() as u128;
            (hi << 64 | lo).to_string()
        }
        _ => {
            let mut s = data.cast::<StringT>().add(r).read_unaligned();
            let len = (api.string_length)(StringT { a: s.a, b: s.b }) as usize;
            let p = (api.string_data)(&mut s) as *const u8;
            String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned()
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let Ok(api) = api() else { return };
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
    fn duckdb_1x_and_2x_pass_the_version_gate() {
        assert!(version_is_supported("v1.5.5"));
        assert!(version_is_supported("1.0.0"));
        assert!(version_is_supported("v2.0.0"));
        assert!(!version_is_supported("v3.0.0"));
        assert!(!version_is_supported("v0.10.2"));
        assert!(!version_is_supported(""));
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
    fn every_decoded_type_renders_as_the_old_varchar_accessor_did() {
        // Expected strings were produced by `duckdb_value_varchar` on 1.5.5.
        let c = conn();
        let t = c
            .query(
                "SELECT true, false, (-5)::TINYINT, (-300)::SMALLINT, (-70000)::INTEGER, \
                 (-5000000000)::BIGINT, 200::UTINYINT, 60000::USMALLINT, \
                 4000000000::UINTEGER, 18446744073709551615::UBIGINT, \
                 (-170141183460469231731687303715884105728)::HUGEINT, \
                 340282366920938463463374607431768211455::UHUGEINT, \
                 'short', 'a string well past the twelve byte inline limit', '', \
                 'h\u{e9}llo \u{4e16}\u{754c}', NULL::INTEGER, NULL::VARCHAR",
            )
            .expect("query");
        let want = [
            Some("true"),
            Some("false"),
            Some("-5"),
            Some("-300"),
            Some("-70000"),
            Some("-5000000000"),
            Some("200"),
            Some("60000"),
            Some("4000000000"),
            Some("18446744073709551615"),
            Some("-170141183460469231731687303715884105728"),
            Some("340282366920938463463374607431768211455"),
            Some("short"),
            Some("a string well past the twelve byte inline limit"),
            Some(""),
            Some("h\u{e9}llo \u{4e16}\u{754c}"),
            None,
            None,
        ];
        let got: Vec<Option<&str>> = t.rows()[0].iter().map(|v| v.as_deref()).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn types_cast_to_varchar_keep_duckdbs_rendering() {
        // The path data.rs takes for everything else: DuckDB renders, we copy.
        let c = conn();
        let t = c
            .query(
                "SELECT CAST(DATE '2024-02-29' AS VARCHAR), CAST(1.50::DECIMAL(5,2) AS VARCHAR), \
                 CAST([1, NULL, 3] AS VARCHAR), CAST({'a': 1, 'b': 'x'} AS VARCHAR), \
                 CAST('\\xAA\\xBB'::BLOB AS VARCHAR), CAST(1.0::DOUBLE AS VARCHAR)",
            )
            .expect("query");
        assert_eq!(t.text(0, 0), "2024-02-29");
        assert_eq!(t.text(0, 1), "1.50");
        assert_eq!(t.text(0, 2), "[1, NULL, 3]");
        assert_eq!(t.text(0, 3), "{'a': 1, 'b': x}");
        assert_eq!(t.text(0, 4), "\\xAA\\xBB");
        assert_eq!(t.text(0, 5), "1.0");
    }

    #[test]
    fn an_undecoded_type_is_an_error_not_a_wrong_cell() {
        let c = conn();
        let err = c.query("SELECT DATE '2024-02-29'").expect_err("date");
        assert!(err.contains("column 0") && err.contains("VARCHAR"), "{err}");
    }

    #[test]
    fn results_spanning_several_chunks_arrive_whole() {
        let c = conn();
        let t = c
            .query("SELECT i, CAST(i AS VARCHAR) FROM range(5000) t(i)")
            .expect("query");
        assert_eq!(t.rows().len(), 5000);
        assert_eq!(t.int(4999, 0), 4999);
        assert_eq!(t.text(4999, 1), "4999");
    }

    #[test]
    fn a_semicolon_inside_a_literal_survives() {
        // Nothing splits on `;`, so a literal containing one is not cut in half.
        let c = conn();
        let t = c.query("SELECT 'a;b' AS s").expect("query");
        assert_eq!(t.text(0, 0), "a;b");
    }
}
