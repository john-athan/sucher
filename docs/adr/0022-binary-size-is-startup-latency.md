# 0022, binary size is startup latency

Status: accepted
Date: 2026-09-13

Amends ADR 0015 (pdfium embedding) and ADR 0016 (DuckDB bundling).

## Context

Starting sucher took 1 to 2 seconds on a warm machine, every time, whatever the
file. Stage timestamps from a real Ghostty window, `sucher .` on a cold binary:

| stage | elapsed |
| --- | --- |
| shell `exec` to `main()` | **1336 ms** |
| `main` to config loaded | 0.6 ms |
| config to dispatch | 0.1 ms |
| graphics probe (`ImagePane::new`) | 0.4 ms |
| dispatch to first frame drawn | ~1 ms |

sucher's own startup is ~13 ms. `sample` on a cold start puts 894 of 894 samples
at `_dyld_start + 0`: the process is blocked in the kernel on exec, mapping and
code-signature-validating a 76 MB image, before one instruction of sucher runs.
The same binary run twice costs 1.24 s then 0.01 s, and pre-reading it with `cat`
(page cache warm, signature still unvalidated) gives 0.66 s, so the cost splits
roughly half disk I/O, half page hashing. Neither half is reachable from our
code.

The decisive measurement is what the cost attaches to. A 0.3 MB test binary and
the same binary plus a 40 MB blob that **no code path ever reads**:

| binary | cold start |
| --- | --- |
| 0.3 MB | 0.11 s |
| 0.3 MB + 40 MB inert blob | 0.74 s |

**macOS charges for the whole image at exec, not for the pages that execute.**
Roughly 15 ms per MB on an M-series laptop. Lazily loading a library at runtime
therefore saves nothing if its bytes are still inside the executable, and dead
weight costs the same as hot code.

That invalidates the reasoning behind two earlier decisions. ADR 0015 embeds
libpdfium with `include_bytes!` so `cargo install sucher` is self-contained, on
the understanding that a runtime `dlopen` keeps it out of the way. It does not:
the 7.2 MB is paid on every `sucher note.md`. ADR 0016 links DuckDB statically
for the same self-containment reason, at ~38 MB.

Budget of the 62 MB binary (after `strip`), at the measured rate:

| | size | cost per start |
| --- | --- | --- |
| DuckDB, statically linked | ~38 MB | ~570 ms |
| pdfium, embedded uncompressed | 7.2 MB | ~110 ms |
| everything else | ~17 MB | ~255 ms |

## Decision

**Treat bytes in the executable as a per-invocation latency budget, not as free
disk space.** A payload that most runs never touch does not belong inside the
binary, however lazily it is used.

Three changes follow.

- **`strip = true` in `[profile.release]`.** The symbol table is ~16 MB that
  never executes. Costs nothing but panic-backtrace symbol names. 1.49 s to
  1.23 s.
- **pdfium embedding becomes the opt-in `embed-pdfium` feature.** The default
  build resolves the library at runtime from beside the executable, which
  `resolve_library_path` already searched first (ADR 0015 fixed that order:
  `$SUCHER_PDFIUM_LIB`, beside the executable, system lib dirs, embedded copy).
  Only the last entry changes: it is now absent unless asked for.
- **`build.rs` stages the sidecar beside the binary it builds.** It already
  fetched and checksummed the pinned library into `OUT_DIR`; it now also copies
  it into the profile directory, so a plain `cargo build` leaves `sucher` and
  `libpdfium.dylib` side by side and the runtime resolver finds it with no
  Makefile plumbing and no second copy of the pinned version and checksum.

The self-containment ADR 0015 wanted is preserved where it is actually needed.
`cargo install` copies only the binary and can place no sidecar, so
`cargo install sucher --features embed-pdfium` still produces one file that
carries its own engine. A packager places the sidecar and pays nothing for it:
the Homebrew formula builds with `cargo build` (not `cargo install`, which would
discard the staged library) and installs it into `lib`.

Every failure path stays soft, as before. No sidecar and no embedded copy means
PDF falls back to poppler, which is ADR 0015's existing behaviour for an
unsupported target or a failed download.

## Consequences

- Startup drops from 1.49 s to 0.86 s median (paired cold starts, fresh inode,
  same machine). 76 MB to 55 MB.
- `cargo install sucher` without the feature no longer gets the fast PDF path; it
  falls back to poppler, and `build.rs` emits no warning about it because nothing
  failed. This is the one real regression, and it is the price of not charging
  every markdown file ~110 ms for a PDF engine.
- The Homebrew formula can no longer use `cargo install`, because that copies
  only the binary. It uses `cargo build --release --locked` plus explicit
  `bin.install` / `lib.install`, and the `lib.install` is conditional so an
  offline or failed fetch degrades to poppler instead of failing the build.
- `libpdfium.dylib` lands in the shared Homebrew `lib` prefix under a generic
  name. A future formula shipping its own libpdfium would collide at link time;
  sucher prefers the copy beside its own executable, so the collision is
  reportable rather than silent.

## DuckDB, the other ~570 ms

Same decision, five times the weight, and it needed more than a feature flag.
`data.rs` used the `duckdb` crate's Rust API, which links `libduckdb-sys`
statically; no crate offers a runtime-loaded DuckDB (`loadable-extension` is the
inverse, for building extensions DuckDB loads), so the binding is hand-written:
`src/duckdyn.rs`, ~15 entry points over the C API behind `libloading`.

It stays small because of a property `data.rs` already had. Every value it reads
goes through `CAST(... AS VARCHAR)`, since the grid renders strings, and every
result set is bounded, by `LIMIT`, by `FIND_CAP`, or by being schema metadata. So
the binding needs one value accessor and can materialise results eagerly, which
removes the streaming/statement/lifetime layer a general-purpose binding needs.
`duckdb` and `libduckdb-sys` leave the dependency tree entirely, and with them the
DuckDB C++ compile, which dominated build times.

Two behaviours were verified rather than assumed, because the offline guarantee
(ADR 0016) depends on them and neither is visible in the type system:

- The official prebuilt library has the parquet and json readers built in.
  `read_parquet` and `read_json_auto` both work with `autoinstall_known_extensions`
  and `autoload_known_extensions` false, and `sqlite_scan` is still refused rather
  than downloaded. `offline_reads_parquet_without_network` keeps proving it.
- `duckdb_query` executes *every* statement in a `;`-separated batch, not just the
  first. `open_conn` sets both offline pragmas in one batch, so a version that ran
  only the first would silently leave autoloading on.
  `execute_batch_runs_every_statement` asserts both statements take effect.

Distribution differs from pdfium's, because homebrew-core already ships a `duckdb`
formula providing `/opt/homebrew/lib/libduckdb.dylib`, which is where
`duckdyn::resolve_library_path` looks anyway. The formula depends on it instead of
installing a second copy into a prefix that formula owns, and `build.rs` skips its
own download when a system copy is already present. There is no homebrew-core
`pdfium`, which is why that one remains a staged sidecar.

## Consequences of the DuckDB half

- Startup drops from 0.86 s to **0.38 s** median, and the binary from 55 MB to
  19 MB. Against the 76 MB starting point: 1.16 s to 0.38 s.
- Opening a Parquet, JSONL or DuckDB file now costs a one-time `dlopen` of a
  ~57 MB library, once per process. Browsing folders, which is the common case,
  never pays it.
- Without the library, data files report `libduckdb.dylib not found; install it
  beside the sucher binary or set SUCHER_DUCKDB_LIB` and everything else works
  unchanged. SQLite is unaffected either way: rusqlite stays statically bundled,
  for the reason ADR 0016 gives (DuckDB's SQLite scanner is a network-only
  extension).
- sucher now follows whatever DuckDB 1.x the system provides rather than a
  version compiled into it. The binding uses a small, long-stable slice of the C
  API, and `duckdb_result`'s layout is asserted in a unit test, so a drift shows
  up as a test failure rather than as memory corruption.
- `cargo install sucher` no longer yields a self-contained data viewer. There is
  no `embed-duckdb` counterpart to `embed-pdfium`, deliberately: embedding
  ~38 MB would restore exactly the cost this ADR removes.
