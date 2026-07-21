# 0015 — pdfium for PDF rendering (with a poppler fallback)

Status: accepted
Date: 2026-07-21

## Context

The PDF viewer rasterised each page by shelling out to poppler's `pdftocairo`
(ADR 0001; `pdf.rs` carried an explicit "no native linking" note). That path is
slow in a way tuning cannot fix, and it showed badly on real documents — a
scanned iOS-exported PDF in the user's `~/UG` archive took **~4.5 seconds per
page** to open.

Measured breakdown (on that file, `pdftocairo -scale-to-x 1600`):

- Each page is a **fresh subprocess** that dynamically links libpoppler + cairo +
  freetype + fontconfig and **re-parses the whole document** — there is no shared
  state between pages, confirmed by a flat ~50 ms floor even on a trivial PDF with
  no warm-cache effect across spawns.
- The page is a full-page **JPEG scan** (1655×2537). cairo resamples it in
  **scalar software**; cost scales with output pixels (0.6 s → 4.6 s as width goes
  400 → 1600 px). This dominates.
- We then **PNG-encode → write /tmp → re-decode** the multi-megapixel raster just
  to move it between two processes.

macOS Preview opens the same file instantly because PDFKit/CoreGraphics parses
once in-process, renders on the GPU with a warm system font cache, and never
serialises the bitmap.

A benchmark of `pdfium-render` (bindings to PDFium, the engine inside Chrome) on
the same files:

| | pdftocairo (old) | pdfium |
|---|---|---|
| scanned page, @1600px | 3.8–4.6 s | **30–50 ms** |
| text page, @1600px | ~0.18 s | ~15 ms (then ~2 ms/page) |
| full-document parse | re-done every spawn | **0.2 ms, once** |

That is ~90–140× on the case that hurt, at full fidelity (annotations,
highlights, fonts all correct — it is Chrome's renderer).

## Decision

Render PDFs with **pdfium when its shared library is available, poppler as the
fallback.**

- **Runtime loading, never linked.** No prebuilt *static* pdfium exists
  (bblanchon ships dynamic only; building from source needs depot_tools + gn, a
  multi-GB, non-reproducible build). So `libpdfium` is loaded at runtime via
  `libloading` through `pdfium-render`. `make` fetches a **pinned, checksum-
  verified** release (`chromium/7961`) into `vendor/` and copies it beside the
  binary (`make install` → next to `sucher`; `make build`/`run` → into `target/`).
  Resolution order at runtime: `$SUCHER_PDFIUM_LIB` → next to the executable →
  system lib dirs.
- **One service thread.** pdfium's bindings/document handles are `!Send` and its
  library must be initialised exactly once per process, so all rendering runs on a
  single dedicated thread that owns the `Pdfium` instance and caches the
  most-recently-opened `PdfDocument`. `pdf.rs` and the browser preview submit
  `(path, page, width)` jobs and get back an in-memory `DynamicImage` — no PNG, no
  /tmp, no subprocess.
- **Fallback is structural, not conditional cruft.** `pdf::render` tries pdfium
  (when `pdfium::available()`) and falls through to the existing `render_page`
  (poppler) on *any* failure — library absent, or a specific document pdfium
  rejects that poppler can still draw. A `cargo install` without the `make` step,
  or a platform we don't ship the lib for, simply keeps the old behaviour.

This reverses the "no native linking" stance of ADR 0001 for PDFs — softened to
"runtime dynamic load with graceful fallback", so the build stays simple and no
platform loses the feature outright.

## Consequences

- Opening a scanned PDF drops from seconds to tens of milliseconds; the async +
  prefetch machinery (added alongside) now almost always hits warm cache. The
  browser preview poster benefits identically.
- New dependency `pdfium-render`, plus a ~7 MB `libpdfium` shipped beside the
  binary. Supply chain is pinned (release tag + SHA-256 for the primary target)
  and auditable by cargo-deny; the lib is gitignored, fetched by `make`.
- Poppler remains a required-ish runtime fallback (and still powers the
  non-interactive `pdftotext`/`pdfinfo` paths), so `pdftocairo` is not removed.
- Cross-platform checksums beyond mac-arm64 are not yet pinned; the Makefile
  verifies where a checksum is known and otherwise trusts the pinned tag over TLS
  with a warning — a follow-up can fill the rest in.
