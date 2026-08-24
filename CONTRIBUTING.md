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

## Releasing

Every step below has been forgotten at least once, which is why it is a list.
The tap in particular sat a full release behind, so `brew` users were on 0.6.2
while the tag said 0.6.3.

1. `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
   `cargo test`.
2. Bump `version` in `Cargo.toml`, then `cargo build` so `Cargo.lock` follows.
   Pre-1.0 this project spends a **minor** version on anything that changes what
   an existing key or flag does, and a patch on additions and fixes.
3. `make notices`. It regenerates `THIRD_PARTY_LICENSES.md`, which stamps the
   version, so this is never a no-op on a release. The target refuses to run
   unless your `cargo-about` matches `.cargo-about-version`, because CI installs
   exactly that version and compares against it.
4. Move the `[Unreleased]` block in `CHANGELOG.md` under a dated `[x.y.z]`
   heading and leave a fresh empty `[Unreleased]` above it.
5. Commit as `release: x.y.z`, saying what changed for a user and why the
   version moved the way it did.
6. `git tag -a vx.y.z -m "sucher x.y.z: <one line>"`, then push `main` and the
   tag.
7. `gh release create vx.y.z --title "sucher x.y.z: <one line>" --notes-file <the
   changelog section>`.
8. `cargo publish --dry-run`, then `cargo publish`. This cannot be undone, only
   yanked.
9. **Bump the Homebrew tap**, which nothing does for you. In
   `john-athan/homebrew-tap`, point `Formula/sucher.rb` at the new tag's tarball
   and update `sha256`:

   ```sh
   curl -sL -o /tmp/v.tar.gz https://github.com/john-athan/sucher/archive/refs/tags/vx.y.z.tar.gz
   shasum -a 256 /tmp/v.tar.gz
   ```

10. Reinstall locally (`make install`) so the `s` on your PATH is the thing you
    just shipped, and check CI went green on the release commit.

## Scope

sucher aims to be a fast, good-looking terminal viewer for awkward-in-a-browser
files. Keep dependencies lean and the startup path quick.
