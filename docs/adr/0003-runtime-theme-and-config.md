# ADR 0003 — Runtime theme palette + user config

Status: **Accepted — 2026-07-09**

## Context

Sucher's colours were compile-time `pub const`s in `theme.rs`, consumed as
`theme::DIR` etc. across the browser and viewers. Two related wants can't be met
by consts:

1. **User-configurable themes** — ship curated palettes (and let users override
   individual colours) without a recompile.
2. **Auto light/dark** — pick a legible palette for the terminal's background.

Separately, the browser wants **Nerd Font icons** (per-extension glyphs) for a
modern look, but Nerd Font *presence* cannot be reliably detected from inside a
process, so it must be a user choice with a safe default.

All of this needs a small **config surface** (file + env + flag) that didn't
exist — the only argument handling was `--plain` / `-h`.

## Decision

**D1 — Colours become a runtime `Palette`, held in a process-global `OnceLock`.**
`theme.rs` defines `struct Palette { dir: Color, image: Color, … , accent: Color }`
and a `static PALETTE: OnceLock<Palette>`. `theme::init(Palette)` sets it once at
startup; `theme::palette() -> &'static Palette` reads it (via `get_or_init` with
the default dark palette, so tests and any pre-init path are safe). Call sites
move from the const `theme::DIR` to the field `theme::palette().dir`.

*Rejected — thread `&Palette` through every render function:* the theme is
immutable for the process's life; a global read is the honest model and avoids
churning dozens of signatures for no gain. *Rejected — keep consts:* cannot be
user-configured, the whole point.

**D2 — One `Config`, resolved from flag > env > file > built-in default.**
`config.rs` owns a `Config { theme: ThemeChoice, icons: IconMode, overrides }`.
Precedence, highest first:

- Theme: `--theme <name>` → `SUCHER_THEME` → file `theme = "…"` → `sucher-dark`.
- Icons: `--icons <mode>` → `SUCHER_ICONS` → file `icons = "…"` → `unicode`.

The file is TOML at `$XDG_CONFIG_HOME/sucher/config.toml` (falling back to
`~/.config/sucher/config.toml`). A missing/malformed file is non-fatal — it logs
nothing and uses defaults, because a broken config must never stop you opening a
file.

```toml
theme = "catppuccin-mocha"   # or "auto" | built-in name
icons = "nerd"               # "unicode" (default) | "nerd" | "none"

[colors]                     # optional per-key hex overrides, applied last
accent = "#7dd3fc"
```

**D3 — Built-in palettes, current look preserved as the default.** The existing
colours become `sucher-dark` and stay the default — no surprise reskin. Added:
`sucher-light`, `catppuccin-mocha`, `gruvbox-dark`, `tokyo-night`. Each is a pure
`fn() -> Palette`, name-matched in one table.

**D4 — `theme = "auto"` picks light/dark by the terminal background.** Detection
runs once, *before* the alternate screen, best-effort and short: read `COLORFGBG`
if present, else query OSC 11 (`ESC ] 11 ; ? ESC \`) with a ~100 ms read timeout,
else assume dark. The luma decision is a pure `is_dark(r,g,b) -> bool` (Rec. 601
luma < 0.5), unit-tested; only the query does IO. Any failure → dark. `auto` maps
to `sucher-dark` / `sucher-light` unless a specific non-auto theme was named.

**D5 — Icons are a mode, not a detection.** `IconMode::Unicode` (the current
geometric set — default, renders everywhere), `Nerd` (per-extension Nerd Font
glyphs + per-extension accent colour), `None` (no glyph column). Nerd Font
presence is deliberately *not* auto-detected — there's no reliable process-level
signal, and guessing wrong prints mojibake, which reads as broken. Users with a
Nerd Font opt in via config/env; the README documents it.

The per-extension icon+colour lookup is a pure function keyed on the lowercased
extension with a `Format` fallback, unit-tested without a terminal. It layers
*above* `Format` (which still drives viewer dispatch and the `unicode`/coarse
colouring) — a `.rs` and a `.py` are both `Format::Text` but get distinct Nerd
glyphs and tints.

## Consequences

- Adding a theme = one `fn` + one match arm. Adding a per-extension icon = one
  table entry. Both stay pure and tested (consistent with ADR 0001's
  single-registry ethos).
- `theme::palette()` is a global read; acceptable because the palette is set once
  and never mutated. Tests that need a specific palette can `init` before asserting
  (or rely on the default).
- A new `config.rs` module and two small deps (`toml`, `dirs`). OSC 11 detection
  is implemented inline (no dep) and fully guarded by a timeout.
