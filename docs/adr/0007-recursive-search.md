# ADR 0007 — Recursive, streaming, content-aware search

Status: **Accepted — 2026-07-09**

## Context

Sucher's directory browser has a local filter (`/`, ADR 0002 typeahead is its
sibling): a smart query (`report kind:pdf size:>1mb modified:<7d ext:rs`) parsed
by `query.rs` and applied in-memory to the **current directory's** listing. It is
instant, allocation-cheap, and never touches the filesystem beyond the one
`read_dir` that already happened.

What it cannot do is answer *"where is this, anywhere below here?"* — the thing
every modern finder (fd, ripgrep, fzf, broot, telescope) exists to do. The user
asked for search that is **recursive, streaming, and content-aware**, and
"more modern and performant than all others." This ADR records how that fits
Sucher without diluting either the viewer identity or the existing filter.

The moat is already in the building: Sucher's right pane renders the *actual
file* (PDF page, image pixels, spreadsheet grid, typeset markdown). A finder whose
results render as the real framed file — not a grey line of text — is something
fd/rg/fzf structurally cannot be. Recursive search is the feature that points that
pane at the whole tree.

## Decision

### D1 — Search is a distinct mode from the local filter, not a merge

`/` (filter) and recursive search are **two different operations**, and keeping
them separate is the non-hybrid choice:

| | Local filter (`/`) | Recursive search |
|---|---|---|
| Scope | current directory's listing | the tree from cwd downward |
| Cost | in-memory, zero IO | streamed background tree walk |
| Result | the same rows, narrowed (order, columns, git gutter preserved) | path-bearing hits across depths, optional content snippet |
| Latency | instant | first hits in ms, streams to completion |

A filter that *heuristically* escalates to a recursive walk (deciding for the user
when "here" should silently become "everywhere") is exactly the hybrid this
project rejects — two behaviours behind one surface, neither predictable. So the
filter stays byte-for-byte as-is, and search gets its **own key (`S`)** and its
own mode. They are not redundant: one narrows what is in front of you, the other
finds what is not.

**What is shared** is deliberate and load-bearing, not duplicated:

- **`query.rs`** — the single predicate parser (`kind:/ext:/size:/modified:`)
  serves both. Adding a search-only parser would fork the one source of truth the
  codebase is built around (cf. ADR 0001's single classifier).
- **The preview pane** (`build_preview`) — a search hit renders through the exact
  same pipeline as a browsed entry. This is the differentiator; it must not fork.
- **The open path** (`activate` / `Action::Open`) — opening a hit is opening a
  file, unchanged. Because `App` state survives the open-and-return round trip
  (`dir::run`'s outer loop), opening a hit and quitting the viewer lands back in
  the live search results for free.

### D2 — One query parser, extended with `content:`

`query.rs` gains a `content: Option<String>` predicate. `Query::matches(name,
format, size, modified)` stays **metadata-only and pure** (no IO) — it is the
cheap pre-filter the walker applies to every entry, and it is still exactly what
the local filter calls. Content matching is a *separate* step the walker performs
only when a `content:` term is present **and** the cheap predicates already
passed, so a metadata-only query never opens a file. One parser, one tested pure
core, IO strictly at the edge — the same discipline as `format.rs`
(`classify` pure, `classify_path` the thin IO wrapper).

### D3 — Streaming background walk via `ignore` (ripgrep's walker)

The walk uses the [`ignore`](https://crates.io/crates/ignore) crate — ripgrep's
own parallel, `.gitignore`-aware, hidden-file-aware directory walker. This is the
"performant than all others" claim made concrete: we run the same engine ripgrep
does rather than hand-rolling a `walkdir` loop.

- The walk runs on a **background thread**; matching hits stream over an `mpsc`
  channel to the main loop, which appends and redraws them **as they arrive** —
  reusing the proven raster worker pattern (ADR 0005 D1): the UI never blocks on
  the walk, and the first hits appear in milliseconds on a big tree.
- **Cancellation** is a shared `Arc<AtomicBool>` the walk checks per entry. A new
  keystroke in the query, or leaving search, cancels the in-flight walk before the
  next one starts — no zombie walkers, no stale hits from an old query.
- Honours the browser's **hidden toggle** (`.` — off by default) and
  `.gitignore` by default; both map onto `ignore::WalkBuilder` options.
- A **result cap** bounds pathological trees, mirroring the xlsx row cap; hitting
  it is surfaced in the status line, never silently truncated.

### D4 — Content matching via `grep-searcher` + `grep-regex` (ripgrep core)

`content:foo` scans file bytes with [`grep-searcher`](https://crates.io/crates/grep-searcher)
driven by a [`grep-regex`](https://crates.io/crates/grep-regex) matcher — again,
ripgrep's own line searcher, which brings binary detection, mmap-vs-stream
selection, and correct line handling for free. Matching is **smart-case**
(case-insensitive unless the pattern contains an uppercase letter), literal by
default. Each content hit carries its **first matching line + line number** for
the result snippet. Reimplementing this to avoid the dependency would be slower
and less correct on exactly the large/odd files Sucher targets.

### D5 — Results reuse the preview pane; result rows are drawn specialised

The right pane **fully reuses** `build_preview` through a mode-aware `selected()`
accessor (search active → hit under cursor; else the browsed entry). That single
branch is the whole integration cost of the moat.

The left result rows are drawn **specialised**, not through the shared
`EntryListView`: a hit shows its path **relative to cwd** and, for a content
match, a dimmed `line N: …matched text…` snippet — data a flat single-directory
listing doesn't have. Contorting the shared renderer to carry relative paths and
snippets would be the wrong reuse. The rule: reuse where it is the differentiator
(the preview), specialise where the data genuinely differs (the rows).

### D6 — Interactive only; not a CLI grep

Recursive search is a **browser** feature. A non-TTY `s <dir>` still emits the
plain listing dump (unchanged). Sucher does not grow a `--find` that competes with
`fd`/`rg` on the pipe — it is a viewer and browser, and you already have those
tools for scripting. This is a deliberate scope line, not deferred work: the
value here is *find → see it framed*, which only exists inside the TUI.

## Consequences

- New dependencies: `ignore`, `grep-searcher`, `grep-regex` (all BurntSushi /
  ripgrep crates, already transitively battle-tested). Weighed against the project
  already pulling `resvg`, `calamine`, `image`, `zip`, this is proportionate and
  buys the performance headline.
- `query.rs` grows one predicate and stays pure; the local filter is untouched.
- `dir.rs` grows a `Mode::Search` arm and an `Option<SearchState>` on `App`; the
  browse and filter paths are unchanged.
- New `search.rs` owns the walker + content matcher behind a channel, testable
  against a temp-tree fixture in isolation from the TUI.
- The preview pane and open path are reused verbatim — the feature is mostly
  wiring an existing renderer to a new source of paths.

## Alternatives considered

- **Merge into `/` with auto-escalation.** Rejected (D1): unpredictable dual
  behaviour behind one key — the hybrid this project avoids.
- **Hand-rolled `walkdir` + manual substring scan.** Rejected (D3/D4): slower,
  loses `.gitignore`/binary-detection correctness, and reinvents ripgrep badly.
  "More performant than all others" argues *for* the real engine, not against a
  dependency.
- **A CLI `--find` subcommand.** Rejected (D6): off-brand; the differentiator is
  the rendered preview, which is inherently interactive.
