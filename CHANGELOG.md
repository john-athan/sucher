# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses semantic
versioning while pre-1.0 (breaking changes may land in minor releases).

## [Unreleased]

## [0.5.0] - 2026-07-21

### Added
- **Data files open in the grid, with an interactive SQL prompt.** Parquet
  (`.parquet` `.pq`), newline-delimited JSON (`.jsonl` `.ndjson`), SQLite
  (`.sqlite` `.sqlite3` `.db` `.db3`), and DuckDB (`.duckdb` `.ddb`) now render
  in the existing spreadsheet grid, backed by a new `DataBook` that embeds
  **DuckDB** (statically bundled from vendored source). Databases are opened
  **read-only** and each table becomes a sheet (switch with `Tab`); Parquet/JSONL
  are a single sheet named for the file stem. Columns keep their real names and
  types, and DuckDB's canonical text gives correct ISO dates/timestamps with NULL
  shown blank — no serial-number date wart. Press `:` in the grid for a live
  **SQL prompt** over the current file: the result replaces the view (schema,
  rows, and `/` search follow it), you can `FROM <stem>` a single-file source or
  `FROM <table>`/join across a database's tables, a parse/bind error keeps your
  text and the previous view intact, and empty input reverts to the base table.
  Reads are **lazy and uncapped** — the grid windows rows on demand (`LIMIT`/
  `OFFSET` + prefetch) and takes the schema from `DESCRIBE` without executing, so
  a file opens instantly regardless of size and scrolls to the end with no row
  cap (unlike the streaming `.xlsx`/CSV backends). It is **fully offline**: every
  DuckDB connection sets `autoinstall_known_extensions = false`, so reading a
  data file never touches the network. Behind the **default-on `data` Cargo
  feature** — `cargo install sucher` includes it (release binary ~65 MB with
  DuckDB bundled), and `cargo install --no-default-features` builds the lean
  ~26 MB binary without it. Arrow/Feather files are deliberately excluded (the
  bundled build lacks the Arrow file reader; Parquet covers the columnar need).
  See ADR 0016.

## [0.4.0] - 2026-07-21

### Changed
- **The fast pdfium PDF path is now self-contained.** `build.rs` fetches the
  pinned, checksum-verified `libpdfium` for the build target and embeds it in the
  binary (materialised to a cache dir on first use), so a plain `cargo install
  sucher` gets the ~100× render speed with no extra steps — no `make`, no sidecar.
  Build-time fetch is soft: offline builds, docs.rs, an unsupported target, or
  `SUCHER_PDFIUM_NO_EMBED=1` skip embedding and fall back to poppler. An external
  `libpdfium` (via `SUCHER_PDFIUM_LIB` or beside the binary) still overrides the
  embedded copy. The Makefile no longer needs any pdfium plumbing.

## [0.3.0] - 2026-07-21

### Added
- **Open in native app** — `x` hands the selected/open file to the OS default
  application, from the directory browser and from every fullscreen viewer. The
  *source* file is opened, not the rendered form (docx → the `.docx`, Keynote →
  the `.key`); works even for formats sucher has no in-app viewer for (ADR 0014).
- Repo HEAD readout on the browser's breadcrumb row: current branch (or
  detached commit), ahead/behind vs upstream, and a dirty dot — `⎇ main ↑2 ↓1 ●`
  (ADR 0004 amendment). Follows the existing `git` toggle.

### Changed
- **PDF rendering now uses pdfium** (Chrome's engine) when its runtime library is
  present, falling back to poppler otherwise (ADR 0015). Scanned pages that took
  ~4.5 s with `pdftocairo` now render in ~30–50 ms (~100×); parsing is done once
  in-process instead of re-spawned per page, with no PNG-to-temp round-trip.
  `make`/`make install` fetch the pinned, checksum-verified `libpdfium` and place
  it beside the binary; `SUCHER_PDFIUM_LIB` overrides the path.
- PDF pages render on a background thread and the current page's neighbours are
  prefetched into the cache, so stepping through a PDF no longer blocks the UI on
  each render — navigation is near-instant once neighbours are warm.

### Fixed
- New clippy 1.97 lints (`bool_assert_comparison`, `type_complexity`,
  `useless_vec`) under `-D warnings`.

## [0.2.0] - 2026-07-09

### Added
- Recursive live search in Browser Mode with a content predicate, powered by
  ripgrep's walker and line searcher (ADR 0007).
- Miller-columns browser layout: parent | current | preview (ADR 0004).
- Git-aware gutter in the browser showing per-file status (ADR 0004).
- Runtime theme palette with user config, per-extension Nerd Font icons and
  tints (ADR 0003).
- Time-based animation engine: folder fade-in, directional folder slide, and
  full-view open/close zoom (ADR 0006).
- Animated GIF playback in preview and full view, plus an animated raster
  spinner (ADR 0004/0005).
- Mouse support: clickable file rows, clickable breadcrumb, wheel scroll.
- Relative-mtime column in the browser.
- Documented remote filesystems via mounts (S3/GCS) in the README.

### Changed
- Rounded borders, soft-tint selection, and active-pane accent styling.

## [0.1.0] - 2026-06-21

- Initial release: a fast terminal viewer for files that are awkward in a
  browser — markdown, spreadsheets, PDF, images, video, docx, pptx, Keynote,
  archives, and binary.
