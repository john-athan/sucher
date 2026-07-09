# 0011 — The theme palette reaches every viewer

Status: proposed (decided; implementation is follow-up work)
Date: 2026-07-09

## Context

ADR 0003 introduced a runtime theme (`theme::palette()`) and a user config so the
UI is re-skinnable (`--theme light`, custom palettes). An architecture review
found the palette is only half-wired: it re-skins the browser chrome
(`dir.rs`, `format.rs` kind colours) and `text.rs`'s highlight tokens, but the
content viewers paint **hardcoded** `Color::Rgb(...)` literals:

- `markdown.rs:13-18` — `SKY`/`AMBER`/`MINT`/`LINK`/`GRAY`/`WHITE` consts drive the
  flagship markdown/docx/pptx/html rendering.
- `tui.rs` status/help/link colours (`:443`, `:468`, `:487`, `:507`, `:523`).
- `sheet.rs`, `pdf.rs`, `video.rs`, `imgview.rs`, `svg.rs` status lines.

Two problems follow. First, `--theme light` produces wrong-looking output in the
very viewers users spend the most time in — fixed dark-theme RGB on a light
terminal. Second, there is literal **drift**: the hardcoded "dim" grey
`Rgb(140,140,150)` in `tui.rs` does not even equal the palette's `dim`
`Rgb(120,120,132)` (`theme.rs:95`), so the same semantic colour has two values.

## Decision

Route every viewer's colours through `theme::palette()`; delete the hardcoded
`Color::Rgb` literals from the viewer modules.

- Where a viewer needs a hue the palette already names (accent, dim, doc/yellow,
  the per-`Format` kind colours), read it from the palette.
- Where the markdown renderer needs hues the palette lacks (heading, emphasis,
  link, code, blockquote), **extend the palette** with those semantic fields
  (defaulted in both the dark and light built-in palettes and overridable in
  config, exactly like the existing fields) rather than hardcoding — so a theme
  can restyle prose, and ADR 0003's "runtime theme" promise finally holds
  end-to-end.
- The palette is a process-global read after startup, so viewers on worker
  threads (the async poster path) may read it too; no plumbing needed.

This is naturally sequenced *after* ADR 0010: once viewers share the scroll
driver, the common status-line rendering reads the palette in one place, and only
the genuinely viewer-specific colours remain to convert.

## Consequences

- `--theme light` and custom palettes work in every viewer, not just the browser
  — the feature ADR 0003 documented is actually delivered.
- One definition per semantic colour; the `dim` drift disappears.
- The palette grows a few prose-oriented fields; the built-in dark palette keeps
  today's exact RGB so the default look is byte-for-byte unchanged.
- Snapshot/visual care needed during conversion: the default-theme output must
  not shift, so each converted literal must map to a palette field whose default
  equals the old constant.
