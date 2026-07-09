// Shared pixel-display pane built on ratatui-image. Queries the terminal once
// for its graphics protocol (kitty / iterm2 / sixel, with unicode-halfblock
// fallback) and renders DynamicImages into a ratatui area. Used by the image,
// PDF, video, and Keynote viewers, plus the directory browser's preview pane.
//
// The pane is animation-aware (ADR 0005 D1): everything it holds is a sequence
// of `Frame`s. A still is simply a one-frame animation — there is no second,
// parallel "animated pane". A one-frame pane never self-advances, so all the
// still callers (PDF/video/Keynote/still image) keep their exact old behaviour
// and cost zero idle work. A multi-frame pane advances on `tick`, looping in
// place; the per-frame protocol re-encode — the real cost — happens only on an
// actual frame change and only while such a pane is on screen.

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, DynamicImage, ImageDecoder};
use ratatui::layout::Rect;
use ratatui::Frame as RtFrame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::time::{Duration, Instant};

/// A single animation frame: the decoded image and how long it shows before the
/// next one. A still image is a one-frame animation with `delay == 0`.
pub struct Frame {
    pub img: DynamicImage,
    pub delay: Duration,
}

/// Hard cap on frames a single animation may hold (ADR 0005 guard). A GIF with
/// more than this degrades to a static first frame (via [`decode_frames`]
/// returning `None`) rather than exhausting memory with a huge frame set.
pub const MAX_FRAMES: usize = 300;

/// Minimum per-frame delay. Many GIFs encode 0 or 10 ms meaning "as fast as the
/// renderer can go"; browsers floor such values, and so do we — both to match
/// their look and to bound how often we re-encode the graphics protocol.
pub const MIN_DELAY: Duration = Duration::from_millis(20);

pub struct ImagePane {
    picker: Picker,
    proto: Option<StatefulProtocol>,
    // The current animation. A still holds exactly one frame; a GIF holds N.
    // `idx` is the visible frame; `last_advance` is when it last changed (set on
    // the first `tick`, so the initial frame gets its full delay before wrapping).
    frames: Vec<Frame>,
    idx: usize,
    last_advance: Option<Instant>,
}

impl ImagePane {
    /// Must be called before entering the alternate screen — it queries the
    /// terminal over stdio.
    pub fn new() -> io::Result<Self> {
        let picker = Picker::from_query_stdio()
            .map_err(|e| io::Error::other(format!("graphics probe failed: {e}")))?;
        Ok(Self {
            picker,
            proto: None,
            frames: Vec::new(),
            idx: 0,
            last_advance: None,
        })
    }

    /// Show a single still image. Stored as a one-frame animation, so `tick` is
    /// a no-op and the pane never self-advances — unchanged semantics for every
    /// existing caller (PDF, video, Keynote, still image).
    pub fn set(&mut self, img: DynamicImage) {
        self.proto = Some(self.picker.new_resize_protocol(img.clone()));
        self.frames = vec![Frame {
            img,
            delay: Duration::ZERO,
        }];
        self.idx = 0;
        self.last_advance = None;
    }

    /// Show a multi-frame animation, starting at frame 0. Empty input is a no-op
    /// (nothing to show); a single frame behaves exactly like [`set`]. Resets the
    /// advance clock so the first frame gets its full delay before wrapping.
    pub fn set_animation(&mut self, frames: Vec<Frame>) {
        if frames.is_empty() {
            return;
        }
        self.proto = Some(self.picker.new_resize_protocol(frames[0].img.clone()));
        self.frames = frames;
        self.idx = 0;
        self.last_advance = None;
    }

    /// Advance the animation if its current frame's delay has elapsed. Returns
    /// `true` when the visible frame changed (the caller should redraw).
    ///
    /// A one-frame pane (any still) always returns `false` — the guarantee that
    /// non-animated content costs zero churn. For a multi-frame pane the first
    /// call only starts the clock (returns `false`); subsequent calls advance
    /// and re-encode the protocol once the delay is up, wrapping to loop.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.frames.len() <= 1 {
            return false;
        }
        let Some(last) = self.last_advance else {
            self.last_advance = Some(now);
            return false;
        };
        if should_advance(
            now.saturating_duration_since(last),
            self.frames[self.idx].delay,
        ) {
            self.idx = (self.idx + 1) % self.frames.len();
            self.last_advance = Some(now);
            self.proto = Some(
                self.picker
                    .new_resize_protocol(self.frames[self.idx].img.clone()),
            );
            true
        } else {
            false
        }
    }

    /// Whether the pane holds a real animation (more than one frame). The caller
    /// uses this to decide whether to tick at all — gating so a still never churns.
    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    pub fn render(&mut self, f: &mut RtFrame, area: Rect) {
        if let Some(p) = self.proto.as_mut() {
            let widget = StatefulImage::<StatefulProtocol>::default().resize(Resize::Fit(None));
            f.render_stateful_widget(widget, area, p);
        }
    }
}

/// Whether an animation frame that has been on screen for `elapsed` should give
/// way to the next, given its `delay`. Pure so the pacing rule is unit-tested
/// without a clock or a real GIF (ADR 0001 ethos).
fn should_advance(elapsed: Duration, delay: Duration) -> bool {
    elapsed >= delay
}

/// Convert a GIF frame delay (a `numerator/denominator` in milliseconds, as the
/// `image` crate reports it) to a `Duration`, floored to [`MIN_DELAY`]. Pure and
/// division-by-zero safe. Extracted so the floor is unit-tested off the IO path.
fn frame_delay(numer_ms: u32, denom_ms: u32) -> Duration {
    let ms = if denom_ms == 0 {
        0
    } else {
        u64::from(numer_ms) / u64::from(denom_ms)
    };
    Duration::from_millis(ms).max(MIN_DELAY)
}

/// The one place GIF internals live: decode `path` into animation frames, or
/// `None` so the caller falls back to the existing single-image decode.
///
/// Returns `None` on any decode error, on a non-animated GIF (fewer than 2
/// frames — a one-frame GIF is just a still, handled by the still path), or on a
/// GIF that exceeds [`MAX_FRAMES`] (which degrades to a static first frame rather
/// than blowing memory — ADR 0005 guard). Each frame's delay is floored to
/// [`MIN_DELAY`]. Scope is GIF only for now; animated WebP/APNG would be another
/// branch here, unchanged everywhere else.
pub fn decode_frames(path: &Path) -> Option<Vec<Frame>> {
    let file = File::open(path).ok()?;
    let mut decoder = GifDecoder::new(BufReader::new(file)).ok()?;
    // Pixel limits before decode (ADR 0009): a GIF claiming enormous dimensions
    // must not force a huge per-frame allocation. `MAX_FRAMES` already bounds the
    // frame count; this bounds each frame's canvas. Keep the default alloc ceiling.
    decoder.set_limits(crate::util::image_limits()).ok()?;
    let raw = decoder.into_frames().collect_frames().ok()?;
    // Guard: over the cap → None, so the caller shows a static first frame
    // instead of holding a huge frame set in memory.
    if raw.len() < 2 || raw.len() > MAX_FRAMES {
        return None;
    }
    let frames = raw
        .into_iter()
        .map(|f| {
            let (numer, denom) = f.delay().numer_denom_ms();
            Frame {
                img: DynamicImage::ImageRgba8(f.into_buffer()),
                delay: frame_delay(numer, denom),
            }
        })
        .collect();
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_only_when_delay_elapsed() {
        // Exactly the delay (or more) advances; a hair under does not.
        let delay = Duration::from_millis(50);
        assert!(should_advance(delay, delay));
        assert!(should_advance(Duration::from_millis(51), delay));
        assert!(!should_advance(Duration::from_millis(49), delay));
        // A zero-delay frame (a still) advances immediately — but stills never
        // reach `tick`'s advance path because a one-frame pane returns early.
        assert!(should_advance(Duration::ZERO, Duration::ZERO));
    }

    #[test]
    fn delay_floors_to_min_and_survives_zero_denominator() {
        // Sub-floor values (0 ms, 10 ms) clamp up to MIN_DELAY, matching browsers.
        assert_eq!(frame_delay(0, 100), MIN_DELAY);
        assert_eq!(frame_delay(10, 1), MIN_DELAY);
        // A denominator of zero must not divide-by-zero; it floors to MIN_DELAY.
        assert_eq!(frame_delay(100, 0), MIN_DELAY);
        // Above the floor passes through: 100/1 ms = 100 ms.
        assert_eq!(frame_delay(100, 1), Duration::from_millis(100));
        // Fractional GIF timing (e.g. 1/3 s reported as 1000/3 ms) rounds down.
        assert_eq!(frame_delay(1000, 3), Duration::from_millis(333));
    }
}
