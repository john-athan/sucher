// Image viewer: decode any format the `image` crate supports and show it via
// the terminal graphics protocol.

use crate::media::ImagePane;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};
use std::io;
use std::time::Duration;

pub fn run(title: String, path: String) -> io::Result<()> {
    let img = match image::ImageReader::open(&path).and_then(|r| Ok(r.decode())) {
        Ok(Ok(img)) => img,
        Ok(Err(e)) => {
            eprintln!("vellum: {path}: {e}");
            return Ok(());
        }
        Err(e) => {
            eprintln!("vellum: {path}: {e}");
            return Ok(());
        }
    };
    let (w, h) = (img.width(), img.height());

    // Probe graphics protocol before taking over the screen.
    let mut pane = ImagePane::new()?;
    pane.set(img);

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
    let mut dirty = true;
    loop {
        if dirty {
            term.draw(|f| render(f, pane, title, w, h))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(1000))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(());
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

fn render(f: &mut Frame, pane: &mut ImagePane, title: &str, w: u32, h: u32) {
    let area = f.area();
    let chunks = Layout::default()
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    pane.render(f, chunks[0]);
    status(f, chunks[1], &format!(" {title}   {w}×{h}px   [q] quit"));
}

fn status(f: &mut Frame, area: Rect, text: &str) {
    f.render_widget(
        Paragraph::new(Line::from(text.to_string()))
            .style(Style::default().fg(Color::Rgb(140, 140, 150))),
        area,
    );
}
