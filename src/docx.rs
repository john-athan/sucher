// DOCX -> markdown. A .docx is a zip; the body lives in word/document.xml as
// WordprocessingML. We walk it with a streaming XML reader and emit markdown
// (headings, bold/italic, lists, tables) so the existing markdown layout/TUI
// renders it — no new UI needed.

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::io::Read;

/// Embedded raster images from a .docx (`word/media/`), extracted to temp files
/// for the viewer's image gallery. Empty when the document has none.
pub fn media(path: &str) -> Vec<std::path::PathBuf> {
    crate::util::extract_ooxml_media(path, "word/media/")
}

pub fn to_markdown(path: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|_| "not a docx (no word/document.xml)".to_string())?
        .read_to_string(&mut xml)
        .map_err(|e| e.to_string())?;
    Ok(parse(&xml))
}

fn attr_val(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// A boolean toggle property (<w:b/>, <w:i/>) is on unless w:val says otherwise.
fn toggle_on(e: &BytesStart) -> bool {
    match attr_val(e, b"w:val") {
        Some(v) => !matches!(v.as_str(), "0" | "false" | "off"),
        None => true,
    }
}

#[derive(Default)]
struct Parser {
    out: String,
    para: String,
    bold: bool,
    italic: bool,
    in_text: bool,
    style: Option<String>,
    is_list: bool,
    // table state
    in_table: bool,
    in_cell: bool,
    cell: String,
    row: Vec<String>,
    rows: Vec<Vec<String>>,
    first_row_cols: usize,
}

fn parse(xml: &str) -> String {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(false);
    let mut p = Parser::default();
    let mut buf = Vec::new();

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => p.open(e.name().as_ref(), &e),
            Ok(Event::Empty(e)) => p.open(e.name().as_ref(), &e),
            Ok(Event::Text(t)) => {
                if p.in_text {
                    let s = crate::util::xml_text(&t);
                    p.text(&s);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if p.in_text {
                    let s = crate::util::xml_ref(&r);
                    p.text(&s);
                }
            }
            Ok(Event::End(e)) => p.close(e.name().as_ref()),
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    p.out
}

impl Parser {
    fn open(&mut self, name: &[u8], e: &BytesStart) {
        match name {
            b"w:p" => {
                self.para.clear();
                self.style = None;
                self.is_list = false;
            }
            b"w:pStyle" => self.style = attr_val(e, b"w:val"),
            b"w:numPr" => self.is_list = true,
            b"w:b" => self.bold = toggle_on(e),
            b"w:i" => self.italic = toggle_on(e),
            b"w:t" => self.in_text = true,
            b"w:tbl" => {
                self.in_table = true;
                self.rows.clear();
                self.first_row_cols = 0;
            }
            b"w:tr" => self.row = Vec::new(),
            b"w:tc" => {
                self.in_cell = true;
                self.cell.clear();
            }
            _ => {}
        }
    }

    fn text(&mut self, s: &str) {
        if self.in_cell {
            self.cell.push_str(s);
            return;
        }
        let mut piece = s.to_string();
        if self.bold {
            piece = format!("**{piece}**");
        }
        if self.italic {
            piece = format!("*{piece}*");
        }
        self.para.push_str(&piece);
    }

    fn close(&mut self, name: &[u8]) {
        match name {
            b"w:t" => self.in_text = false,
            b"w:r" => {
                self.bold = false;
                self.italic = false;
            }
            b"w:p" => {
                if self.in_cell {
                    self.cell.push(' ');
                } else {
                    self.flush_para();
                }
            }
            b"w:tc" => {
                self.in_cell = false;
                self.row.push(self.cell.trim().to_string());
            }
            b"w:tr" => self.rows.push(std::mem::take(&mut self.row)),
            b"w:tbl" => {
                self.in_table = false;
                self.flush_table();
            }
            _ => {}
        }
    }

    fn flush_para(&mut self) {
        let text = self.para.trim();
        if text.is_empty() {
            return;
        }
        let prefix = match self.style.as_deref() {
            Some(s) if s.starts_with("Heading") => {
                let lvl = s
                    .trim_start_matches("Heading")
                    .parse::<usize>()
                    .unwrap_or(1)
                    .clamp(1, 6);
                format!("{} ", "#".repeat(lvl))
            }
            Some("Title") => "# ".to_string(),
            Some("Subtitle") => "## ".to_string(),
            // python-docx etc. use a "List…" paragraph style; numbering itself
            // lives in styles.xml which we don't read, so trust the style name.
            Some(s) if s.starts_with("List") => "- ".to_string(),
            _ if self.is_list => "- ".to_string(),
            _ => String::new(),
        };
        self.out.push_str(&prefix);
        self.out.push_str(text);
        self.out.push_str("\n\n");
    }

    fn flush_table(&mut self) {
        let rows = std::mem::take(&mut self.rows);
        if rows.is_empty() {
            return;
        }
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let cell = |row: &Vec<String>, i: usize| {
            row.get(i)
                .map(|s| s.replace('|', "\\|"))
                .unwrap_or_default()
        };
        // header row
        let header: Vec<String> = (0..ncols).map(|i| cell(&rows[0], i)).collect();
        self.out.push_str(&format!("| {} |\n", header.join(" | ")));
        self.out.push_str(&format!("|{}\n", " --- |".repeat(ncols)));
        for row in &rows[1..] {
            let cells: Vec<String> = (0..ncols).map(|i| cell(row, i)).collect();
            self.out.push_str(&format!("| {} |\n", cells.join(" | ")));
        }
        self.out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    const DOC: &str = r#"<?xml version="1.0"?>
<w:document xmlns:w="x">
 <w:body>
  <w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Title Here</w:t></w:r></w:p>
  <w:p><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:t> normal</w:t></w:r></w:p>
  <w:p><w:pPr><w:pStyle w:val="ListBullet"/></w:pPr><w:r><w:t>item one</w:t></w:r></w:p>
  <w:tbl>
   <w:tr><w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Age</w:t></w:r></w:p></w:tc></w:tr>
   <w:tr><w:tc><w:p><w:r><w:t>Ada</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>5</w:t></w:r></w:p></w:tc></w:tr>
  </w:tbl>
 </w:body>
</w:document>"#;

    #[test]
    fn converts_structure() {
        let md = parse(DOC);
        assert!(md.contains("# Title Here"), "heading: {md}");
        assert!(md.contains("**bold** normal"), "bold run: {md}");
        assert!(md.contains("- item one"), "list: {md}");
        assert!(md.contains("| Name | Age |"), "table header: {md}");
        assert!(md.contains("| Ada | 5 |"), "table row: {md}");
    }
}
