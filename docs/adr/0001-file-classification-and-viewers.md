# ADR 0001 — Single file-classification registry & viewer routing

Status: **Accepted — 2026-07-04**

## Context

Sucher classified every file **twice**, with two hand-maintained extension tables
that disagreed:

- `main.rs::kind_of` drove *which viewer opens* (Markdown / Sheet / Image / Pdf /
  Video / Docx), with **everything unrecognized falling through to Markdown**.
- `dir.rs::classify` drove the browser's *colour / glyph / label / preview routing*
  (a richer taxonomy: Dir / Markdown / Sheet / Image / Pdf / Video / Doc / Code /
  Archive / Audio / Other).

Because the directory browser opens the selection through `kind_of` while colouring
it through `classify`, the label and the behaviour diverged — visibly wrong for real
files:

| File | Browser showed | Enter actually did |
|---|---|---|
| `.csv` / `.tsv` | green "Spreadsheet" | opened as **Markdown** |
| `.svg` | "Image", tried to raster | opened as **Markdown** |
| `.doc` / `.rtf` / `.pptx` | "Document" | `read_to_string` on binary → **Markdown garbage** |
| `.rs` / `.py` / any code | "Source" (coloured) | opened as **Markdown**, mangled |

The root causes are (1) two sources of truth for one fact, and (2) a
"default-to-Markdown" fallback that renders arbitrary bytes through the CommonMark
parser, corrupting anything that isn't Markdown.

This mirrors a lesson carried over from the sibling project *Sucher*, whose
`preview.rs` routes every file type through one `Previewer` registry ("add a type =
add one previewer, no central change").

## Decision

**D1 — One registry.** A single `format.rs` module owns classification. One
`Format` enum is rich enough for both jobs; each variant answers both *"which viewer
opens me"* and *"how does the browser present me"* (colour / glyph / label). The two
old tables are deleted. Adding a file type touches exactly one place.

```
Format = Directory | Markdown | Text | Sheet | Image | Pdf | Video
       | Docx | Pptx | Keynote | Doc | Audio | Archive | Binary
```

Classification is a **pure** function `classify(ext, is_dir, head: Option<&[u8]>)`
(extension first; a byte `head` disambiguates only extension-less / unknown files),
with a thin `classify_path` IO wrapper that reads the head when needed. Pure core is
unit-tested without the filesystem, matching the discipline used elsewhere in the
codebase (markdown layout, xlsx search).

**D2 — Markdown is no longer the default for unrecognized text.** Only
`.md` / `.markdown` / `.mdx` open in the Markdown viewer. All other text — source
code, `.txt`, `.log`, `.conf`, `.json`, `.svg`, and extension-less UTF-8 files — opens
in a new **Text viewer** (`Format::Text`) that renders the bytes *faithfully*, with
lightweight syntax highlighting where the language is known and plain styling
otherwise. Rendering a `.log` or a `.rs` through the Markdown parser corrupted it
(`#` became a heading, `*` italic, indented lines became code blocks); faithful
rendering is Sucher's core promise, so the mangling default is removed.

*Consequence:* the README's "Markdown … and the default for unknown text" claim is
updated. `v somefile` with no extension now shows the file as text, not as
speculative Markdown.

**D3 — `.svg` is its own format (superseded).** Originally SVG was classified as
Text because the `image` crate cannot rasterise vectors. It now has a dedicated
`Format::Svg` with an in-tree rasteriser (resvg/usvg/tiny-skia): the viewer shows the
rendered picture *above* the scrolling XML source, and the browser preview rasterises
a thumbnail. Terminals without a graphics protocol still get the source; piped output
is the raw XML. (Historical note: for a time `.svg` deliberately opened in the Text
viewer — that was this decision's original form.)

**D4 — csv / tsv open in the grid (Sheet) viewer.** Comma/tab-separated values are
tabular data, and Sucher already has a grid viewer — that is their correct home, and
it makes the browser's existing green "Spreadsheet" label honest. A `CsvBook` backend
is added to `sheet.rs`'s `Book` (calamine does not read CSV). See the CSV parsing
notes in that step.

**D5 — Recognized-but-unopenable types degrade gracefully.** The remaining
viewerless types — legacy office binaries `Doc` (`.doc/.rtf/.odt/.ppt`) and `Audio`
— show a concise "no viewer for <kind>" message plus file metadata (size, modified)
rather than feeding binary bytes to a text/Markdown renderer. In the browser these
keep their distinct category colour/label; their preview pane shows metadata.

**D6 — pptx, Keynote, archives, and binary each get a real viewer.** Extending the
same one-registry pattern rather than widening the unopenable set:
`Pptx` (`.pptx`) unzips its slide parts and converts `<a:t>` runs to markdown —
mirroring the `Docx` path — so the markdown TUI renders it. `Keynote` (`.key`) is an
iWork package whose IWA-protobuf body we do not decode; instead we extract its
embedded QuickLook preview image and hand it to the image viewer, an honest visual
without a bespoke parser. `Archive` (`.zip/.tar/.tar.gz/.tgz/.gz`) opens a read-only,
scrolling table of contents (path + size) — Sucher lists, it does not extract; types
with no in-tree decoder (`.7z/.rar/.xz/.bz2/.zst`) report that honestly. `Binary` (any
unrecognized non-text file) opens a scrolling canonical hexdump. Each new viewer is
one module plus one dispatch arm; `opens()` now returns true for these four.

## Consequences

- One extension table, one taxonomy; label and behaviour can no longer drift.
- The Markdown parser only ever sees Markdown.
- New Text viewer becomes a first-class surface (code, configs, logs, svg).
- csv/tsv gain a real grid; svg and office/audio/archive files get honest handling.
- `format.rs`'s `classify` is a pure, unit-tested core; only `classify_path` does IO.
- Follow-on ADRs cover the Text viewer's highlighter, browser typeahead, and the
  smart-query filter.

## Status: implemented 2026-07-04

Landed by:

- `src/format.rs` — new: the single `Format` registry with the pure, unit-tested
  `classify` / `looks_textual` core and the `classify_path` IO wrapper, plus
  `label` / `glyph` / `color` / `opens` methods.
- `src/main.rs` — deleted the old `Format` enum and `kind_of`; `main` and
  `open_interactive` now route through `format::classify_path`. Unopenable files
  print a "no viewer for …" notice with a size/modified metadata line
  (`unsupported`) instead of being fed to a renderer.
- `src/dir.rs` — deleted the `Kind` enum, its `classify` free fn, and the
  `color` / `label` / `glyph` methods; `Entry.kind` is now `Format`, the browser
  classifies by extension only (no per-entry read), and `activate` refuses to
  open a file whose `Format` does not `opens()`.
- `README.md` — Supported-formats table, Text-viewer keys, and How-it-works
  updated for the single registry.
