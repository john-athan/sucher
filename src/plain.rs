// Non-interactive one-shot renderer. Used when stdout is not a tty (piped),
// or with --plain. Emits the kitty text-sizing protocol for big headings when
// the terminal supports it (runtime-probed), otherwise bold+color fallback.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::process::Command;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";

fn fg(r: u8, g: u8, b: u8) -> String {
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn sized(scale: u8, text: &str) -> String {
    format!("\x1b]66;s={scale};{text}\x1b\\")
}

fn link_open(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}
const LINK_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Runtime probe: does this terminal actually render the text-sizing protocol?
/// Print a scale-2 char, ask for cursor column, check it advanced 2 cells.
pub fn supports_sizing() -> bool {
    if env::var_os("VELLUM_NO_SIZING").is_some() {
        return false;
    }
    let mut stdout = io::stdout();
    if !stdout.is_terminal() || !io::stdin().is_terminal() {
        return false;
    }
    let saved = match Command::new("stty").arg("-g").output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return false,
    };
    let _ = Command::new("stty")
        .args(["-icanon", "-echo", "min", "0", "time", "3"])
        .status();

    let _ = stdout.write_all(b"\r\x1b]66;s=2;A\x1b\\\x1b[6n");
    let _ = stdout.flush();

    let mut buf = Vec::new();
    let mut chunk = [0u8; 32];
    let mut stdin = io::stdin();
    while buf.len() < 64 {
        match stdin.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'R') {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let _ = stdout.write_all(b"\r\x1b[K");
    let _ = stdout.flush();
    let _ = Command::new("stty").arg(&saved).status();

    parse_cpr_col(&buf).map_or(false, |col| col >= 3)
}

fn parse_cpr_col(buf: &[u8]) -> Option<u32> {
    let r = buf.iter().position(|&b| b == b'R')?;
    let semi = buf[..r].iter().rposition(|&b| b == b';')?;
    std::str::from_utf8(&buf[semi + 1..r]).ok()?.parse().ok()
}

struct Renderer {
    out: String,
    sizing: bool,
    heading_scale: Option<u8>,
    heading_buf: String,
    in_code_block: bool,
}

impl Renderer {
    fn scale_for(level: HeadingLevel) -> u8 {
        match level {
            HeadingLevel::H1 => 3,
            HeadingLevel::H2 => 2,
            _ => 1,
        }
    }

    fn flush_heading(&mut self) {
        let scale = self.heading_scale.take().unwrap_or(1);
        let text = std::mem::take(&mut self.heading_buf);
        self.out.push_str(&fg(125, 211, 252));
        self.out.push_str(BOLD);
        if self.sizing && scale > 1 {
            self.out.push_str(&sized(scale, &text));
        } else {
            self.out.push_str(&text);
        }
        self.out.push_str(RESET);
        self.out.push_str("\n\n");
    }

    fn run(&mut self, parser: Parser) {
        for ev in parser {
            match ev {
                Event::Start(Tag::Heading { level, .. }) => {
                    self.heading_scale = Some(Self::scale_for(level));
                    self.heading_buf.clear();
                }
                Event::End(TagEnd::Heading(_)) => self.flush_heading(),
                Event::Start(Tag::Strong) => self.out.push_str(BOLD),
                Event::End(TagEnd::Strong) => self.out.push_str(RESET),
                Event::Start(Tag::Emphasis) => self.out.push_str(ITALIC),
                Event::End(TagEnd::Emphasis) => self.out.push_str(RESET),
                Event::Start(Tag::Link { dest_url, .. }) => {
                    self.out.push_str(&link_open(&dest_url));
                    self.out.push_str(&fg(96, 165, 250));
                    self.out.push_str(UNDERLINE);
                }
                Event::End(TagEnd::Link) => {
                    self.out.push_str(RESET);
                    self.out.push_str(LINK_CLOSE);
                }
                Event::Start(Tag::CodeBlock(_)) => {
                    self.in_code_block = true;
                    self.out.push_str(&fg(110, 231, 183));
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.in_code_block = false;
                    self.out.push_str(RESET);
                    self.out.push('\n');
                }
                Event::Code(t) => {
                    self.out.push_str(&fg(252, 211, 77));
                    self.out.push_str(&t);
                    self.out.push_str(RESET);
                }
                Event::Start(Tag::Item) => self.out.push_str("  • "),
                Event::End(TagEnd::Item) => self.out.push('\n'),
                Event::End(TagEnd::Paragraph) => self.out.push_str("\n\n"),
                Event::Rule => self.out.push_str(&format!(
                    "{}{}{}\n\n",
                    fg(82, 82, 91),
                    "─".repeat(48),
                    RESET
                )),
                Event::Text(t) => {
                    if self.heading_scale.is_some() {
                        self.heading_buf.push_str(&t);
                    } else if self.in_code_block {
                        for line in t.lines() {
                            self.out.push_str("    ");
                            self.out.push_str(line);
                            self.out.push('\n');
                        }
                    } else {
                        self.out.push_str(&t);
                    }
                }
                Event::SoftBreak => self.out.push(' '),
                Event::HardBreak => self.out.push('\n'),
                _ => {}
            }
        }
    }
}

pub fn render(src: &str) -> String {
    let sizing = supports_sizing();
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let mut r = Renderer {
        out: String::new(),
        sizing,
        heading_scale: None,
        heading_buf: String::new(),
        in_code_block: false,
    };
    r.run(Parser::new_ext(src, opts));
    r.out
}
