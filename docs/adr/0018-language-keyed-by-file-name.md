# ADR 0018: Language keyed by file name, not extension

Status: **Accepted, 2026-08-24**

## Context

Every layer that answers "what kind of file is this" (`format::classify`,
`highlight::syntax_for`, `icons::nerd_glyph`/`nerd_color`, the directory
listing, the preview pane, the text viewer, recursive search) keyed off
`Path::extension()`. That works for `main.rs` or `README.md`, but a real
family of files is typed by its NAME, not its extension: `Dockerfile`,
`Makefile`, `.gitignore`, `.env`. `Path::extension()` returns `None` for all
of them (a dotfile's leading dot is not a stem/extension split; a bare
`Dockerfile` has no dot at all).

The concrete symptom: a `Dockerfile` classified as `Format::Binary` (the
generic "File" label, the `·` glyph) and opened with zero highlighting. The
evidence that this was already a known-but-unfixed gap, not a new problem:
`highlight::is_text_ext` already listed `"gitignore"` in its literal set, code
that has been dead since the day it was written, because `Path::extension()`
on `.gitignore` never produces `"gitignore"`. The table believed in a key that
the extraction never produced.

A second, unrelated bug shared the same file: `src/highlight.rs`'s
`Syntax::block_comment` only understood a single-char string delimiter and one
two-delimiter block span. `"""..."""` parsed as an empty string `""` followed
by a bare opening `"`, so a Python docstring's body was tokenised as ordinary
code, a keyword like `is` lit up mid-prose. Both bugs are fixed together here
because the docstring fix (a generalised block mechanism) is what lets
Dockerfile's `#` comments and the language table share one code path with
Python's triple-quoted strings.

## Decision

**D1: One pure function turns a file NAME into the lookup key.**
`highlight::lang_key(file_name: &str) -> String` is the single place that
answers "what extension-shaped key does this file's language and format get
looked up by". For an ordinary file it is the lowercased extension, unchanged
from before. For the named families, it is a canonical pseudo-extension:
`Dockerfile` / `Dockerfile.dev` / `web.dockerfile` all become `"dockerfile"`;
`Makefile` / `GNUmakefile` / `*.mk` become `"make"`; `.gitignore` and its
sibling ignore files become `"gitignore"`; `.env` and `.env.*` become `"env"`.
It lives in `highlight.rs`, next to `syntax_for`, rather than in `format.rs`,
because the key exists to answer a language question first (which `Syntax`
applies) and a format question second (`format::classify` just consumes the
same string). Every call site that used to compute `path.extension()...to_lowercase()`
now calls `lang_key` instead: `format::classify_path`, `dir::read_entries`,
`dir::preview_text_head`, the two `IconMode::Nerd` icon lookups in `dir.rs`,
`search.rs`'s per-entry classification, and `text.rs`'s viewer setup. One
currency, one place it is minted.

**D2: `format::classify` takes the key, not an extension.** Its parameter is
renamed `key`; its behaviour is unchanged; a known key wins outright, an
unknown or empty key falls back to sniffing the byte head. `classify_path`
computes `lang_key` from `path.file_name()` instead of `path.extension()`, so
a Dockerfile classifies as `Format::Text` without ever touching its bytes; the
existing "unknown key -> read the head" path stays byte-identical for files
that still key off the extension.

**D3: block comments generalise into typed, multi-line `Block` spans.**
`Syntax::block_comment: Option<(open, close)>` becomes
`Syntax::blocks: &'static [Block]`, where `Block { open, close, kind }` carries
its own `TokenKind`. A block comment and a triple-quoted docstring are the same
mechanism, an open delimiter, a close delimiter, and cross-line state, with a
different `kind`: `Comment` for one, `Str` for the other. Treating the
docstring as a special case bolted onto comment handling would have meant a
second, parallel piece of state; treating it as "one more block with a
different kind" means Python's `"""`/`'''` and JS/TS's template-literal
backtick both slot into the existing mechanism, and Rust/C/Go/CSS/HTML/Lua/Ruby
keep exactly the spans they had, just re-expressed as a one-element slice. The
per-line cross-line state changes from `bool` to `Option<usize>`, the index of
the currently-open block, so a file can carry an unterminated docstring or
comment across lines and close it with the delimiter that actually opened it,
not just "any close". Matching order inside `highlight_line` is block-open
first, then line comments, then single-char strings, then numbers, then
identifiers, which is what makes `"""` win over a lone `"`: the three-char
opener is tried and consumed before the single-char string branch ever sees
the first quote. The degenerate case, open and close being the identical
string, is handled by searching for the close starting one delimiter's length
past the open, so the opener can never match itself as its own closer.

**D4: `keywords_ci` is an explicit flag, not an implicit uppercase lookup.**
Docker instructions are case-insensitive (`from alpine` is as valid as
`FROM alpine`); the keyword table is written upper case for readability.
Rather than silently uppercasing every scanned word before comparison (which
would be wrong for every other language, where `let` and `LET` are not the
same token), `Syntax` gains `keywords_ci: bool`. Every existing language sets
it `false`, unchanged; only `"dockerfile"` sets it `true`. The comparison in
`highlight_line` branches on the flag: `eq_ignore_ascii_case` when set,
`contains` when not.

**D5: `make` and `.env`/`.gitignore` get syntaxes so `is_text_ext` needs no
literal fallback list for them.** Previously `is_text_ext` special-cased
`"gitignore"` and `"env"` as bare strings in a `matches!` list, dead for
`"gitignore"` as noted above, and coincidentally alive for `"env"` only because
a real `.tar.env`-style extension happened to match. Giving both a real
`Syntax` (via `syntax_for`) means `is_text_ext`'s `syntax_for(key).is_some()`
branch covers them honestly, and the stale literals are removed; `txt`, `log`,
`csv`, `text`, `lock`, `cfg`, `properties` keep their plain-text fallback
entries since they carry no real syntax of their own.

## Consequences

- A `Dockerfile`, `Makefile`, `.gitignore`, or `.env` now shows the right
  glyph and colour in the browser, highlights correctly in the preview pane
  and the text viewer, and the text viewer's status bar names the language
  (`dockerfile`, `make`, `gitignore`, `env`) instead of `text`.
- `lang_key` is the one place a new named-by-name family gets added; every
  consumer already speaks its output.
- A Python docstring, a JS/TS template literal, and a `/* */` comment all
  correctly span multiple lines and never leak a keyword or number token from
  their body, closing the exact reported bug (`is` inside `"""..."""`).
- `icons.rs` gained two table rows: `"dockerfile"` with the Docker brand blue
  (`Color::Rgb(36, 150, 237)`), an identity colour per the module's existing
  policy, and `"make"` reusing the shell glyph and its green tint rather than
  inventing a new Nerd Font code point for a family with no brand identity of
  its own.
