# ADR 0004 — Miller columns, git signals, and motion in the browser

Status: **Accepted — 2026-07-09**

## Context

Three browser upgrades (roadmap items 4–6) share the same render path and must
compose without turning `render()` into a tangle:

- **Git-aware gutter** — show each entry's git state (modified / staged /
  untracked / deleted) inline, like `lf`/ranger plugins.
- **Miller columns** — the ranger signature: `parent | current | preview`, so
  you see where you came from and where you'd go in one glance.
- **Motion** — make async work feel alive (a spinner while a poster rasters)
  without the jank of faked transitions.

## Decision

**D1 — Miller layout via ONE reusable pane renderer; parent + current share it.**
`render_list` becomes `render_entry_list(f, area, &EntryListView, active: bool)`
where `EntryListView` bundles the entries/indices, selection, icon mode, and an
optional git map. `render()` composes columns:

- **Three-column** (`parent | current | preview`) when the layout is Miller *and*
  the frame is wide enough (`width >= MILLER_MIN`, ~100 cols) *and* a parent
  exists. Suggested split ~ `[20%, 34%, 46%]`.
- **Two-column** (`current | preview`, the existing `[42%, 58%]`) otherwise.

The parent pane lists `cwd.parent()`'s entries with the current directory
highlighted; it is **navigation context only** — no git gutter, no live preview,
not focused. The current pane is the active pane (accent border, per ADR 0003).
Building the parent list reuses `load`-style reading factored into a pure
`read_entries(dir) -> Vec<Entry>` so both panes and the folder-preview share one
lister.

*Layout source:* config `layout = "auto" | "miller" | "double"` (default `auto`
= Miller when wide, double when narrow), plus a runtime toggle key **`M`** that
cycles the effective mode. `M` joins `browse_char` (ADR 0002 D2) so typeahead
stays correct. *Rejected — always three columns:* wastes width on narrow
terminals and buries the preview; auto-collapse is friendlier.

**D2 — Git status by subprocess, not `libgit2`.** A new `git.rs` shells out once
per directory load:

- `git -C <dir> rev-parse --show-toplevel --show-prefix` → repo root + the dir's
  root-relative prefix; non-zero exit ⇒ not a repo ⇒ no gutter (graceful).
- `git -C <dir> status --porcelain=v1 -z --untracked-files=normal --ignored=no`
  → NUL-separated, root-relative `XY path` records.

A **pure** `resolve(records, prefix, names) -> HashMap<String, GitStatus>` maps
each visible entry: an exact `prefix+name` match takes that record's status; a
directory entry with any record under `prefix+name/` is marked *modified*
(aggregated), and an untracked dir (porcelain `prefix+name/`) is *untracked*. The
XY→`GitStatus` mapping (Untracked, Added, Modified, Deleted, Renamed, Conflict)
and the aggregation are unit-tested without a repo; only the two `git` calls do
IO. Each status has one glyph + one palette colour, rendered as a slim gutter
column between the icon and the name. Clean files get no marker. Recomputed on
every `load()` (cheap, correct after directory changes); a stale gutter after an
in-place edit is acceptable and refreshes on the next navigation.

*Rejected — `git2`/libgit2 dependency:* heavy native build for what two piped
git commands do; the subprocess path also inherits the user's exact git
semantics (ignores, submodules) for free. *Config:* `git = true|false`
(default `true`); when git isn't installed or the dir isn't a repo, the gutter
silently absents.

**D3 — Motion is real work made visible, never a faked transition.** Cell
terminals can't alpha-blend, so crossfades/slides read as jank; we don't do them.
What we do:

- An **animated braille spinner** (`⠋⠙⠹…`) replaces the static `rendering…` while
  a poster rasters, advanced by a frame counter on each redraw. The event loop
  already tightens to a 60 ms poll while a raster is pending (`main_loop`), so the
  spinner ticks smoothly with no busy-loop when idle.
- The counter advances only while an animation is live; a fully idle browser
  still blocks at the 1 s poll (no idle CPU, preserving the README's promise).

*Rejected — eased preview crossfades / directory slide animations:* not
achievable cleanly in a character grid; instant swaps are snappier and honest.

## Consequences

- `render_entry_list` is the single place entries are drawn — git gutter, icons,
  selection, and the parent/current distinction all live there once. Adding a
  column later touches one function.
- `git.rs` and the Miller/parent lister keep the pure-core / thin-IO split
  (ADR 0001 ethos): status mapping, aggregation, and layout choice are tested
  without a terminal, a repo, or a clock.
- New config keys `layout` and `git`; a new `M` binding. Two assumptions: `git`
  on `PATH` (absence → no gutter) and that per-load subprocess latency is
  negligible for interactive dirs (it is; git status on a working tree is ms).
