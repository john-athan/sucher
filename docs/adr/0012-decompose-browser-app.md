# 0012 — Decompose the browser `App` into cohesive components

Status: proposed (decided; implementation is follow-up work)
Date: 2026-07-09

## Context

`dir.rs::App` has grown to ~30 fields and ~2000 lines of methods in a
~3500-line file. It is the browser, but it bundles at least eight independent
concerns into one struct where every method can touch every field, so the
(well-documented) invariants are enforced only by convention:

- **Listing / navigation** — `cwd`, `all`, `view`, `state`, `filter`, `sort`,
  `parent`, `show_hidden`.
- **Preview building** — `preview`, `preview_for`, `pv`, `caption`, and the
  `preview_*` methods.
- **Async raster pipeline** — `raster_tx`/`rx`, `raster_pending`, `raster_want`,
  `img_cache`, `pane`, `preview_animated`, `spin`.
- **Navigation animation** — `fade`, `fade_frames`, `slide`, `animate`.
- **Mouse hit-testing** — `crumb_hits`, `list_area`, `parent_area`, `search_area`.
- **Recursive search** — `search` (already a sub-struct, `SearchState`).
- **Git gutter** — `git`, `git_enabled`.
- **Typeahead** — `typeahead`, `typeahead_at`.

Some seams are already extracted (`SearchState`, `Slide`, `Anim`), which shows the
decomposition is natural — the rest just haven't followed.

## Decision

Extract the remaining concerns into owned components with their own state and
methods, leaving `App` a coordinator that wires them to the event loop:

- **`Preview`** — owns `preview`/`pv`/`caption`/`preview_for` and the `preview_*`
  builders plus the raster channel/cache/pane/spinner (the whole async poster
  pipeline). Exposes `build(sel)`, `pump() -> bool`, `render(f, area)`.
- **`NavAnim`** — owns `fade`/`fade_frames`/`slide`/`animate`; exposes `arm(dir)`,
  `tick(now) -> bool`, `render_into(...)`. The `visible_window` virtualization for
  the slide/snapshot lives here.
- **`MouseHits`** — owns the four `Rect`s + `crumb_hits`; exposes `record(...)`
  during render and `hit(col,row) -> Target` on a click, folding in `row_to_index`
  and `crumb_hit`.

`App` keeps listing/navigation (its actual job) plus the already-extracted
`SearchState`, and delegates the rest. Each extracted component becomes
independently unit-testable (today none of this is, because it is all tangled
into one struct that needs a live terminal).

Ordering: this composes with ADR 0010 — once the shared scroll driver exists, the
browser is one more consumer of it, and `Preview`/`NavAnim`/`MouseHits` are the
browser-specific pieces layered on top. Do ADR 0010 first, then this.

## Consequences

- Invariants become structural: e.g. only `Preview` can touch the raster channel,
  so "the worker never mutates `img_cache`/`pane`" is enforced by ownership rather
  than a comment.
- Each component is testable in isolation.
- `dir.rs` shrinks toward a readable coordinator; the file may split into
  `dir/mod.rs` + `dir/preview.rs` + `dir/anim.rs` + `dir/mouse.rs`.
- Pure mechanical risk (large move-refactor). Mitigated by doing it component by
  component, each behind the full existing test suite, with no behaviour change.
- This is the lowest-urgency of the arch ADRs: the code works and is
  well-commented; the win is testability and navigability, not correctness.
