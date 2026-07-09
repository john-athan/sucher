// Image viewer: decode any format the `image` crate supports and show it via
// the terminal graphics protocol. Animated GIFs loop in place (ADR 0005 D1):
// the pane holds their frames and the main loop ticks them; every other image
// is a one-frame animation that never self-advances (no idle churn).

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
    let img = match image::ImageReader::open(&path).map(|r| r.decode()) {
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

fn render(f: &mut RtFrame, pane: &mut ImagePane, title: &str, w: u32, h: u32) {
    let area = f.area();
    let chunks = Layout::default()
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    pane.render(f, chunks[0]);
    status(f, chunks[1], &format!(" {title}   {w}×{h}px   [q] quit"));
}

fn status(f: &mut RtFrame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Line::from(text.to_string()))
            .style(Style::default().fg(Color::Rgb(140, 140, 150))),
        area,
    );
}
