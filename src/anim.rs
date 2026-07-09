// Time-based animation engine (ADR 0006). One tiny, pure core: an [`Anim`] is a
// start instant plus a duration, and every derived value — `progress`, `done`,
// the eased factor — is a function of **wall-clock elapsed**, never a frame
// counter. That is the whole trick behind framerate-independence: a fade lasts
// the same real 120 ms whether the loop sustains 30 fps or 250 fps, and a
// dropped frame costs a little smoothness but never stretches the timing.
//
// The module is deliberately clock-free except at its edges: callers read
// `Instant::now()` at the render/loop boundary and pass it in, so the pure parts
// (`progress`, `ease_out_cubic`, `lerp_color`) are unit-tested with injected
// instants and no real waiting. Alongside the engine sit two process globals
// mirroring `theme`: an `enabled()` toggle (so any viewer, including the
// config-less in-process `imgview`, honours `animate = false`) and an opt-in
// frame-stats sink that proves the achieved emission FPS on exit.

use ratatui::layout::Rect;
use ratatui::style::Color;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A single time-based animation: the instant it began and how long it runs.
/// `Copy` so the browser can hold it in an `Option<Anim>` and cheaply snapshot
/// it each loop iteration. All state is derived — there is no mutable cursor.
#[derive(Clone, Copy, Debug)]
pub struct Anim {
    start: Instant,
    dur: Duration,
}

impl Anim {
    /// Start an animation at `now` lasting `dur`. The clock is injected so the
    /// engine stays pure and testable; production callers pass `Instant::now()`.
    pub fn new(now: Instant, dur: Duration) -> Self {
        Anim { start: now, dur }
    }

    /// Linear progress in `0.0..=1.0`: `elapsed / dur`, clamped. A zero (or
    /// negative-elapsed) duration is treated as already complete (→ `1.0`) so a
    /// degenerate animation renders its final frame at once rather than dividing
    /// by zero. `now` before `start` reads as `0.0` (saturating).
    pub fn progress(&self, now: Instant) -> f32 {
        let dur = self.dur.as_secs_f32();
        if dur <= 0.0 {
            return 1.0;
        }
        let elapsed = now.saturating_duration_since(self.start).as_secs_f32();
        (elapsed / dur).clamp(0.0, 1.0)
    }

    /// Whether the animation has reached (or passed) its end — `progress >= 1.0`.
    pub fn done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }

    /// Wall-clock elapsed since `start`, saturating at zero. Used only to report
    /// the achieved FPS to the stats sink when an animation completes.
    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.start)
    }
}

/// Cubic ease-out: fast to start, gently settling — `1 - (1 - t)^3` for `t` in
/// `0..=1` (input clamped for safety). Fixes the endpoints (`0 → 0`, `1 → 1`),
/// is monotonically increasing, and sits at or above the linear ramp across the
/// interior (e.g. `0.5 → 0.875`), so a fade feels immediate then eases in.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// The starting scale of the full-view open/close zoom (ADR 0006 D3): at `t = 0`
/// the image is a small centred box this fraction of the display, growing to the
/// whole area at `t = 1`. Small enough to read as "opening", large enough that
/// even the first frame is a recognisable thumbnail.
const ZOOM_START: f32 = 0.15;

/// Centred sub-rect of `full` scaled by a factor that ramps from [`ZOOM_START`]
/// at `t = 0` to `1.0` at `t = 1` — the geometry of the image viewer's open/close
/// zoom (ADR 0006 D3). Pure so the "always inside, centred, exact at the ends"
/// invariants are unit-tested without a terminal.
///
/// The endpoint identity matters: `zoom_rect(full, 1.0) == full` **exactly** (the
/// scale is `1.0`, the rounded dimensions land back on `full`'s, the centring
/// offset is zero), so the intro's settle frame renders the picture at precisely
/// the size and position the normal `render` uses — no visible jump when the zoom
/// hands off to the static display. `w`/`h` are floored to `1` and capped at
/// `full`'s so the rect is never empty and never spills outside `full` (the
/// offsets use `saturating_sub`, so a degenerate zero-width `full` can't underflow).
pub fn zoom_rect(full: Rect, t: f32) -> Rect {
    let scale = ZOOM_START + (1.0 - ZOOM_START) * t.clamp(0.0, 1.0);
    let w = (full.width as f32 * scale).round() as u16;
    let h = (full.height as f32 * scale).round() as u16;
    let w = w.clamp(1, full.width.max(1));
    let h = h.clamp(1, full.height.max(1));
    Rect {
        x: full.x + full.width.saturating_sub(w) / 2,
        y: full.y + full.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}

/// Component-wise linear interpolation between two colours at factor `t` (clamped
/// to `0..=1`): `t = 0` yields `from`, `t = 1` yields `to` **exactly**, so a fade
/// that runs to completion produces a byte-for-byte identical colour to the
/// non-animated render (see the `lerp_endpoints` test).
///
/// Sucher's palette is `Color::Rgb` everywhere (see `theme.rs`; every file-kind,
/// git, and nerd colour resolves to an RGB triple), so only the `Rgb`×`Rgb` case
/// interpolates. Any non-`Rgb` input falls back to `to` — still exact at the
/// `t = 1` endpoint and never inventing an off-palette colour — which in practice
/// is unreachable for the browser's fade.
pub fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (from, to) {
        (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) => {
            Color::Rgb(lerp_u8(fr, tr, t), lerp_u8(fg, tg, t), lerp_u8(fb, tb, t))
        }
        _ => to,
    }
}

/// Linear interpolation of one 8-bit channel. Rounds to the nearest integer so
/// the `t = 1` endpoint lands exactly on `b` (`a + (b - a) * 1 == b`).
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let a = a as f32;
    let b = b as f32;
    (a + (b - a) * t).round().clamp(0.0, 255.0) as u8
}

// --- Process-global enable toggle (mirrors `theme`) ---------------------------

/// The process-global animate toggle. Set once at startup from the resolved
/// config; read via [`enabled`]. Defaults to `true` when never set, so tests and
/// pre-init paths animate by default just like the shipped config.
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Install the resolved `animate` setting. Idempotent (`OnceLock`): the first
/// call wins. Called once from `main` after config resolves, beside `theme::init`.
pub fn set_enabled(b: bool) {
    let _ = ENABLED.set(b);
}

/// Whether animations are enabled. `true` if [`set_enabled`] was never called,
/// matching the config default so any viewer can gate on this global directly.
pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| true)
}

// --- Frame-stats sink: the 120 Hz proof (opt-in via SUCHER_ANIM_STATS) --------

/// One completed animation's measured emission: its kind, the number of frames
/// drawn, the wall-clock it took (ms), and the achieved frames-per-second.
type Stat = (&'static str, u32, f32, f32);

/// Collected stats, guarded for the (rare) case of concurrent recorders. Only
/// ever populated when [`stats_on`] is true.
static STATS: OnceLock<Mutex<Vec<Stat>>> = OnceLock::new();

/// Cached one-time check of the `SUCHER_ANIM_STATS` env var: any value (even
/// empty) turns the sink on. Checked once so the hot `record` path is a cheap
/// bool read, not a per-call env lookup.
fn stats_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SUCHER_ANIM_STATS").is_some())
}

/// Record a finished animation's frame count and elapsed time. A no-op unless
/// `SUCHER_ANIM_STATS` is set, so the instrumentation costs nothing in normal
/// runs. The achieved FPS is derived here (`frames / elapsed`) and stashed for
/// [`dump_stats`] to print after the terminal is restored.
pub fn record(kind: &'static str, frames: u32, elapsed: Duration) {
    if !stats_on() {
        return;
    }
    let ms = elapsed.as_secs_f32() * 1000.0;
    let fps = if ms > 0.0 {
        frames as f32 / (ms / 1000.0)
    } else {
        0.0
    };
    if let Ok(mut v) = STATS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        v.push((kind, frames, ms, fps));
    }
}

/// Print every recorded animation's kind, frame count, elapsed ms, and achieved
/// FPS to stderr — the honest, measurable answer to "actually 120 Hz?" (ADR
/// 0006). A no-op unless `SUCHER_ANIM_STATS` is set. Must be called **after** the
/// alternate screen is torn down (from `main`, post-`restore`) so it never
/// corrupts the TUI.
pub fn dump_stats() {
    if !stats_on() {
        return;
    }
    let Some(m) = STATS.get() else { return };
    let Ok(v) = m.lock() else { return };
    for (kind, frames, ms, fps) in v.iter() {
        eprintln!("sucher anim: {kind}: {frames} frames in {ms:.1} ms → {fps:.0} fps");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed base instant so progress maths is exact and reproducible.
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn progress_endpoints_and_clamping() {
        let now = base();
        let a = Anim::new(now, Duration::from_millis(100));
        // At start → 0.0.
        assert_eq!(a.progress(now), 0.0);
        // At start + dur → exactly 1.0.
        assert_eq!(a.progress(now + Duration::from_millis(100)), 1.0);
        // Past the end → clamped to 1.0.
        assert_eq!(a.progress(now + Duration::from_millis(500)), 1.0);
        // Halfway → ~0.5.
        let mid = a.progress(now + Duration::from_millis(50));
        assert!((mid - 0.5).abs() < 1e-4, "mid was {mid}");
    }

    #[test]
    fn zero_duration_is_immediately_complete() {
        let now = base();
        let a = Anim::new(now, Duration::from_millis(0));
        assert_eq!(a.progress(now), 1.0);
        assert!(a.done(now));
    }

    #[test]
    fn done_transitions_at_the_end() {
        let now = base();
        let a = Anim::new(now, Duration::from_millis(100));
        assert!(!a.done(now));
        assert!(!a.done(now + Duration::from_millis(99)));
        assert!(a.done(now + Duration::from_millis(100)));
        assert!(a.done(now + Duration::from_millis(200)));
    }

    #[test]
    fn ease_out_cubic_endpoints_monotonic_and_above_linear() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        // Clamps out-of-range inputs.
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
        // Monotonically increasing and >= the linear ramp across the interior.
        let mut prev = 0.0;
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let e = ease_out_cubic(t);
            assert!(e >= prev - 1e-6, "not monotonic at {t}");
            assert!(e >= t - 1e-6, "ease below linear at {t}: {e} < {t}");
            prev = e;
        }
        // A concrete interior point: 0.5 → 0.875.
        assert!((ease_out_cubic(0.5) - 0.875).abs() < 1e-6);
    }

    #[test]
    fn zoom_rect_full_at_t_one() {
        // The no-jump identity: at t = 1 the sub-rect is exactly `full`, so the
        // intro's settle frame matches the normal render pixel-for-pixel.
        let full = Rect::new(3, 5, 80, 24);
        assert_eq!(zoom_rect(full, 1.0), full);
        // Out-of-range t clamps to the same full rect (never overshoots).
        assert_eq!(zoom_rect(full, 2.0), full);
    }

    #[test]
    fn zoom_rect_small_and_centred_at_t_zero() {
        let full = Rect::new(0, 0, 100, 40);
        let r = zoom_rect(full, 0.0);
        // Starts at ZOOM_START (0.15) of each dimension: 15 wide, 6 tall.
        assert_eq!(r.width, 15);
        assert_eq!(r.height, 6);
        // Centred: left margin == right margin (even gaps here).
        assert_eq!(r.x, (100 - 15) / 2);
        assert_eq!(r.y, (40 - 6) / 2);
        // Clamped t (negative) behaves like t = 0.
        assert_eq!(zoom_rect(full, -1.0), r);
    }

    #[test]
    fn zoom_rect_always_inside_and_symmetric() {
        let full = Rect::new(7, 2, 81, 25); // odd dimensions to stress centring
        let mut prev_w = 0;
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let r = zoom_rect(full, t);
            // Never empty.
            assert!(r.width >= 1 && r.height >= 1, "empty at t={t}");
            // Fully contained within `full`.
            assert!(r.x >= full.x && r.y >= full.y, "escapes top-left at t={t}");
            assert!(r.right() <= full.right(), "escapes right at t={t}: {r:?}");
            assert!(
                r.bottom() <= full.bottom(),
                "escapes bottom at t={t}: {r:?}"
            );
            // Symmetric centring: left and right margins differ by at most 1
            // (integer division of an odd gap).
            let left = r.x - full.x;
            let right = full.right() - r.right();
            assert!(left.abs_diff(right) <= 1, "off-centre x at t={t}");
            let top = r.y - full.y;
            let bottom = full.bottom() - r.bottom();
            assert!(top.abs_diff(bottom) <= 1, "off-centre y at t={t}");
            // Monotonically non-shrinking as t grows.
            assert!(r.width >= prev_w, "width shrank at t={t}");
            prev_w = r.width;
        }
    }

    #[test]
    fn zoom_rect_degenerate_full_does_not_panic() {
        // A zero-width/height full must not underflow the centring offset.
        let r = zoom_rect(Rect::new(0, 0, 0, 0), 0.0);
        assert_eq!((r.width, r.height), (1, 1));
        assert_eq!((r.x, r.y), (0, 0));
    }

    #[test]
    fn lerp_endpoints_and_midpoint() {
        let from = Color::Rgb(16, 16, 20);
        let to = Color::Rgb(96, 165, 250);
        // t = 0 → from, t = 1 → to (exactly — the identity the fade relies on).
        assert_eq!(lerp_color(from, to, 0.0), from);
        assert_eq!(lerp_color(from, to, 1.0), to);
        // Midpoint is the rounded component-wise average: R 16→96 ⇒ 56, G 16→165
        // ⇒ 90.5 rounds to 91, B 20→250 ⇒ 135.
        assert_eq!(lerp_color(from, to, 0.5), Color::Rgb(56, 91, 135));
    }

    #[test]
    fn lerp_bg_to_color_at_one_is_identity() {
        // The core invariant: the final frame of a fade equals the normal render.
        let bg = Color::Rgb(16, 16, 20);
        for c in [
            Color::Rgb(96, 165, 250),
            Color::Rgb(0, 0, 0),
            Color::Rgb(255, 255, 255),
            Color::Rgb(120, 120, 132),
        ] {
            assert_eq!(lerp_color(bg, c, 1.0), c);
        }
    }

    #[test]
    fn lerp_clamps_and_falls_back_for_non_rgb() {
        let from = Color::Rgb(0, 0, 0);
        let to = Color::Rgb(200, 200, 200);
        // Out-of-range t is clamped, so it can never overshoot the endpoints.
        assert_eq!(lerp_color(from, to, -1.0), from);
        assert_eq!(lerp_color(from, to, 5.0), to);
        // A non-Rgb input resolves to the destination (still exact at t = 1).
        assert_eq!(lerp_color(Color::Reset, to, 0.3), to);
    }
}
