# 0016 — Data files (parquet / JSONL / SQLite / DuckDB) via an embedded DuckDB

Status: accepted
Date: 2026-07-21

## Context

sucher frames the files that are awkward to open in a browser. One family is
conspicuously missing, and it is the family its most technical users live in:
**columnar and database files** — Parquet, newline-delimited JSON, SQLite
databases, DuckDB databases. Today `s data.parquet` hexdumps bytes; `s app.db`
hexdumps bytes; `s events.jsonl` opens as plain text. For a terminal-native,
future-oriented audience these are exactly the files they reach for, and every
one of them currently degrades to "binary" or "raw text".

The existing grid viewer (`sheet.rs`) already renders tabular data well over a
backend-agnostic `Book` seam (`names / selected / select / dims / window /
find`), with two backends: a streaming `.xlsx` reader (`xlsx.rs`) and an eager
calamine/CSV `MemBook`. A data-file family needs a *third* backend behind the
same seam — and, uniquely, it wants a **query** capability, because the whole
point of these formats is that they are queryable.

## Decision

Add a **`Format::Data`** classification (Parquet, JSONL/NDJSON, SQLite, DuckDB)
that routes to the existing grid viewer, backed by a new **`Book::Data`** that
embeds **DuckDB** (the `duckdb` crate, `bundled`). The grid gains an optional
**SQL prompt** available only when the active book is a `Data` book.

Viewer ≠ format: just as `.docx`/`.pptx`/`.html` reduce to the markdown viewer
(ADR 0010), `Format::Data` reduces to the grid viewer. The streaming `.xlsx` and
CSV backends are **not touched** — they remain the fast path for spreadsheets.

> **Correction (post-CI, 2026-07-21).** The original decision below assumed
> DuckDB read *all four* formats offline, including SQLite via its scanner. That
> was wrong: the initial spike ran on a machine whose `~/.duckdb/extensions`
> cache had been populated by an earlier network auto-install, so the SQLite (and
> even Parquet) reads only *appeared* statically bundled. On a clean machine (CI,
> or an empty `extension_directory`) the truth is:
> - **Parquet and JSON are static** — but only when the `duckdb` crate's
>   `parquet` and `json` **features** are enabled (they compile those readers
>   into libduckdb). They are not in the default `bundled` set.
> - **The SQLite scanner has no static feature at all** (`libduckdb-sys` exposes
>   `parquet`/`json` but no `sqlite`); it is loadable-only and auto-installs over
>   the network on first use.
>
> So DuckDB is kept for Parquet / JSONL / native DuckDB (all static + offline via
> the `parquet`+`json` features), and **SQLite is read with `rusqlite`** — its own
> statically-bundled engine — rather than compromising the offline guarantee. See
> the updated "Offline" and "SQLite" sections and "Spike evidence".

### Why DuckDB, and why bundled

DuckDB is the reference SQL-on-files engine. One dependency reads Parquet, JSONL,
and native DuckDB databases and gives real SQL over them — `read_parquet`,
`read_json_auto`, and native `ATTACH`. (SQLite is handled separately; see below.)
The alternatives were weighed and rejected:

- **Polars** — all-Rust and lighter, but has **no native SQLite reader**, so
  SQLite (a headline developer format) would need a separate `rusqlite` path: a
  hybrid. Its SQL surface is also narrower.
- **Per-format Rust crates** (`parquet` + `arrow` + `rusqlite` + `serde_json`) —
  leanest, but SQL then needs a *fifth* bolted-on engine, and there are four
  readers to maintain instead of one. Most code, most hybrid, weakest SQL.

DuckDB is the only option where SQL and SQLite are *free* and there is exactly
**one engine** — the non-hybrid choice for a feature whose entire draw is SQL.

`bundled` compiles DuckDB from vendored source (via `libduckdb-sys`), so
`cargo install sucher` is self-contained — no sidecar library, no download at
build time (unlike pdfium, ADR 0015), no network at runtime. It requires a C++
toolchain at build time, which the Rust toolchain's `cc` already implies and CI
runners already have.

### Offline is non-negotiable — proven, not assumed

sucher's identity is a **local** viewer that holds no credentials and needs no
network (README "Remote & cloud files"). DuckDB's format readers are *loadable
extensions*, and DuckDB will by default **auto-install them over the network** on
first use. That would silently break the offline guarantee.

With the `parquet` and `json` crate features enabled, those two readers — plus
native DuckDB `ATTACH` — are compiled **into** libduckdb and are available with
*no* extension directory and *no* network. Verified on a clean machine by
pointing `extension_directory` at an empty folder and disabling both auto-flags:

```sql
SET autoinstall_known_extensions = false;  -- never install (never touch network)
SET autoload_known_extensions    = false;  -- never even load from the on-disk
                                           -- extension cache; only built-ins work
```

Every DuckDB connection sucher opens runs these first. `autoinstall = false` is
the network guard; `autoload = false` additionally refuses the on-disk extension
cache, so only the statically-compiled readers (`parquet`, `json`, core) can run
— a reader that is not built in fails loudly instead of phoning home. This is
exactly why SQLite cannot go through DuckDB here (its scanner is neither, so it
would fail under these pragmas) and is read with rusqlite instead.

### Reading pattern: DESCRIBE for schema, CAST-to-VARCHAR for cells

The grid is a grid of strings. Rather than match DuckDB's full logical-type
lattice in Rust (fragile; the spike hit internal panics reading typed values via
`ValueRef`), the backend:

1. `DESCRIBE <relation>` → `(column, type)` pairs. This yields the schema
   **without executing** the query — instant even on a billion-row Parquet.
2. Builds a display query `SELECT CAST("col" AS VARCHAR), … FROM (<relation>)`
   and reads each cell as `Option<String>` — NULL-safe, and DuckDB's canonical
   text rendering gives correct ISO dates/timestamps for free (fixing, for this
   backend, the serial-number date wart the `.xlsx` path has).

The column *types* from step 1 drive alignment (numerics right-align) and the
header hint.

### Lazy, uncapped windowing — a third backend shape, honestly

`MemBook` is eager; `StreamBook` streams into a **row-capped** store because
parsing `.xlsx` is slow. DuckDB is neither: it is a fast lazy query engine, so
forcing it into either mold would be a workaround. The `Data` book instead
**windows on demand** — `… LIMIT <viewport> OFFSET <top>` around the visible
range, with a prefetch margin cached so ordinary scrolling is a cache hit, and
total row count from a one-shot `COUNT(*)`. This means the grid can scroll an
arbitrarily large file with **no row cap** — a genuine improvement over the
`.xlsx`/CSV backends, and the design that fits the engine instead of fighting it.

Queries run synchronously on the UI thread (as `MemBook` does): DuckDB window
reads are single-digit-to-low-tens of milliseconds, well inside interactive
budget. A dedicated service thread (as pdfium uses, ADR 0015) is the documented
escalation if profiling ever shows a UI stall on pathological files; it is not
warranted for v1.

### SQLite via rusqlite (not DuckDB)

Because DuckDB's SQLite scanner is loadable-only (would break the offline
guarantee, see the Correction above), SQLite databases are read with the
`rusqlite` crate (`bundled`), which statically compiles libsqlite — fully offline,
no build-time download, no runtime install. This is a second read engine behind
the same `Book::Data` interface: DuckDB for Parquet/JSONL/DuckDB, rusqlite for
SQLite. It is *native-engine-per-source*, not a hybrid of interchangeable
libraries — each file is read by the engine that actually owns its format, and
both are equally lazy, windowed, and offline. The `:` SQL prompt therefore runs
DuckDB SQL on a DuckDB-backed source and SQLite SQL on a SQLite file — which is
the intuitive behaviour (you query a `.db` with SQLite SQL). Both engines expose
the same internal shape (list tables → sheets, `DESCRIBE`/`PRAGMA` for schema,
`LIMIT`/`OFFSET` windowing, a query override), so the grid is unaware which one
backs a given file.

### Sheets = tables

Multi-relation sources map onto the grid's existing sheet tabs for free: each
**table** in a SQLite/DuckDB database is a "sheet". Single-relation files
(Parquet, JSONL) are one sheet named by the file stem, exposed as a view so the
SQL prompt can `FROM <stem>` naturally.

### SQL prompt

The grid gains a `:` prompt (paralleling the existing `/` search prompt),
enabled only for `Data` books via a capability method on the `Book` enum
(`set_sql` / `supports_sql`). Submitting replaces the active sheet's relation
with the user's query; a parse/bind error is shown in the status line and the
previous relation is kept. The engine that runs it is DuckDB for every source,
including SQLite tables (queried through DuckDB's scanner) — **one SQL dialect**,
no per-source split.

### Feature gate

All of the above sits behind a default-on Cargo feature `data` (`duckdb` is an
`optional` dependency). `cargo install sucher` gets it; anyone who wants the lean
~26 MB binary builds `--no-default-features`. Without the feature the target
extensions fall back to their pre-existing handling (Parquet → binary hexdump,
JSONL → text).

## Scope

**In:** `.parquet` `.pq`; `.jsonl` `.ndjson`; `.sqlite` `.sqlite3` `.db` `.db3`;
`.duckdb` `.ddb`. Read-only (databases attached `READ_ONLY`).

**Out (documented), and why:**

- **Arrow/Feather files** (`.arrow` `.feather` `.ipc`). The spike found the Arrow
  *file* reader is **not** in the bundled build (`Copy Function "arrow" does not
  exist`); enabling it would require a network extension install, violating the
  offline guarantee. Excluded rather than compromised. Parquet covers the
  columnar-interchange need.
- **`.json`** stays `Text` — a `.json` file is usually a single document, not a
  table; only line-delimited `.jsonl`/`.ndjson` are tabular.
- **Writing / editing.** sucher is a viewer; databases are opened `READ_ONLY`.

## Consequences

- New dependencies `duckdb` (`bundled`, `parquet`, `json`) and `rusqlite`
  (`bundled`) — the two data engines, both statically linked and offline.
  The binary grows substantially: the release binary rose from ~26 MB to a
  **measured ~65 MB**, and a clean build compiles DuckDB's C++ (minutes, once,
  cached thereafter). This is the real cost;
  it is contained by the `data` feature gate (opt-out to the lean binary) and
  justified by turning four "binary/blob" formats into first-class, queryable
  views. Supply chain is a single pinned crate, auditable by cargo-deny.
- `cargo install sucher` gains Parquet/JSONL/SQLite/DuckDB viewing and terminal
  SQL with no extra steps and no runtime network.
- The `Book` seam now spans three backend shapes (eager, capped-streaming, lazy
  DuckDB) — the seam holds; each backend is the right shape for its source.
- The grid grows an optional SQL prompt, its first *capability that varies by
  backend*; expressed as a capability method on `Book`, not a new viewer.
- Offline behaviour is enforced in code (`autoinstall = false`) and asserted in
  tests, not left to DuckDB's network-happy defaults.

## Spike evidence

### Corrected run (2026-07-21, on a *clean* extension dir)

The decisive test: `duckdb 1.x` + `features = ["bundled", "parquet", "json"]`,
`SET extension_directory='<empty>'; autoinstall=false; autoload=false` (a
clean-machine simulation — no cached extensions, no network):

- `read_parquet('…')` — **OK** (static via the `parquet` feature).
- `read_json_auto('…')` on `.jsonl` — **OK** (static via the `json` feature).
- `ATTACH '…native.duckdb' (READ_ONLY)` — **OK** (core).
- `ATTACH '…' (TYPE SQLITE)` — **FAILS**: `Extension "…/sqlite_scanner.duckdb_extension"
  not found`. This is the finding the first spike missed → SQLite moved to rusqlite.
- `DESCRIBE <relation>` returns `(name, type)` without executing — **OK**;
  `CAST(col AS VARCHAR)` read returns clean strings incl. ISO dates, Unicode,
  NULL → `None` — **OK**.
- `COPY … (FORMAT ARROW)` / `read_arrow('…')` — **FAILS** (`arrow` function
  absent) → Arrow files excluded from scope.

### Original run (2026-07-21) — superseded, kept as the cautionary record

The first spike ran with `autoload=true` on a developer machine whose
`~/.duckdb/extensions` cache already held network-installed `parquet` and
`sqlite_scanner` extensions (from an earlier default-pragma run). It therefore
reported SQLite `ATTACH` as "statically bundled — OK", which CI disproved on a
clean runner. Lesson: prove offline claims with an **empty `extension_directory`**,
never against a developer's populated cache.
