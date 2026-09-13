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
roughly half disk I/O, half page hashing. Neither halves is reachable from our
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
- The same reasoning applies with ~5x the weight to DuckDB (~570 ms). That is a
  larger change, because `data.rs` uses the `duckdb` crate's Rust API
  (`Connection`, `prepare`, `query`, `rows`) and runtime loading needs a shim
  over the C API; no crate provides one (`loadable-extension` is the inverse,
  for building extensions DuckDB loads). DuckDB publishes prebuilt
  `libduckdb-*.zip` for every target sucher targets, in the same shape as the
  pdfium assets `build.rs` already fetches, so the build-side machinery carries
  over unchanged.
