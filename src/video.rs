// Video viewer. No pure-Rust decode, so we drive ffmpeg.
//
// Playback streams raw rgb24 frames from ONE long-lived ffmpeg process (scaled
// to the display, fps-limited) over a pipe, decoded on a background thread that
// paces to real time and keeps only the latest frame. The UI shows whatever is
// current — so when terminal-graphics encoding can't keep up, frames drop
// instead of the whole thing lagging behind. Scrubbing while paused extracts a
// single frame (fast input-seek). There is NO audio.

use crate::media::ImagePane;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use image::{DynamicImage, RgbImage};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PLAY_FPS: f64 = 30.0;
const MAX_PLAY_W: u32 = 1000;

struct Probe {
    w: u32,
    h: u32,
    fps: f64,
    dur: f64,
}

fn ffprobe(path: &str) -> Probe {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "stream=width,height,r_frame_rate",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output();
    let mut p = Probe { w: 1280, h: 720, fps: 30.0, dur: 0.0 };
    if let Ok(o) = out {
        let s = String::from_utf8_lossy(&o.stdout);
        let mut it = s.lines();
        if let Some(w) = it.next().and_then(|v| v.trim().parse().ok()) {
            p.w = w;
        }
        if let Some(h) = it.next().and_then(|v| v.trim().parse().ok()) {
            p.h = h;
        }
        if let Some(fr) = it.next() {
            // r_frame_rate is "num/den"
            let mut parts = fr.trim().split('/');
            if let (Some(n), Some(d)) = (parts.next(), parts.next()) {
                if let (Ok(n), Ok(d)) = (n.parse::<f64>(), d.parse::<f64>()) {
                    if d > 0.0 {
                        p.fps = n / d;
                    }
                }
            }
        }
        if let Some(d) = it.next().and_then(|v| v.trim().parse().ok()) {
            p.dur = d;
        }
    }
    p
}

/// Single-frame extract for paused scrubbing (fast input seek).
fn frame_at(path: &str, t: f64, w: u32, h: u32) -> Result<DynamicImage, String> {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-ss"])
        .arg(format!("{t:.3}"))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-vf"])
        .arg(format!("scale={w}:{h}"))
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() || out.stdout.len() < (w * h * 3) as usize {
        return Err("ffmpeg frame extract failed".into());
    }
    RgbImage::from_raw(w, h, out.stdout[..(w * h * 3) as usize].to_vec())
        .map(DynamicImage::ImageRgb8)
        .ok_or_else(|| "bad frame".into())
}

fn fmt_time(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Even-rounded display height for a given width, preserving aspect.
fn scaled_dims(p: &Probe) -> (u32, u32) {
    let w = (target_w()).min(MAX_PLAY_W).max(160);
    let h = if p.w > 0 {
        ((w as u64 * p.h as u64) / p.w as u64) as u32
    } else {
        (w * 9 / 16) as u32
    };
    (w & !1, h.max(2) & !1)
}

fn target_w() -> u32 {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 => ws.width as u32,
        Ok(ws) => ws.columns as u32 * 8,
        Err(_) => 800,
    }
}

#[derive(Default)]
struct Latest {
    img: Option<DynamicImage>,
    pos: f64,
    gen: u64,
    ended: bool,
}

struct VideoApp {
    title: String,
    path: String,
    pos: f64,
    probe: Probe,
    w: u32,
    h: u32,
    playing: bool,
    pane: ImagePane,
    err: Option<String>,
    shared: Arc<Mutex<Latest>>,
    stop: Arc<AtomicBool>,
    child: Option<Child>,
    last_gen: u64,
}

pub fn run(title: String, path: String) -> std::io::Result<()> {
    let probe = ffprobe(&path);
    let pane = ImagePane::new()?;
    let (w, h) = scaled_dims(&probe);
    let mut app = VideoApp {
        title,
        path,
        pos: 0.0,
        probe,
        w,
        h,
        playing: false,
        pane,
        err: None,
        shared: Arc::new(Mutex::new(Latest::default())),
        stop: Arc::new(AtomicBool::new(false)),
        child: None,
        last_gen: 0,
    };
    app.show_static(); // instant first frame while the stream spins up
    app.start_play(); // auto-play on open
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    app.stop_play();
    ratatui::restore();
    res
}

/// Non-interactive: print metadata via ffprobe.
pub fn dump(path: &str) -> String {
    match Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries",
            "format=duration,size:stream=codec_type,codec_name,width,height,r_frame_rate",
            "-of", "default=noprint_wrappers=1",
        ])
        .arg(path)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => format!("vellum: ffprobe: {}", String::from_utf8_lossy(&o.stderr)),
        Err(e) => format!("vellum: ffprobe: {e}"),
    }
}

impl VideoApp {
    /// Render one frame at the current position (paused / scrub).
    fn show_static(&mut self) {
        match frame_at(&self.path, self.pos, self.w, self.h) {
            Ok(img) => {
                self.pane.set(img);
                self.err = None;
            }
            Err(e) => self.err = Some(e),
        }
    }

    fn start_play(&mut self) {
        self.stop_play();
        self.stop = Arc::new(AtomicBool::new(false));
        self.shared = Arc::new(Mutex::new(Latest::default()));

        let fps = self.probe.fps.min(PLAY_FPS).max(1.0);
        let mut child = match Command::new("ffmpeg")
            .args(["-nostdin", "-ss"])
            .arg(format!("{:.3}", self.pos))
            .arg("-i")
            .arg(&self.path)
            .args(["-an", "-vf"])
            .arg(format!("scale={}:{},fps={fps}", self.w, self.h))
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.err = Some(format!("ffmpeg: {e}"));
                return;
            }
        };
        let stdout = child.stdout.take().unwrap();
        self.child = Some(child);
        self.playing = true;

        let (w, h, pos0) = (self.w, self.h, self.pos);
        let shared = self.shared.clone();
        let stop = self.stop.clone();
        std::thread::spawn(move || decode_loop(stdout, w, h, fps, pos0, shared, stop));
    }

    fn stop_play(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.playing = false;
    }

    fn seek(&mut self, delta: f64) {
        self.pos = (self.pos + delta).clamp(0.0, (self.probe.dur - 0.05).max(0.0));
        if self.playing {
            self.start_play(); // restart stream at new position
        } else {
            self.show_static();
        }
    }

    fn main_loop(&mut self, term: &mut DefaultTerminal) -> std::io::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }

            if self.playing {
                // pull the latest decoded frame, if newer than what we showed
                let mut ended = false;
                let newframe = {
                    let l = self.shared.lock().unwrap();
                    if l.ended {
                        ended = true;
                        None
                    } else if l.gen != self.last_gen {
                        l.img.clone().map(|img| (img, l.pos, l.gen))
                    } else {
                        None
                    }
                };
                if let Some((img, pos, gen)) = newframe {
                    self.pane.set(img);
                    self.pos = pos;
                    self.last_gen = gen;
                    dirty = true;
                } else if ended {
                    self.stop_play();
                    dirty = true;
                }
            }

            let timeout = if self.playing { 16 } else { 1000 };
            if event::poll(Duration::from_millis(timeout))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    dirty = true;
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char(' ') => {
                            if self.playing {
                                self.stop_play();
                            } else {
                                self.start_play();
                            }
                        }
                        KeyCode::Right | KeyCode::Char('l') => self.seek(5.0),
                        KeyCode::Left | KeyCode::Char('h') => self.seek(-5.0),
                        KeyCode::Up | KeyCode::Char('k') => self.seek(30.0),
                        KeyCode::Down | KeyCode::Char('j') => self.seek(-30.0),
                        KeyCode::Char('.') => self.seek(1.0 / PLAY_FPS),
                        KeyCode::Char(',') => self.seek(-1.0 / PLAY_FPS),
                        KeyCode::Char('g') | KeyCode::Home => {
                            self.pos = 0.0;
                            self.seek(0.0);
                        }
                        KeyCode::Char('G') | KeyCode::End => {
                            self.pos = (self.probe.dur - 1.0).max(0.0);
                            self.seek(0.0);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let chunks = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        if let Some(e) = &self.err {
            f.render_widget(
                Paragraph::new(format!("frame error: {e}"))
                    .style(Style::default().fg(Color::Rgb(248, 113, 113))),
                chunks[0],
            );
        } else {
            self.pane.render(f, chunks[0]);
        }
        let state = if self.playing { "▶" } else { "⏸" };
        let status = format!(
            " {}   {} {} / {}   {}×{}@{:.0}fps   [space] play  [←/→] ±5s  [↑/↓] ±30s  [,/.] frame  [q] quit  (no audio)",
            self.title,
            state,
            fmt_time(self.pos),
            fmt_time(self.probe.dur),
            self.w,
            self.h,
            self.probe.fps.min(PLAY_FPS),
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(Color::Rgb(140, 140, 150))),
            chunks[1],
        );
    }
}

impl Drop for VideoApp {
    fn drop(&mut self) {
        self.stop_play();
    }
}

fn decode_loop(
    stdout: std::process::ChildStdout,
    w: u32,
    h: u32,
    fps: f64,
    pos0: f64,
    shared: Arc<Mutex<Latest>>,
    stop: Arc<AtomicBool>,
) {
    let frame_bytes = (w * h * 3) as usize;
    let mut reader = BufReader::with_capacity(frame_bytes.max(1 << 16), stdout);
    let mut buf = vec![0u8; frame_bytes];
    let start = Instant::now();
    let mut i: u64 = 0;
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if reader.read_exact(&mut buf).is_err() {
            if let Ok(mut l) = shared.lock() {
                l.ended = true;
            }
            return;
        }
        // pace to real time
        let target = i as f64 / fps;
        let elapsed = start.elapsed().as_secs_f64();
        if target > elapsed {
            std::thread::sleep(Duration::from_secs_f64(target - elapsed));
        }
        if let Some(img) = RgbImage::from_raw(w, h, buf.clone()) {
            if let Ok(mut l) = shared.lock() {
                l.img = Some(DynamicImage::ImageRgb8(img));
                l.pos = pos0 + target;
                l.gen += 1;
            }
        }
        i += 1;
    }
}

