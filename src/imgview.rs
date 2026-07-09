// Image viewer: decode any format the `image` crate supports and show it via
// the terminal graphics protocol. Animated GIFs loop in place (ADR 0005 D1):
// the pane holds their frames and the main loop ticks them; every other image
// is a one-frame animation that never self-advances (no idle churn).

use crate::anim::{self, ease_out_cubic, zoom_rect, Anim};
use crate::media::{self, Frame, ImagePane};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use image::DynamicImage;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame as RtFrame};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// While an animation is on screen we poll on this short cadence so a frame
/// whose delay has elapsed is drawn promptly. Stills keep the 1 s idle poll.
const ANIM_POLL: Duration = Duration::from_millis(50);
const IDLE_POLL: Duration = Duration::from_millis(1000);

pub fn run(title: String, path: String) -> io::Result<()> {
    // Animated GIF? Decode its frames and loop them. `decode_frames` returns
    // None for a non-animated / oversized / undecodable GIF, so we fall through
    // to the ordinary single-image decode — a static first frame in that case.
    if is_gif(&path) {
        if let Some(frames) = media::decode_frames(Path::new(&path)) {
            return show_frames(title, frames);
        }
    }
    // Explicit pixel limits (ADR 0009): a tiny file claiming enormous dimensions
    // would otherwise force a huge allocation on decode. Set the per-axis bound
    // before decoding, keeping `image`'s default allocation ceiling.
    let img = match image::ImageReader::open(&path).map(|mut r| {
        r.limits(crate::util::image_limits());
        r.decode()
    }) {
        Ok(Ok(img)) => img,
        Ok(Err(e)) => {
            eprintln!("sucher: {path}: {e}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("sucher: {path}: {e}");
            return Ok(());
        }
    };
    show(title, img)
}

fn is_gif(path: &str) -> bool {
    Path::new(path)
        .extension()
        .map(|e| e.eq_ignore_ascii_case("gif"))
        .unwrap_or(false)
}

/// Display an already-decoded still image interactively. Shared by the image
/// viewer and by formats that surface an embedded raster (e.g. Keynote previews).
pub fn show(title: String, img: DynamicImage) -> io::Result<()> {
    let (w, h) = (img.width(), img.height());
    let mut pane = ImagePane::new()?; // probe graphics before taking the screen
    pane.set(img);
    run_pane(title, pane, w, h)
}

/// Display a decoded animation interactively, looping in place. The status
/// line's dimensions come from the first frame (all GIF frames share a canvas).
fn show_frames(title: String, frames: Vec<Frame>) -> io::Result<()> {
    let (w, h) = (frames[0].img.width(), frames[0].img.height());
    let mut pane = ImagePane::new()?;
    pane.set_animation(frames);
    run_pane(title, pane, w, h)
}

/// Take over the terminal and run the shared loop for a prepared pane (still or
/// animated). One entry point so both paths share the exact same init/restore.
fn run_pane(title: String, mut pane: ImagePane, w: u32, h: u32) -> io::Result<()> {
    let mut term = ratatui::init();
    let res = main_loop(&mut term, &mut pane, &title, w, h);
    ratatui::restore();
    res
}

fn main_loop(
    term: &mut DefaultTerminal,
    pane: &mut ImagePane,
    title: &str,
    w: u32,
    h: u32,
) -> io::Result<()> {
    // Full-view OPEN: zoom the picture up from a small centred box to full before
    // the static/animated display begins (ADR 0006 D3). Gated on the global
    // toggle, so `animate = false` skips it and the viewer shows the image at once
    // exactly as it always has. For a GIF this grows frame 0 in place — we do not
    // tick during the intro; the main loop below resumes ticking as usual.
    if anim::enabled() {
        zoom_in(term, pane, title, w, h)?;
    }

    // Only a genuine multi-frame pane animates; a still uses the idle cadence and
    // never ticks, so a static image costs zero CPU while displayed.
    let animated = pane.is_animated();
    let poll = if animated { ANIM_POLL } else { IDLE_POLL };
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| render(f, pane, title, w, h))?;
            dirty = false;
        }
        if event::poll(poll)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        // Full-view CLOSE: mirror the intro, shrinking the current
                        // frame back down, then exit (ADR 0006 D3). Teardown is
                        // unchanged — `run_pane` still calls `ratatui::restore()`
                        // after we return. `animate = false` exits immediately.
                        if anim::enabled() {
                            zoom_out(term, pane, title, w, h)?;
                        }
                        return Ok(());
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        } else if animated {
            // Poll timed out with no input: advance the animation. `tick` redraws
            // only on a real frame change, so we redraw exactly when the picture
            // moved. (Stills never take this arm — `animated` is false.)
            if pane.tick(Instant::now()) {
                dirty = true;
            }
        }
    }
}

/// The static display frame: the picture filling the pane area with the status
/// line split off the bottom. Implemented as the `t = 1` case of [`draw_zoom`],
/// which guarantees the intro's settle frame is pixel-identical to this one —
/// `zoom_rect(area, 1.0) == area` — so there is no jump when the zoom hands off.
fn render(f: &mut RtFrame, pane: &mut ImagePane, title: &str, w: u32, h: u32) {
    draw_zoom(f, pane, title, w, h, 1.0);
}

/// Draw one frame of the viewer with the picture scaled to `t` (0 → small centred
/// box, 1 → full). The layout is identical for every `t`: the pane occupies all
/// but the bottom row, the status line sits on that row, and the image is rendered
/// into `zoom_rect` of the pane area. `term.draw` clears the buffer each frame, so
/// the shrinking/growing picture leaves no trail on the surrounding cells.
fn draw_zoom(f: &mut RtFrame, pane: &mut ImagePane, title: &str, w: u32, h: u32, t: f32) {
    let area = f.area();
    let chunks = Layout::default()
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    pane.render(f, zoom_rect(chunks[0], t));
    status(f, chunks[1], &format!(" {title}   {w}×{h}px   [q] quit"));
}

// The open/close zooms are the *graphics* animation path of ADR 0006: every frame
// re-encodes the image to the terminal's graphics protocol and pushes it over the
// PTY, which for a non-trivial image routinely blows the 8.3 ms/120 Hz budget. So
// unlike the cheap cell-fade in `dir.rs`, these run encode/transmit-bound and the
// `SUCHER_ANIM_STATS` FPS for `open-zoom`/`close-zoom` is expected to sit well
// below the folder-fade's — the honest proof that the graphics path cannot hit
// 120 Hz for general images. We add **no** artificial cap beyond the ~4 ms poll:
// the zoom is time-based (fixed duration), so a fast terminal simply emits more
// frames and a slow one fewer, over the same wall-clock span. Intermediate frames
// are cheaper by construction — a smaller `zoom_rect` means a smaller image to
// encode — which is a bonus, not a target.

/// Duration of the open zoom. Slightly longer than the close so the reveal reads
/// as deliberate while the dismissal feels snappy.
const ZOOM_IN_DUR: Duration = Duration::from_millis(150);
/// Duration of the close zoom.
const ZOOM_OUT_DUR: Duration = Duration::from_millis(120);
/// Poll cadence during a zoom: short enough to emit as fast as the pipeline allows
/// (≤ ~250 fps), yet still a blocking poll so a keypress ends the motion at once.
const ZOOM_POLL: Duration = Duration::from_millis(4);

/// Grow the picture from a small centred box to full over [`ZOOM_IN_DUR`], eased.
/// Time-based via [`Anim`], so the duration is constant regardless of achieved
/// FPS. A keypress during the intro ends it immediately and is **left unread**, so
/// the main loop that follows reads and handles it normally (ADR 0006 D2 — motion
/// never adds latency). Records the achieved emission FPS for the stats sink.
fn zoom_in(
    term: &mut DefaultTerminal,
    pane: &mut ImagePane,
    title: &str,
    w: u32,
    h: u32,
) -> io::Result<()> {
    let anim = Anim::new(Instant::now(), ZOOM_IN_DUR);
    let mut frames = 0u32;
    loop {
        let now = Instant::now();
        if anim.done(now) {
            break;
        }
        // Any key ends the intro. Leave it in the queue: the main loop reads it on
        // its first pass, so a `q` pressed mid-intro still quits (via the outro).
        if event::poll(ZOOM_POLL)? {
            break;
        }
        let t = ease_out_cubic(anim.progress(now));
        term.draw(|f| draw_zoom(f, pane, title, w, h, t))?;
        frames += 1;
    }
    anim::record("open-zoom", frames, anim.elapsed(Instant::now()));
    Ok(())
}

/// Shrink the current frame back down over [`ZOOM_OUT_DUR`], the mirror of the
/// intro (`t` runs 1 → 0). A second keypress during the outro ends it at once and
/// is consumed so it does not leak to the shell after we exit. Records the
/// achieved emission FPS for the stats sink.
fn zoom_out(
    term: &mut DefaultTerminal,
    pane: &mut ImagePane,
    title: &str,
    w: u32,
    h: u32,
) -> io::Result<()> {
    let anim = Anim::new(Instant::now(), ZOOM_OUT_DUR);
    let mut frames = 0u32;
    loop {
        let now = Instant::now();
        if anim.done(now) {
            break;
        }
        // A second key ends the close immediately; consume it — we're exiting, so
        // it must not linger for whatever runs after us.
        if event::poll(ZOOM_POLL)? {
            let _ = event::read()?;
            break;
        }
        let t = 1.0 - ease_out_cubic(anim.progress(now));
        term.draw(|f| draw_zoom(f, pane, title, w, h, t))?;
        frames += 1;
    }
    anim::record("close-zoom", frames, anim.elapsed(Instant::now()));
    Ok(())
}

fn status(f: &mut RtFrame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Line::from(text.to_string()))
            .style(Style::default().fg(Color::Rgb(140, 140, 150))),
        area,
    );
}
