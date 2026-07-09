# ADR 0006 — Navigation animations (time-based, framerate-independent)

Status: **Accepted — 2026-07-09**

Supersedes the blanket rejection in ADR 0004 D3.

## Context

ADR 0004 D3 rejected preview transitions as "faked" jank. That was too broad:
animating the motion of *real content* — interpolating actual cell colours, or
growing the real image's render rect — is honest motion, not a fake crossfade.
The user wants navigation to animate: folder enter/leave and full-view
open/close. They also set a hard bar: **"actually 120 Hz on a 120 Hz display, or
prove it's impossible."**

## The 120 Hz question (answered honestly)

A terminal app emits frames into a PTY; the terminal emulator parses them and
presents to the display on its own (usually vsync'd) schedule. Two regimes:

- **Cell/text animation** (folder fade). Per frame ratatui writes only the diff —
  a pane is ~2 KB of escape codes. GPU terminals (Ghostty, Kitty, Alacritty,
  WezTerm) parse+upload that in well under 1 ms and present at the display's
  refresh. So emitting ≥120 distinct interpolated frames/sec is easily within the
  8.3 ms budget, and a 120 Hz-vsync terminal shows them at 120 Hz. **Achievable.**
- **Graphics animation** (full-view image zoom). Each frame must resize + encode
  the image to the graphics protocol and push tens-to-hundreds of KB over the
  PTY. Encode+transmit alone routinely exceeds 8.3 ms for non-trivial images, so
  **true 120 Hz is impossible for general images**; it runs encode/transmit-bound
  (~20–60 fps). We keep the *duration* constant with time-based easing and use a
  downscaled bitmap for intermediate frames.

A TUI **cannot portably query the monitor's refresh rate** — so we do not "target
120 Hz". We render **time-based** and emit as fast as the per-frame budget allows,
capped at a high ceiling (~4 ms ⇒ ≤250 fps emission); the terminal's vsync
throttles to the actual display rate. Emitted FPS is measurable, and
`SUCHER_ANIM_STATS=1` prints the achieved rate per animation on exit — the proof,
and the demonstration of the graphics-path ceiling.

## Decision

**D1 — One time-based animation engine (`anim.rs`).** `struct Anim { start:
Instant, dur: Duration }` with `progress(now) -> f32` (clamped 0..1 =
`elapsed/dur`), `done(now) -> bool`, and a pure easing `ease_out_cubic(t)`.
Animation state is a function of **wall-clock elapsed**, never a frame counter, so
duration is identical whether the pipeline sustains 30 or 250 fps — a dropped
frame costs smoothness, never timing. All of `anim.rs` is pure and unit-tested
(endpoints, monotonicity, clamping, easing 0→0 / 1→1). A process-global
`anim::enabled()` (set once at startup, mirroring `theme::init`) lets any viewer —
including the in-process `imgview` launched with no config in hand — honour the
toggle.

**D2 — Frame pump, gated; interruptible; zero idle cost.** While an animation is
live the browser/viewer loop polls at ~4 ms, recomputes the interpolated frame
from `progress(now)`, and redraws until `done`. This reuses the existing
"tighten the poll while work is live" structure (ADR 0004 D3 spinner, 0005 D1
GIF); when no animation is live the loop still blocks at the 1 s idle poll — **no
new idle churn**. Any keypress during an animation ends it immediately (jump to
the final state) and is then handled normally — motion never adds latency.

**D3 — What animates.** *Folder enter/leave* → a fast (~120 ms) `ease_out_cubic`
**fade-in** of the current pane: each entry's foreground is `lerp(bg, colour,
eased)` so the new listing resolves from the background. This is the cheap-cell
path that can present at the display refresh. *Full-view open/close* → the image's
render rect grows from a small centred box to full (open) and shrinks back
(close) over ~150 ms, time-based; intermediate frames use a downscaled image so
encode stays cheap, the settle frame is full-res. Directory-slide (panes
translating) is deliberately *not* done now — a true cell-slide needs
buffer-blitting and reads as jank on slower terminals; the fade is the reliable,
honestly-smooth choice and a slide can layer on later.

**D4 — Config `animate = true|false` (default true).** Precedence flag
(`--no-animate`) > env (`SUCHER_ANIMATE`) > file > default, same shape as
`git`/`mouse`. Off ⇒ every transition is instant and no anim code runs.

## Consequences

- `anim.rs` is a tiny pure engine; adding an animation is "start an `Anim`, read
  `progress` in render". No frame-count bugs, no per-terminal tuning.
- Honesty about the ceiling is built in: `SUCHER_ANIM_STATS` reports achieved FPS,
  making the cell-vs-graphics difference observable rather than asserted.
- New global `anim::enabled()` and config `animate`; the loops gain a gated fast-
  poll arm. The idle path and all prior features are unchanged.
- We interpolate toward a background colour we assume from the palette; on
  terminals whose real bg differs, the fade origin is approximate but brief.
