# Contributing

Thanks for your interest in sucher.

## Development

```sh
cargo build            # debug build
cargo test             # unit tests (markdown, docx, xlsx)
cargo clippy           # lints
cargo fmt              # format
make run               # run against samples/sample.md
```

CI runs `fmt --check`, `clippy -D warnings`, `test`, and a release build, so
please run those locally before opening a PR.

## Adding a format

Each format lives in its own module under `src/` and exposes:

- `run(title, path)` — the interactive TUI (TTY), and
- a non-interactive `dump`/`to_markdown` for piped output.

Classification has a single source of truth (see `docs/adr/0001`): add the
variant to the `Format` enum and its extension mapping in `src/format.rs` —
that one registry drives both which viewer opens a file and how the directory
browser colours and previews it. Then dispatch the new variant in `src/main.rs`
(the `main()` match for TTY vs. pipe, and `open_interactive` for previews).

**Data files are the exception.** Parquet/JSONL/SQLite/DuckDB reduce to the
shared grid viewer via `src/data.rs` (an embedded DuckDB `DataBook`), behind the
default-on `data` Cargo feature — analogous to how docx/pptx/html reduce to the
markdown viewer (see `docs/adr/0016`). Adding another data format is therefore a
branch in the `DataBook`/classifier, not a new module or viewer.

## Runtime dependencies

PDF needs poppler (`pdftocairo`, `pdfinfo`, `pdftotext`); video needs `ffmpeg`
and `ffprobe`. Keep these optional — the tool should degrade gracefully when a
backend is missing.

## Scope

sucher aims to be a fast, good-looking terminal viewer for awkward-in-a-browser
files. Keep dependencies lean and the startup path quick.
