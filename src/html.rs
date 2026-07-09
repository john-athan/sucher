// HTML -> markdown (ADR 0008). Real-world HTML is not well-formed XML, so we
// parse it with a browser-grade HTML5 parser (html5ever + rcdom) and walk the
// DOM, emitting the same markdown vocabulary the rest of the app renders
// (headings, bold/italic, code, links, lists, blockquotes, rules, tables) so the
// existing markdown layout/TUI shows it — no new UI (mirrors docx.rs).
//
// The reducer is a PURE `fn parse(&str) -> String`, unit-tested with inline HTML
// literals; `to_markdown` is the thin IO wrapper.

use html5ever::parse_document;
use html5ever::tendril::TendrilSink;
use markup5ever_rcdom::{Handle, NodeData, RcDom};

pub fn to_markdown(path: &str) -> Result<String, String> {
    // Byte-capped read (ADR 0009): a multi-GB HTML file is parsed merely by
    // scrolling onto it in the browser, so bound the input rather than reading it
    // whole. Past the cap we surface the honest "too large" Err to both preview
    // and interactive open instead of building an unbounded DOM.
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let html = crate::util::read_to_string_capped(file, crate::util::MAX_DECODE_BYTES)?;
    Ok(parse(&html))
}

fn parse(html: &str) -> String {
    let dom = parse_document(RcDom::default(), Default::default()).one(html);
    let mut w = Writer::default();
    w.block(&dom.document);
    normalize(&w.out)
}

/// Lowercased tag name of an element node, else None.
fn tag_of(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.as_ref().to_ascii_lowercase()),
        _ => None,
    }
}

/// An element attribute's value, case-insensitive on the name.
fn attr(node: &Handle, key: &str) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &node.data {
        for a in attrs.borrow().iter() {
            if a.name.local.as_ref().eq_ignore_ascii_case(key) {
                return Some(a.value.to_string());
            }
        }
    }
    None
}

/// Non-content subtrees dropped wholesale (HTML's honest-degradation set).
fn skip(tag: &str) -> bool {
    matches!(
        tag,
        "script" | "style" | "head" | "noscript" | "template" | "title" | "meta" | "link" | "svg"
    )
}

/// Block-level elements that terminate a paragraph. Everything else is inline.
fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "figure"
            | "figcaption"
            | "form"
            | "fieldset"
            | "dl"
            | "dt"
            | "dd"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "table"
            | "blockquote"
            | "pre"
            | "hr"
            | "html"
            | "body"
    )
}

/// Recursion budget. `parse` must be total on arbitrary input; html5ever builds
/// an arbitrarily deep DOM for auto-nesting tags (`<div>`, `<span>`, `<b>`, …), so
/// every tree walk is depth-guarded to keep a pathological file from overflowing
/// the stack. Deeper subtrees are truncated, not rendered — honest degradation.
const MAX_DEPTH: usize = 500;

#[derive(Default)]
struct Writer {
    out: String,
    depth: std::cell::Cell<usize>,
}

impl Writer {
    /// Enter one recursion level; returns false (bail) past [`MAX_DEPTH`]. Every
    /// recursive walker pairs a true result with a later [`Self::leave`].
    fn enter(&self) -> bool {
        let d = self.depth.get();
        if d >= MAX_DEPTH {
            return false;
        }
        self.depth.set(d + 1);
        true
    }

    fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }

    /// Walk a container's children, buffering runs of inline content into a
    /// paragraph and flushing it whenever a block element intervenes.
    fn block(&mut self, node: &Handle) {
        if !self.enter() {
            return;
        }
        let mut para = String::new();
        for child in node.children.borrow().iter() {
            match &child.data {
                NodeData::Text { contents } => {
                    let t = collapse_ws(&contents.borrow());
                    if !t.trim().is_empty() {
                        para.push_str(&t);
                    }
                }
                NodeData::Element { .. } => {
                    let tag = tag_of(child).unwrap();
                    if skip(&tag) {
                        continue;
                    }
                    if is_block(&tag) {
                        self.flush_para(&mut para);
                        self.block_element(&tag, child);
                    } else {
                        para.push_str(&self.inline_node(child));
                    }
                }
                _ => {}
            }
        }
        self.flush_para(&mut para);
        self.leave();
    }

    fn flush_para(&mut self, para: &mut String) {
        let t = para.trim();
        if !t.is_empty() {
            self.out.push_str(t);
            self.out.push_str("\n\n");
        }
        para.clear();
    }

    fn block_element(&mut self, tag: &str, node: &Handle) {
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let lvl = tag[1..].parse::<usize>().unwrap_or(1);
                let text = self.inline(node);
                let text = text.trim();
                if !text.is_empty() {
                    self.out
                        .push_str(&format!("{} {text}\n\n", "#".repeat(lvl)));
                }
            }
            "hr" => self.out.push_str("---\n\n"),
            "ul" => self.list(node, false, 0),
            "ol" => self.list(node, true, 0),
            "pre" => self.pre(node),
            "blockquote" => self.blockquote(node),
            "table" => self.table(node),
            // div / section / body / html / nav / p / dt / dd … — transparent
            // containers: recurse and let their inline/block children sort out.
            _ => self.block(node),
        }
    }

    /// Inline markdown for a node's subtree. Block structures (lists, tables,
    /// pre, blockquote, rules) are not inline and are skipped here — the block
    /// walker handles them — so this is safe to call on an `<li>` with a nested
    /// list.
    fn inline(&self, node: &Handle) -> String {
        let mut s = String::new();
        for child in node.children.borrow().iter() {
            s.push_str(&self.inline_node(child));
        }
        s
    }

    /// Inline markdown for a single node, applying that node's own formatting
    /// (`<b>` → `**…**`, `<a>` → `[…](…)`). Shared by [`Self::inline`] and by the
    /// block walker when an inline element sits directly in a container.
    fn inline_node(&self, child: &Handle) -> String {
        match &child.data {
            NodeData::Text { contents } => collapse_ws(&contents.borrow()),
            NodeData::Element { .. } => {
                if !self.enter() {
                    return String::new();
                }
                let tag = tag_of(child).unwrap();
                let out = self.inline_element(&tag, child);
                self.leave();
                out
            }
            _ => String::new(),
        }
    }

    fn inline_element(&self, tag: &str, child: &Handle) -> String {
        match tag {
            _ if skip(tag) => String::new(),
            "ul" | "ol" | "table" | "pre" | "blockquote" | "hr" => String::new(),
            "strong" | "b" => emphasize(&self.inline(child), "**"),
            "em" | "i" => emphasize(&self.inline(child), "*"),
            "code" => code_span(&self.inline(child)),
            // Backslash line break survives `normalize`'s per-line trim (two
            // trailing spaces would not); both renderers treat it as a HardBreak.
            "br" => "\\\n".to_string(),
            "a" => {
                let inner = self.inline(child);
                let core = inner.trim();
                match attr(child, "href") {
                    Some(h) if !core.is_empty() && !h.is_empty() => {
                        format!("[{core}]({})", link_dest(&h))
                    }
                    _ => inner,
                }
            }
            "img" => match attr(child, "src") {
                Some(src) => {
                    let alt = attr(child, "alt").unwrap_or_default();
                    format!("![{alt}]({})", link_dest(&src))
                }
                None => String::new(),
            },
            // span / small / sup / label … — transparent inline.
            _ => self.inline(child),
        }
    }

    fn list(&mut self, node: &Handle, ordered: bool, depth: usize) {
        let indent = "  ".repeat(depth);
        let mut i = 1;
        for child in node.children.borrow().iter() {
            if tag_of(child).as_deref() != Some("li") {
                continue;
            }
            let marker = if ordered {
                format!("{i}. ")
            } else {
                "- ".to_string()
            };
            i += 1;
            let text = self.inline(child);
            self.out
                .push_str(&format!("{indent}{marker}{}\n", text.trim()));
            // Block children that `inline` doesn't flatten (nested lists, tables,
            // code, quotes) are emitted after the item line so their content is
            // preserved rather than dropped. Nested lists indent one level.
            for gc in child.children.borrow().iter() {
                match tag_of(gc).as_deref() {
                    Some("ul") => self.list(gc, false, depth + 1),
                    Some("ol") => self.list(gc, true, depth + 1),
                    Some("table") => self.table(gc),
                    Some("pre") => self.pre(gc),
                    Some("blockquote") => self.blockquote(gc),
                    _ => {}
                }
            }
        }
        if depth == 0 {
            self.out.push('\n');
        }
    }

    fn pre(&mut self, node: &Handle) {
        let text = raw_text(node, self.depth.get());
        let text = text.trim_matches('\n');
        self.out.push_str("```\n");
        self.out.push_str(text);
        self.out.push_str("\n```\n\n");
    }

    fn blockquote(&mut self, node: &Handle) {
        let mut sub = Writer::default();
        // Carry the recursion budget so deeply nested <blockquote> can't overflow
        // the stack by resetting the counter each level.
        sub.depth.set(self.depth.get());
        sub.block(node);
        let inner = normalize(&sub.out);
        for line in inner.lines() {
            if line.is_empty() {
                self.out.push_str(">\n");
            } else {
                self.out.push_str(&format!("> {line}\n"));
            }
        }
        self.out.push('\n');
    }

    fn table(&mut self, node: &Handle) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        self.collect_rows(node, &mut rows);
        if rows.is_empty() {
            return;
        }
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let cell = |row: &[String], i: usize| row.get(i).cloned().unwrap_or_default();
        let header: Vec<String> = (0..ncols).map(|i| cell(&rows[0], i)).collect();
        self.out.push_str(&format!("| {} |\n", header.join(" | ")));
        self.out.push_str(&format!("|{}\n", " --- |".repeat(ncols)));
        for row in &rows[1..] {
            let cells: Vec<String> = (0..ncols).map(|i| cell(row, i)).collect();
            self.out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        self.out.push('\n');
    }

    /// Recursively collect `<tr>` rows (through thead/tbody/tfoot), each row a
    /// list of cell texts with markdown table specials escaped.
    fn collect_rows(&self, node: &Handle, rows: &mut Vec<Vec<String>>) {
        if !self.enter() {
            return;
        }
        for child in node.children.borrow().iter() {
            match tag_of(child).as_deref() {
                Some("tr") => {
                    let mut cells = Vec::new();
                    for c in child.children.borrow().iter() {
                        if matches!(tag_of(c).as_deref(), Some("td") | Some("th")) {
                            cells.push(
                                self.inline(c)
                                    .trim()
                                    .replace('|', "\\|")
                                    .replace('\n', " "),
                            );
                        }
                    }
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                }
                Some(_) => self.collect_rows(child, rows),
                None => {}
            }
        }
        self.leave();
    }
}

/// Collapse each run of ASCII/Unicode whitespace to a single space, preserving
/// leading/trailing spaces (HTML's whitespace model) so inline runs join cleanly.
fn collapse_ws(s: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out
}

/// Wrap `inner` in emphasis markers, hoisting any boundary whitespace *outside*
/// the markers so the result stays valid markdown (`** word **` is not emphasis).
fn emphasize(inner: &str, mark: &str) -> String {
    let core = inner.trim();
    if core.is_empty() {
        return inner.to_string();
    }
    let lead = if inner.starts_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    let trail = if inner.ends_with(char::is_whitespace) {
        " "
    } else {
        ""
    };
    format!("{lead}{mark}{core}{mark}{trail}")
}

/// Inline `<code>` → backtick span; double-backtick fence when the text itself
/// contains a backtick.
fn code_span(inner: &str) -> String {
    let core = inner.trim();
    if core.is_empty() {
        return String::new();
    }
    if core.contains('`') {
        format!("`` {core} ``")
    } else {
        format!("`{core}`")
    }
}

/// A link/image destination made markdown-safe: URLs containing spaces or
/// parentheses (which would terminate `(...)` early) are wrapped in angle
/// brackets, with any literal `<`/`>` percent-encoded.
fn link_dest(url: &str) -> String {
    if url.contains([' ', '(', ')']) {
        format!("<{}>", url.replace('<', "%3C").replace('>', "%3E"))
    } else {
        url.to_string()
    }
}

/// All descendant text with whitespace preserved (for `<pre>`), skipping
/// non-content subtrees. Depth-guarded like the other walkers.
fn raw_text(node: &Handle, depth: usize) -> String {
    if depth >= MAX_DEPTH {
        return String::new();
    }
    let mut s = String::new();
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => s.push_str(&contents.borrow()),
            NodeData::Element { .. } => {
                if !skip(&tag_of(child).unwrap()) {
                    s.push_str(&raw_text(child, depth + 1));
                }
            }
            _ => {}
        }
    }
    s
}

/// Collapse runs of blank lines to one and trim leading blanks / trailing
/// whitespace, so the emitted markdown has clean paragraph spacing. Fenced code
/// blocks (our emitter always fences with a bare ```` ``` ```` line) pass through
/// verbatim — their blank lines and trailing spaces are significant.
fn normalize(s: &str) -> String {
    let mut out = String::new();
    let mut blanks = 0usize;
    let mut in_fence = false;
    for line in s.lines() {
        if line == "```" {
            in_fence = !in_fence;
            blanks = 0;
            out.push_str("```\n");
        } else if in_fence {
            out.push_str(line);
            out.push('\n');
        } else if line.trim().is_empty() {
            blanks += 1;
            if blanks <= 1 {
                out.push('\n');
            }
        } else {
            blanks = 0;
            out.push_str(line.trim_end());
            out.push('\n');
        }
    }
    while out.starts_with('\n') {
        out.remove(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parse, MAX_DEPTH};

    const DOC: &str = r#"<!DOCTYPE html>
<html><head><title>ignored</title><style>body{color:red}</style></head>
<body>
  <h1>Title Here</h1>
  <p>A <strong>bold</strong> and <em>italic</em> and <code>code()</code> line
     with a <a href="https://x.test">link</a>.</p>
  <ul><li>item one</li><li>item two<ul><li>nested</li></ul></li></ul>
  <ol><li>first</li><li>second</li></ol>
  <blockquote><p>quoted line</p></blockquote>
  <pre><code>let x = 1;
let y = 2;</code></pre>
  <hr>
  <table>
    <tr><th>Name</th><th>Age</th></tr>
    <tr><td>Ada</td><td>5</td></tr>
  </table>
  <script>console.log('dropped')</script>
</body></html>"#;

    #[test]
    fn converts_structure() {
        let md = parse(DOC);
        assert!(md.contains("# Title Here"), "heading: {md}");
        assert!(md.contains("**bold**"), "bold: {md}");
        assert!(md.contains("*italic*"), "italic: {md}");
        assert!(md.contains("`code()`"), "code span: {md}");
        assert!(md.contains("[link](https://x.test)"), "link: {md}");
        assert!(md.contains("- item one"), "ul: {md}");
        assert!(md.contains("  - nested"), "nested list indent: {md}");
        assert!(md.contains("1. first"), "ol: {md}");
        assert!(md.contains("2. second"), "ol numbering: {md}");
        assert!(md.contains("> quoted line"), "blockquote: {md}");
        assert!(md.contains("```\nlet x = 1;\nlet y = 2;\n```"), "pre: {md}");
        assert!(md.contains("---"), "rule: {md}");
        assert!(md.contains("| Name | Age |"), "table header: {md}");
        assert!(md.contains("| Ada | 5 |"), "table row: {md}");
    }

    #[test]
    fn drops_scripts_and_styles() {
        let md = parse(DOC);
        assert!(!md.contains("dropped"), "script leaked: {md}");
        assert!(!md.contains("color:red"), "style leaked: {md}");
        assert!(!md.contains("ignored"), "title leaked: {md}");
    }

    #[test]
    fn recovers_from_malformed_markup() {
        // Unclosed tags, missing quotes, stray entity — an XML reader would choke;
        // the HTML5 parser recovers the way a browser does.
        let md = parse("<p>one<p>two<br>three &amp; four <b>bold");
        assert!(md.contains("one"), "{md}");
        assert!(md.contains("two"), "{md}");
        assert!(md.contains("three & four"), "entity/br: {md}");
        assert!(md.contains("**bold**"), "unclosed bold: {md}");
    }

    #[test]
    fn emphasis_boundary_whitespace_is_valid() {
        // Space inside the <b> must move outside the ** markers.
        let md = parse("<p>a<b> b </b>c</p>");
        assert!(md.contains("a **b** c"), "{md}");
    }

    #[test]
    fn plain_document_body_renders() {
        let md = parse("<html><body>just text</body></html>");
        assert_eq!(md.trim(), "just text");
    }

    #[test]
    fn deeply_nested_input_is_bounded_not_crashing() {
        // Pathological auto-nesting well past MAX_DEPTH: parse must be total (no
        // stack overflow) and the walk truncates rather than recursing forever.
        let n = MAX_DEPTH * 4;
        let deep = format!("{}deep{}", "<div>".repeat(n), "</div>".repeat(n));
        assert!(!parse(&deep).contains("deep"), "guard should truncate block nest");
        let inline = format!("<p>{}deep</p>", "<b>".repeat(n));
        assert!(!parse(&inline).contains("deep"), "guard should truncate inline nest");
        let _ = parse(&"<blockquote>".repeat(n)); // must return, not crash
    }

    #[test]
    fn br_becomes_a_hard_break() {
        // The backslash break must survive the normalize pass.
        let md = parse("<p>a<br>b</p>");
        assert!(md.contains("a\\\nb"), "{md:?}");
    }

    #[test]
    fn pre_preserves_blank_lines_and_trailing_space() {
        let md = parse("<pre>a\n\n\nb  \nc</pre>");
        assert!(md.contains("a\n\n\nb  \nc"), "pre corrupted: {md:?}");
    }

    #[test]
    fn link_with_spaces_or_parens_stays_valid() {
        let md = parse(r#"<a href="a b(c)">x</a>"#);
        assert!(md.contains("[x](<a b(c)>)"), "{md}");
    }

    #[test]
    fn block_content_inside_list_item_is_kept() {
        let md = parse("<ul><li>item<blockquote>note</blockquote></li></ul>");
        assert!(md.contains("- item"), "{md}");
        assert!(md.contains("> note"), "lost blockquote in li: {md}");
    }
}
