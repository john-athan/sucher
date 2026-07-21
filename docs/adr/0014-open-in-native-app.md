# 0014 — "Open in native app" for the file you are viewing

Status: accepted
Date: 2026-07-21

## Context

Sucher renders many formats in the terminal, but the terminal view is a
*reduction*: a PDF is rastered page by page, a docx is flattened to markdown, a
Keynote is a single extracted preview JPEG, a spreadsheet loses formulas and
styling. Users regularly want to bail out to the real application — open the
`.pdf` in Preview, the `.docx` in Word, the folder in Finder — without leaving
sucher to type `open file` at a shell.

We already spawn the OS opener in one place: `tui::open_url` hands *link targets
embedded in a rendered document* to `open`/`xdg-open`/`rundll32`. That path is
gated by `util::is_safe_url` (ADR 0009 / S5): the URL is **untrusted** — it came
from inside a document sucher parsed — so only `http`/`https`/`mailto` schemes
are allowed and a `file://`/`javascript:`/`-`-leading target is refused. Reusing
that gate for "open the current file" would be wrong on both ends: it rejects
every local path, and its threat model does not apply.

## Decision

Add `util::open_in_native_app(path)` — a second, deliberately separate entry to
the OS opener — and bind it to `x` in the directory browser and in every
fullscreen viewer (pdf, image/keynote, video, svg, text, sheet, hex, archive,
docx/pptx/epub/ipynb/markdown/html).

Two boundaries, kept distinct:

- **`is_safe_url` (untrusted link target).** Scheme allow-list. Unchanged.
- **`open_in_native_app` (trusted local file).** The path is one the user
  themselves selected or opened in sucher — not data extracted from a document.
  "Open externally" *means* "hand this file to its default handler", so no scheme
  allow-list applies. The only guard retained is `cmd_path_arg`, so a `-`-leading
  filename can never be misread as an option flag (ADR 0009 / S4). Spawned
  detached (never waited on), so returning to sucher is instant.

The source file, not the rendered form, is what `x` opens: a viewer showing a
*derived* artifact carries the original path separately (docx → its `.docx`, not
the intermediate markdown; Keynote → the `.key` package, not the preview JPEG).
Viewers rendered from stdin have no source file and simply omit the binding.

`x` was chosen because it is the one character free in the browser's binding
table (`browse_char`) **and** unused in every viewer loop — so the key is
identical everywhere, which is the point of a "works no matter where you are"
action.

## Consequences

- One trusted-path opener, one untrusted-URL gate; the two threat models never
  share code and cannot be confused.
- Every format — including ones sucher deliberately renders poorly or not at all
  (legacy `.doc`, audio, a folder) — has a first-class escape hatch to the real
  app. `x` on an entry the browser has "no viewer" for still opens it natively.
- Adding a viewer means threading its source path into its loop and matching `x`;
  the help/status lines advertise it, so discoverability stays uniform.
