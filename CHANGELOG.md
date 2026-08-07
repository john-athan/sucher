# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses semantic
versioning while pre-1.0 (breaking changes may land in minor releases).

## [Unreleased]

## [0.6.0] - 2026-08-07

### Added
- **File operations in the directory browser.** sucher still changes only *where
  files are*, never *what they contain* (no editing, no archive extraction, no
  writes to a spreadsheet or database), but the browser can now do the shell
  around files. `Space` marks and steps down, `V` inverts the view, `Ctrl-a`
  marks all; the mark set is **global**, surviving navigation, so a selection can
  be gathered across several folders and acted on once. With nothing marked every
  verb falls back to the entry under the cursor. `y`/`X` copy/cut to a clipboard
  that also survives navigation, `p` pastes into the current directory, `r`
  renames (opening pre-filled with the cursor between the name and the
  extension, so editing lands on the name and never on the `.txt`), `a` creates
  (a trailing `/` makes a folder), `D` moves to the trash, `U` undoes, and `Y`
  yanks absolute paths via **OSC 52** so it works over ssh with no helper binary.

  Every batch **shows its plan before anything happens**: item and byte totals,
  each destination, which names took a ` (2)` suffix to dodge a collision, and
  which marked paths vanished in the meantime. `Enter` authorises, `Esc` cancels,
  `o` re-plans under overwrite and shows the count it would replace in red.
  Collisions are suffixed by default; overwriting is never automatic.

  **Nothing is ever permanently deleted.** There is no permanent-delete binding,
  and the rule is total rather than a property of one key: an overwrite trashes
  what it displaces before writing, a cross-device move trashes the original once
  the copy lands, and undoing a copy trashes what sucher created. Where no trash
  exists the operation fails honestly instead of falling back to `rm`.

  Operations run on a background thread with live progress and can be cancelled;
  `Esc` backs out one layer per press (cancel the run, then the clipboard, then
  the marks, then quit) and `q` is refused while a run is in flight, so sucher
  cannot exit with a mutation half done and unreported. Failures and
  informational notes are shown in a report rather than reduced to a status line.
  One operation at a time; a batch is capped at 50,000 entries and 64 levels and
  is refused whole rather than copied halfway. Symlinks are copied as links and
  never followed.

  New modules: `marks.rs` (pure global mark set), `fileop/` (`collect` bounded
  walk, a **pure** `plan`, and an `execute` that replays a decided list and also
  runs undo), and `lineedit.rs` (single-line buffer with a UTF-8-safe cursor).
  Adds the `trash` dependency and `tempfile` as the project's first
  dev-dependency. See ADR 0017.

## [0.5.0] - 2026-07-21

### Added
- **Data files open in the grid, with an interactive SQL prompt.** Parquet
  (`.parquet` `.pq`), newline-delimited JSON (`.jsonl` `.ndjson`), SQLite
  (`.sqlite` `.sqlite3` `.db` `.db3`), and DuckDB (`.duckdb` `.ddb`) now render
  in the existing spreadsheet grid, backed by a new `DataBook` with two native,
  fully-static engines behind one interface: **DuckDB** (statically bundled from
  vendored source) reads Parquet/JSONL/DuckDB, and **rusqlite**'s bundled
  libsqlite reads SQLite — each format read by the engine that owns it, both
  offline. Databases are opened **read-only** and each table becomes a sheet
  (switch with `Tab`); Parquet/JSONL are a single sheet named for the file stem.
  Columns keep their real names, and dates/timestamps render as ISO text with
  NULL shown blank — no serial-number date wart. Press `:` in the grid for a live
  **SQL prompt** over the current file: the result replaces the view (schema,
  rows, and `/` search follow it), you can `FROM <stem>` a single-file source or
  `FROM <table>`/join across a database's tables, a parse/bind error keeps your
  text and the previous view intact, and empty input reverts to the base table.
  Reads are **lazy and uncapped** — the grid windows rows on demand (`LIMIT`/
  `OFFSET` + prefetch) and takes the schema from `DESCRIBE` without executing, so
  a file opens instantly regardless of size and scrolls to the end with no row
  cap (unlike the streaming `.xlsx`/CSV backends). It is **fully offline**: both
  engines are statically compiled in and every DuckDB connection disables
  extension autoinstall/autoload, so reading a data file never touches the
  network. Behind the **default-on `data` Cargo
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
