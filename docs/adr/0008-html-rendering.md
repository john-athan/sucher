# ADR 0008 — HTML rendering via markdown reduction

Status: **Accepted — 2026-07-09**

## Context

`.html` / `.htm` files fell through `classify` to `Format::Text` (they have a
highlight syntax, so `is_text_ext` is true) and opened in the source viewer:
raw tags, syntax-coloured. Useful for reading markup, useless for reading the
*document* the markup describes. Sucher's remit is "files that are awkward in a
browser"; an HTML file is the one thing a browser already renders well, but from
a terminal you still want the rendered text, not the angle brackets.

Two prior decisions constrain the shape of any fix:

- **Faithful rendering is the core promise (ADR 0001 D2).** Never feed bytes to a
  parser that will corrupt them. Real-world HTML is *not* well-formed XML —
  unclosed `<br>`/`<img>`/`<p>`, unquoted attributes, stray entities — so the
  `quick-xml` reader that serves docx/pptx would silently mangle it.
- **Reduce complex documents to the existing markdown model (ADR 0001 D6).** docx
  and pptx do not get bespoke UI; each converts to a markdown string and reuses
  `markdown.rs` layout + `tui.rs` (scroll, search, TOC, link-picker) + `plain.rs`
  (piped). HTML is a structured text document of the same species.

## Decision

**D1 — HTML is its own format that reduces to markdown.** Add `Format::Html`
(`.html` / `.htm` / `.xhtml`). A new `html.rs` exposes
`to_markdown(path) -> Result<String, String>` — exactly the docx/pptx contract —
and wires through the existing `render_markdown` path. No new viewer, no new UI.
The `Format::Html` arm in `run`, `open_interactive`, and the browser preview all
mirror the `Docx` arms.

**D2 — Parse with a real HTML5 parser, not the XML reader.** The core promise
forbids routing tag soup through `quick-xml`. `html.rs` uses `html5ever` +
`markup5ever_rcdom` — the same tokeniser/tree-builder browsers are specified
against — which recovers from malformed markup the way a browser does. This adds
one capability-scoped dependency, matching the codebase's "one small crate per
capability" habit (resvg for SVG, calamine for sheets, poppler for PDF).

*Rejected alternatives:*

- **Reuse `quick-xml` (zero new deps).** Tempting and mirrors docx, but it is an
  XML reader; on messy HTML it drops or misreads content. A fragility hybrid that
  violates D1's faithful-rendering promise. Rejected.
- **`html2text` (HTML → terminal text directly).** Easiest, but it renders to
  styled text itself, bypassing `markdown.rs`. That is a second, parallel
  rendering path — colours, wrapping, link handling all diverging from the
  markdown viewer. Violates the one-model discipline of ADR 0001 D6. Rejected.

**D3 — The reducer walks the DOM and emits the shared markdown vocabulary.** Pure
`fn parse(html: &str) -> String`, unit-tested with inline HTML literals exactly
like `docx::parse`. It emits the same markdown the rest of the app already
renders: `#`..`######` headings, `**bold**` / `*italic*`, `` `code` `` and fenced
blocks, `[text](href)` links, `-`/`N.` lists (nested by indent), `>` blockquotes,
`---` rules, and pipe tables. Non-content subtrees (`<script>`, `<style>`,
`<head>`, `<noscript>`, `<template>`) are dropped — honest degradation, never
garbage.

*Known simplifications (altitude matches docx, not a browser):* no CSS, no
JavaScript, no absolute-vs-relative link resolution, no image fetching (`<img>`
becomes `![alt](src)` for the link-picker). Whitespace is collapsed per the HTML
model; emphasis whitespace is hoisted outside the markers so `**word** rest`
stays valid markdown.

## Consequences

- `.html` shows the *rendered document* in the TUI and browser preview; piped
  output is plain text via `plain::render`, for free.
- One new module + one dependency + the enumerable per-format match arms
  (`format.rs` variant/classify/label/glyph/color/opens, `main.rs` × 2,
  `dir.rs` preview) — the ADR 0001 one-registry checklist, nothing more.
- `markdown.rs` / `tui.rs` / `plain.rs` are reused unchanged; no new UI surface.
- The markdown parser still only ever sees markdown; the HTML5 parser only ever
  sees HTML.

## Status: implemented 2026-07-09

Landed by:

- `src/html.rs` — new: the pure `parse` DOM-walk reducer + `to_markdown` IO
  wrapper, with inline-HTML unit tests.
- `src/format.rs` — `Format::Html` variant; `classify` arm for `html`/`htm`/
  `xhtml`; `label`/`glyph`/`color`/`opens` arms; classify test.
- `src/main.rs` — `mod html`; `Format::Html` arms in `run` and
  `open_interactive` mirroring `Docx`.
- `src/dir.rs` — `Format::Html` browser-preview arm mirroring `Docx`.
- `Cargo.toml` — `html5ever` + `markup5ever_rcdom`.
- `README.md` — supported-formats table.
