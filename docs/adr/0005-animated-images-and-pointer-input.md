# ADR 0005 — Animated images & pointer input

Status: **Accepted — 2026-07-09**

## Context

Two browser gaps:

1. **Animated GIFs play only their first frame** — both in the full-screen image
   viewer (`imgview`) and in the directory browser's preview pane. A GIF should
   loop, in place, in both.
2. **The breadcrumb isn't clickable** and there's no pointer navigation at all;
   users expect to click a path segment to jump there.

The image display path is shared: `media::ImagePane` wraps `ratatui-image` and is
used by the image, PDF, video, and Keynote viewers, plus the browser preview. The
video viewer already animates by decoding frames on a thread and calling
`pane.set(img)` per frame — proof the terminal-graphics path can sustain motion.

## Decision

**D1 — `ImagePane` becomes animation-aware; a still is a 1-frame animation.**
Rather than a parallel "animated pane" (a hybrid), every image the pane holds is a
sequence of `Frame { img: DynamicImage, delay: Duration }`:

- `set(img)` — one frame, `delay = 0`; never self-advances (unchanged for
  PDF/video/Keynote/still images; the video viewer keeps pushing its own frames).
- `set_animation(frames)` — N frames with per-frame delays; frame 0 shown
  immediately.
- `tick(now) -> bool` — for a multi-frame pane, advances to the next frame when
  its delay has elapsed (wrapping = looping) and re-encodes the protocol,
  returning `true` when the visible frame changed (so the caller redraws). A
  single-frame pane's `tick` is a no-op returning `false`.

The per-frame protocol re-encode is the cost; it happens only on an actual frame
change, and only while an animated pane is on screen.

**Decoding** lives in one pure-ish helper `media::decode_frames(path) -> Option<Vec<Frame>>`
using the `image` crate's `AnimationDecoder` (GIF via `GifDecoder::into_frames`).
It returns `None` for a non-animated / undecodable file (caller falls back to the
existing single-image decode). **Guards:** frames are capped (`MAX_FRAMES`, e.g.
300) and a per-frame minimum delay floor (e.g. 20 ms, matching browsers' handling
of 0-delay GIFs) is applied; a GIF exceeding the frame cap degrades to its first
frame (static) rather than exhausting memory — logged via the caption, not a
crash. Scope is **GIF only** for now; animated WebP/APNG are a later, mechanical
extension of `decode_frames`.

**Full view (`imgview`):** for a `.gif`, decode frames and `set_animation`; the
main loop tightens its poll while animating, calls `pane.tick(now)`, and redraws
on change. Non-GIF images are unchanged (1 s idle poll, no churn).

**Preview pane (`dir`):** the async raster worker's channel payload widens from
`Option<DynamicImage>` to a small `Rastered { Still(DynamicImage) |
Animated(Vec<Frame>) }`. For a GIF the worker decodes frames off-thread and ships
`Animated`; `show_image`/`show_animation` install it. The browser's `main_loop`
already tightens to a 60 ms poll while a raster is pending (ADR 0004 D3); it
additionally ticks the preview pane when the current preview is animated, and
stops as soon as the selection moves off the GIF (no idle churn on non-animated
selections). Animated previews are **not** added to the still `img_cache`
(bounded, frame sets are large); reselecting a GIF re-decodes off-thread — cheap
and backgrounded.

**D2 — Opt-in mouse capture for pointer navigation.** The browser enables
crossterm mouse capture (around `ratatui::init`/`restore`), gated by config
`mouse = true|false` (default `true`). While captured:

- **Breadcrumb** — each rendered path segment records its column span and target
  `PathBuf`; a click in the breadcrumb row navigates (`enter_dir`) to that
  segment's directory.
- **Wheel** — scroll up/down moves the selection (cheap, expected once the mouse
  is live).

*Tradeoff, documented:* capturing the mouse disables the terminal's native
click-drag text selection inside sucher. This is the norm for full-screen TUIs
(ranger, lf) and most terminals still allow Shift/Option-drag to bypass capture;
users who prefer native selection set `mouse = false`. Mouse is disabled cleanly
on exit so the shell is never left in capture mode (a `restore` guard runs on
every exit path).

## Consequences

- One animation abstraction in `ImagePane` serves stills and GIFs; adding APNG/
  WebP is just another `decode_frames` branch. No second render path.
- The raster channel carries a `Rastered` enum; `pump_raster`/`show_*` gain an
  animated arm. Preserves the "worker never touches the pane" threading rule.
- `decode_frames`' cap/floor and the breadcrumb segment-hit math are pure and
  unit-tested (frame count/delay-floor logic; x→segment resolution); the terminal
  and the clock stay out of the tested core (ADR 0001 ethos).
- New config `mouse`; new terminal mode (mouse capture) with a guaranteed
  teardown. `image` crate's `gif`/animation features may need enabling in
  `Cargo.toml`.
