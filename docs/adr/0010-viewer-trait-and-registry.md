# 0010 — A `Viewer` trait: one dispatch registry + one interactive loop

Status: proposed (decided; implementation is follow-up work)
Date: 2026-07-09

## Context

ADR 0001 established a single classification registry (`format.rs`) so "which
kind is this file" has one source of truth. That principle is honoured for
*classification* but quietly violated for everything downstream of it:

1. **Viewer dispatch is triplicated.** Three separate `match Format` sites route
   a kind to its viewer: `main.rs::run` (TTY-vs-pipe, ~L117), `main.rs::open_interactive`
   (~L366, the browser's open-and-return), and `dir.rs::build_preview` (~L1690, the
   preview pane). Adding a format means editing all three, and they already differ
   subtly (e.g. SVG dumps via `text::dump` in one arm, has bespoke logic in
   another). This is exactly the divergence ADR 0001 set out to kill, one layer up.

2. **Nine viewers hand-roll the same interactive loop.** `text.rs`, `hex.rs`,
   `svg.rs`, `sheet.rs`, `pdf.rs`, `video.rs`, `archive.rs`, `imgview.rs`, `tui.rs`
   each own a `ratatui::init()` → poll/`dirty` loop → `ratatui::restore()`, and 8
   of them repeat the identical `j/k/d/u/g/G` scroll keys, a `max_offset()`, and a
   body+status layout. `text.rs` and `hex.rs`'s loops are byte-for-byte identical;
   `max_offset` is redefined with the same body in four modules. A change to poll
   cadence, resize handling, or the scroll key set must be made in ~9 places and
   will drift. The loops are also untested (only the pure helpers are).

3. **The CONTRIBUTING "every format exposes `run` + `dump`" contract is not real.**
   `keynote`/`imgview`/`svg` have `run` but no `dump` (piped output is inlined in
   `main.rs`); `docx`/`pptx`/`html` expose `to_markdown` but no `run` (they borrow
   `tui::run`). Nothing enforces the contract, so each `main.rs` arm is bespoke.

## Decision

Introduce a `Viewer` abstraction that owns dispatch and the interactive loop, so
a format is registered once and both the classifier's spirit (ADR 0001) and the
module contract hold by construction.

Two cooperating pieces:

### a) A dispatch registry keyed by `Format`

Give `Format` (or a sibling `viewer` module) two methods that replace the three
match sites:

```rust
/// Open interactively; returns when the user quits back to the caller.
fn open(self, title: &str, path: &str) -> io::Result<()>;
/// Non-interactive textual dump for pipe mode.
fn dump(self, path: &str) -> String;
```

`main.rs::run` becomes "classify → TTY? `open` : print `dump`"; `open_interactive`
becomes one `open` call; `build_preview` keeps its *preview* specialisation (a
preview is not a full open) but sources the "how do I render this kind" decision
from the same table rather than a parallel match. Formats that reduce to markdown
(docx/pptx/html/md) route through the markdown viewer inside their `open`/`dump`,
so the reduction lives in the module, not in `main.rs`.

### b) A shared interactive scroll driver

Most viewers are "a scrollable body + a status line + the standard motion keys."
Model that once:

```rust
trait ScrollView {
    fn content_len(&self) -> usize;         // rows, for scroll clamping
    fn render_body(&mut self, f: &mut Frame, area: Rect, offset: usize);
    fn status(&self) -> Line<'_>;
    fn on_key(&mut self, key: KeyEvent) -> Flow { Flow::Pass } // extra, viewer-specific keys
}
fn run_scroll_view(view: impl ScrollView) -> io::Result<()>;  // owns init/restore, poll/dirty, resize, j/k/d/u/g/G, quit
```

`run_scroll_view` owns terminal lifecycle, the adaptive poll/`dirty` loop, resize,
and the standard scroll + quit keys (the single home for the `visible_window`
virtualization from the perf work). `text`/`hex`/`archive`/`svg-source` become
thin `ScrollView` impls; the richer viewers (`sheet` grid, `pdf`/`video`/`imgview`
graphics, `tui` markdown with TOC/links) either implement it with extra `on_key`
handling or keep a bespoke loop where they genuinely differ — the driver is for
the ones that are the same, not a Procrustean bed for the ones that aren't.

The `Viewer::open`/`dump` contract is then enforceable: the compiler requires both
for every registered format, so CONTRIBUTING describes something real (finding 3
resolved by construction, not by editing prose).

## Consequences

- One dispatch table: adding a format touches one place, and preview/open/pipe
  can never route it three different ways. Restores ADR 0001's principle to the
  viewer layer.
- ~700 lines of duplicated event-loop/scroll code collapse to one tested driver;
  poll cadence, resize, and scroll semantics change in one place.
- The interactive loop becomes testable via the trait (drive `on_key`, assert
  offset/selection) — today it is untested.
- Migration is incremental: introduce the registry first (mechanical, low-risk),
  then move viewers onto the driver one at a time behind it. No big-bang rewrite.
- Risk: over-abstracting the divergent viewers. Mitigated by keeping `on_key`/
  bespoke-loop escape hatches — a viewer that is genuinely different stays
  different rather than being forced through the trait.
