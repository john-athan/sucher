# Changelog

All notable changes to this project are documented here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/); this project uses semantic
versioning while pre-1.0 (breaking changes may land in minor releases).

## [Unreleased]

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
