// Markdown -> a navigable document model for the TUI.
//
// Parsing produces "logical lines" (paragraphs, headings, code, rules, table
// rows) plus a TOC and link table, keyed by logical-line index. A width-aware
// `layout` step word-wraps logical lines into display lines — applying
// blockquote gutters and list indentation — and returns a logical->display
// index map so the TOC can jump to the right row.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const SKY: Color = Color::Rgb(125, 211, 252);
const AMBER: Color = Color::Rgb(252, 211, 77);
const MINT: Color = Color::Rgb(110, 231, 183);
const LINK: Color = Color::Rgb(96, 165, 250);
const GRAY: Color = Color::Rgb(110, 110, 122);
const WHITE: Color = Color::Rgb(220, 220, 228);

const CELL_MAX: usize = 28;

#[derive(Clone)]
enum Tok {
    Word(String, Style),
    Space,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    Heading,
    Code,
    Pre, // preformatted, no wrap (tables) — tokens carry their own styles
    Rule,
    Blank,
}

struct LogLine {
    toks: Vec<Tok>,
    kind: Kind,
    indent: u16,
    quote: u8,
}

pub struct TocEntry {
    pub level: u8,
    pub title: String,
    pub line: usize,
}

pub struct LinkRef {
    pub url: String,
    pub text: String,
}

pub struct Rendered {
    lines: Vec<LogLine>,
    pub toc: Vec<TocEntry>,
    pub links: Vec<LinkRef>,
}

#[derive(Default)]
struct Builder {
    lines: Vec<LogLine>,
    toc: Vec<TocEntry>,
    links: Vec<LinkRef>,
    cur: Vec<Tok>,
    strong: u32,
    emph: u32,
    in_heading: Option<u8>,
    heading_title: String,
    link_url: Option<String>,
    link_text: String,
    in_code: bool,
    list_depth: u16,
    quote_depth: u8,
    // table accumulation
    in_table: bool,
    in_cell: bool,
    cur_cell: String,
    cur_row: Vec<String>,
    table: Vec<Vec<String>>,
    header_cols: usize,
}

impl Builder {
    fn style(&self) -> Style {
        if self.in_heading.is_some() {
            return Style::default().fg(SKY).add_modifier(Modifier::BOLD);
        }
        if self.link_url.is_some() {
            return Style::default().fg(LINK).add_modifier(Modifier::UNDERLINED);
        }
        let mut s = Style::default();
        if self.strong > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.emph > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    }

    fn indent(&self) -> u16 {
        self.list_depth.saturating_sub(1) * 2
    }

    fn tokenize(&mut self, text: &str, style: Style) {
        let mut word = String::new();
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !word.is_empty() {
                    self.cur.push(Tok::Word(std::mem::take(&mut word), style));
                }
                if !matches!(self.cur.last(), Some(Tok::Space)) {
                    self.cur.push(Tok::Space);
                }
            } else {
                word.push(ch);
            }
        }
        if !word.is_empty() {
            self.cur.push(Tok::Word(word, style));
        }
    }

    fn finish(&mut self, kind: Kind) {
        if self.cur.is_empty() && kind == Kind::Normal {
            return;
        }
        let toks = std::mem::take(&mut self.cur);
        self.lines.push(LogLine {
            toks,
            kind,
            indent: self.indent(),
            quote: self.quote_depth,
        });
    }

    fn push_raw(&mut self, toks: Vec<Tok>, kind: Kind) {
        self.lines.push(LogLine { toks, kind, indent: 0, quote: self.quote_depth });
    }

    fn blank(&mut self) {
        self.lines.push(LogLine { toks: vec![], kind: Kind::Blank, indent: 0, quote: 0 });
    }

    fn run(&mut self, parser: Parser) {
        for ev in parser {
            // Table cells swallow inline content into a string buffer.
            if self.in_cell {
                match &ev {
                    Event::Text(t) | Event::Code(t) => {
                        self.cur_cell.push_str(t);
                        continue;
                    }
                    Event::End(TagEnd::TableCell) => {
                        self.cur_row.push(std::mem::take(&mut self.cur_cell));
                        self.in_cell = false;
                        continue;
                    }
                    _ => {}
                }
            }

            match ev {
                Event::Start(Tag::Heading { level, .. }) => {
                    self.in_heading = Some(heading_level(level));
                    self.heading_title.clear();
                    self.cur.clear();
                }
                Event::End(TagEnd::Heading(_)) => {
                    let level = self.in_heading.take().unwrap_or(1);
                    self.toc.push(TocEntry {
                        level,
                        title: std::mem::take(&mut self.heading_title),
                        line: self.lines.len(),
                    });
                    self.finish(Kind::Heading);
                    self.blank();
                }

                Event::Start(Tag::Strong) => self.strong += 1,
                Event::End(TagEnd::Strong) => self.strong = self.strong.saturating_sub(1),
                Event::Start(Tag::Emphasis) => self.emph += 1,
                Event::End(TagEnd::Emphasis) => self.emph = self.emph.saturating_sub(1),

                Event::Start(Tag::BlockQuote(_)) => {
                    self.finish(Kind::Normal);
                    self.quote_depth += 1;
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    self.finish(Kind::Normal);
                    self.quote_depth = self.quote_depth.saturating_sub(1);
                }

                Event::Start(Tag::List(_)) => {
                    self.finish(Kind::Normal);
                    self.list_depth += 1;
                }
                Event::End(TagEnd::List(_)) => {
                    self.list_depth = self.list_depth.saturating_sub(1);
                    if self.list_depth == 0 {
                        self.blank();
                    }
                }
                Event::Start(Tag::Item) => {
                    self.cur.push(Tok::Word("•".into(), Style::default().fg(AMBER)));
                    self.cur.push(Tok::Space);
                }
                Event::End(TagEnd::Item) => self.finish(Kind::Normal),

                Event::Start(Tag::Link { dest_url, .. }) => {
                    self.link_url = Some(dest_url.to_string());
                    self.link_text.clear();
                }
                Event::End(TagEnd::Link) => {
                    if let Some(url) = self.link_url.take() {
                        self.links.push(LinkRef {
                            url,
                            text: std::mem::take(&mut self.link_text),
                        });
                    }
                }

                Event::End(TagEnd::Paragraph) => {
                    if self.list_depth > 0 {
                        self.finish(Kind::Normal);
                    } else {
                        self.finish(Kind::Normal);
                        self.blank();
                    }
                }

                Event::Start(Tag::CodeBlock(_)) => {
                    self.finish(Kind::Normal);
                    self.in_code = true;
                }
                Event::End(TagEnd::CodeBlock) => {
                    self.in_code = false;
                    self.blank();
                }

                // ---- tables ----
                Event::Start(Tag::Table(_)) => {
                    self.finish(Kind::Normal);
                    self.in_table = true;
                    self.table.clear();
                    self.header_cols = 0;
                }
                Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                    self.cur_row = Vec::new();
                }
                Event::Start(Tag::TableCell) => {
                    self.in_cell = true;
                    self.cur_cell.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    self.header_cols = self.cur_row.len();
                    self.table.push(std::mem::take(&mut self.cur_row));
                }
                Event::End(TagEnd::TableRow) => {
                    self.table.push(std::mem::take(&mut self.cur_row));
                }
                Event::End(TagEnd::Table) => {
                    self.in_table = false;
                    self.emit_table();
                    self.blank();
                }

                Event::Code(t) => {
                    self.cur.push(Tok::Word(t.to_string(), Style::default().fg(AMBER)));
                    if self.link_url.is_some() {
                        self.link_text.push_str(&t);
                    }
                }

                Event::Rule => self.push_raw(vec![], Kind::Rule),

                Event::Text(t) => {
                    if self.in_heading.is_some() {
                        self.heading_title.push_str(&t);
                        self.tokenize(&t, self.style());
                    } else if self.link_url.is_some() {
                        self.link_text.push_str(&t);
                        self.tokenize(&t, self.style());
                    } else if self.in_code {
                        for line in t.lines() {
                            self.push_raw(
                                vec![Tok::Word(
                                    format!("    {line}"),
                                    Style::default().fg(MINT),
                                )],
                                Kind::Code,
                            );
                        }
                    } else {
                        let st = self.style();
                        self.tokenize(&t, st);
                    }
                }
                Event::SoftBreak => self.cur.push(Tok::Space),
                Event::HardBreak => self.finish(Kind::Normal),
                _ => {}
            }
        }
        self.finish(Kind::Normal);
    }

    fn emit_table(&mut self) {
        let rows = std::mem::take(&mut self.table);
        if rows.is_empty() {
            return;
        }
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        // column widths (clamped)
        let mut widths = vec![0usize; ncols];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count().min(CELL_MAX));
            }
        }

        let border = |l: char, mid: char, r: char| -> String {
            let mut s = String::new();
            s.push(l);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push(if i + 1 == ncols { r } else { mid });
            }
            s
        };
        let data_row = |row: &Vec<String>| -> String {
            let mut s = String::from("│");
            for (i, w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                s.push(' ');
                s.push_str(&pad(cell, *w));
                s.push_str(" │");
            }
            s
        };

        let gray = Style::default().fg(GRAY);
        let white = Style::default().fg(WHITE);
        let push_line = |text: String, style: Style, b: &mut Builder| {
            b.push_raw(vec![Tok::Word(text, style)], Kind::Pre);
        };

        push_line(border('┌', '┬', '┐'), gray, self);
        let has_header = self.header_cols > 0;
        for (idx, row) in rows.iter().enumerate() {
            push_line(data_row(row), white, self);
            if idx == 0 && has_header {
                push_line(border('├', '┼', '┤'), gray, self);
            }
        }
        push_line(border('└', '┴', '┘'), gray, self);
    }
}

fn pad(s: &str, w: usize) -> String {
    let t = truncate(s, w);
    let len = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(len)))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn heading_level(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

impl Rendered {
    pub fn build(src: &str) -> Rendered {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TABLES);
        let mut b = Builder::default();
        b.run(Parser::new_ext(src, opts));
        Rendered { lines: b.lines, toc: b.toc, links: b.links }
    }

    /// Wrap logical lines to `width`, returning display lines, their plain-text
    /// (for search), and a logical->display-index map.
    pub fn layout(&self, width: usize) -> (Vec<Line<'static>>, Vec<String>, Vec<usize>) {
        let width = width.max(8);
        let mut display: Vec<Line<'static>> = Vec::new();
        let mut plain: Vec<String> = Vec::new();
        let mut map: Vec<usize> = Vec::with_capacity(self.lines.len());

        for ll in &self.lines {
            map.push(display.len());
            match ll.kind {
                Kind::Blank => {
                    display.push(Line::default());
                    plain.push(String::new());
                }
                Kind::Rule => {
                    let rule = "─".repeat(width.min(60));
                    plain.push(rule.clone());
                    display.push(Line::from(Span::styled(rule, Style::default().fg(GRAY))));
                }
                Kind::Code | Kind::Pre => {
                    let (line, text) = render_unwrapped(&ll.toks);
                    display.push(line);
                    plain.push(text);
                }
                Kind::Heading => {
                    for (line, text) in wrap(&ll.toks, width) {
                        display.push(line);
                        plain.push(text);
                    }
                }
                Kind::Normal => {
                    // blockquote gutter + list indentation
                    let mut prefix: Vec<Span<'static>> = Vec::new();
                    let mut pwidth = 0usize;
                    let mut pplain = String::new();
                    for _ in 0..ll.quote {
                        prefix.push(Span::styled("│ ", Style::default().fg(GRAY)));
                        pplain.push_str("│ ");
                        pwidth += 2;
                    }
                    if ll.indent > 0 {
                        let pad = " ".repeat(ll.indent as usize);
                        prefix.push(Span::raw(pad.clone()));
                        pplain.push_str(&pad);
                        pwidth += ll.indent as usize;
                    }

                    let avail = width.saturating_sub(pwidth).max(4);
                    let wrapped = wrap(&ll.toks, avail);
                    if wrapped.is_empty() {
                        display.push(Line::default());
                        plain.push(String::new());
                    } else {
                        for (line, text) in wrapped {
                            let mut spans = prefix.clone();
                            spans.extend(line.spans);
                            display.push(Line::from(spans));
                            plain.push(format!("{pplain}{text}"));
                        }
                    }
                }
            }
        }
        (display, plain, map)
    }
}

fn render_unwrapped(toks: &[Tok]) -> (Line<'static>, String) {
    let mut spans = Vec::new();
    let mut text = String::new();
    for t in toks {
        match t {
            Tok::Word(w, st) => {
                text.push_str(w);
                spans.push(Span::styled(w.clone(), *st));
            }
            Tok::Space => {
                text.push(' ');
                spans.push(Span::raw(" "));
            }
        }
    }
    (Line::from(spans), text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_at(src: &str, width: usize) -> Vec<String> {
        Rendered::build(src).layout(width).1
    }

    #[test]
    fn table_renders_grid() {
        let md = "| Key | Val |\n|-----|-----|\n| a | 1 |\n| bb | 22 |\n";
        let lines = plain_at(md, 80);
        let joined = lines.join("\n");
        assert!(joined.contains('┌') && joined.contains('┼') && joined.contains('└'));
        assert!(joined.contains("Key") && joined.contains("bb"));
    }

    #[test]
    fn blockquote_gets_gutter() {
        let lines = plain_at("> quoted text here\n", 80);
        assert!(lines.iter().any(|l| l.starts_with("│ ") && l.contains("quoted")));
    }

    #[test]
    fn nested_list_indents() {
        let md = "- top\n  - nested\n";
        let lines = plain_at(md, 80);
        let nested = lines.iter().find(|l| l.contains("nested")).unwrap();
        assert!(nested.starts_with("  "), "nested item should be indented: {nested:?}");
    }

    #[test]
    fn toc_and_links_collected() {
        let md = "# H1\n\n## H2\n\n[x](http://e.com)\n";
        let r = Rendered::build(md);
        assert_eq!(r.toc.len(), 2);
        assert_eq!(r.links.len(), 1);
        assert_eq!(r.links[0].url, "http://e.com");
    }

    #[test]
    fn long_paragraph_wraps() {
        let md = "word ".repeat(40);
        let lines = plain_at(&md, 20);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
    }
}

fn wrap(toks: &[Tok], width: usize) -> Vec<(Line<'static>, String)> {
    let mut out: Vec<(Line<'static>, String)> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut text = String::new();
    let mut w = 0usize;

    let flush = |spans: &mut Vec<Span<'static>>, text: &mut String, out: &mut Vec<_>| {
        out.push((Line::from(std::mem::take(spans)), std::mem::take(text)));
    };

    for t in toks {
        match t {
            Tok::Space => {
                if w == 0 {
                    continue;
                }
                if w + 1 > width {
                    flush(&mut spans, &mut text, &mut out);
                    w = 0;
                } else {
                    spans.push(Span::raw(" "));
                    text.push(' ');
                    w += 1;
                }
            }
            Tok::Word(word, st) => {
                let len = word.chars().count();
                if w + len > width && w > 0 {
                    flush(&mut spans, &mut text, &mut out);
                    w = 0;
                }
                spans.push(Span::styled(word.clone(), *st));
                text.push_str(word);
                w += len;
            }
        }
    }
    if !spans.is_empty() {
        flush(&mut spans, &mut text, &mut out);
    }
    out
}
