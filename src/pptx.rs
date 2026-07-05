// PPTX -> markdown. A .pptx is a zip; each slide's text lives in
// ppt/slides/slideN.xml as DrawingML, where visible runs are `<a:t>` inside
// paragraphs `<a:p>`. We enumerate the slide parts in numeric order, extract the
// text paragraph by paragraph, and emit markdown (a `## Slide N` heading plus one
// bullet per paragraph) so the existing markdown TUI renders it — no new UI.
//
// Layout and speaker notes are dropped: this is a reading view of the words on
// the slides, matching how docx.rs reduces a document. Embedded images aren't
// inlined here, but `media()` extracts them for the viewer's image gallery.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::io::Read;

/// Embedded raster images from a .pptx (`ppt/media/`), extracted to temp files
/// for the viewer's image gallery. Empty when the deck has none.
pub fn media(path: &str) -> Vec<std::path::PathBuf> {
    crate::util::extract_ooxml_media(path, "ppt/media/")
}

pub fn to_markdown(path: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    // Collect slide part names, then order them by their numeric suffix so
    // slide10 sorts after slide2 (lexical order would not).
    let mut slides: Vec<(u32, String)> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter_map(|name| slide_number(&name).map(|n| (n, name)))
        .collect();
    slides.sort_by_key(|(n, _)| *n);
    if slides.is_empty() {
        return Err("no slides found (not a pptx?)".to_string());
    }

    let mut out = String::new();
    for (idx, (_, name)) in slides.iter().enumerate() {
        let mut xml = String::new();
        zip.by_name(name)
            .map_err(|e| e.to_string())?
            .read_to_string(&mut xml)
            .map_err(|e| e.to_string())?;
        out.push_str(&format!("## Slide {}\n\n", idx + 1));
        let paras = slide_paragraphs(&xml);
        if paras.is_empty() {
            out.push_str("*(no text)*\n\n");
        } else {
            for p in paras {
                out.push_str(&format!("- {p}\n"));
            }
            out.push('\n');
        }
    }
    Ok(out)
}

/// The slide index of a part name, e.g. `ppt/slides/slide12.xml` -> `12`. Returns
/// None for any other part (layouts, masters, media, rels, …).
fn slide_number(name: &str) -> Option<u32> {
    let stem = name.strip_prefix("ppt/slides/slide")?;
    stem.strip_suffix(".xml")?.parse().ok()
}

/// Extract one string per `<a:p>` paragraph: the concatenation of its `<a:t>`
/// runs, trimmed. Empty paragraphs (spacer boxes) are dropped. PURE — unit-tested.
fn slide_paragraphs(xml: &str) -> Vec<String> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut paras = Vec::new();
    let mut cur = String::new();
    let mut in_text = false;

    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"a:p" => cur.clear(),
                b"a:t" => in_text = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                cur.push_str(&crate::util::xml_text(&t));
            }
            Ok(Event::GeneralRef(r)) if in_text => {
                cur.push_str(&crate::util::xml_ref(&r));
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"a:t" => in_text = false,
                b"a:p" => {
                    let text = cur.trim();
                    if !text.is_empty() {
                        paras.push(text.to_string());
                    }
                }
                _ => {}
            },
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    paras
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slide_number_parses_only_slide_parts() {
        assert_eq!(slide_number("ppt/slides/slide1.xml"), Some(1));
        assert_eq!(slide_number("ppt/slides/slide12.xml"), Some(12));
        assert_eq!(slide_number("ppt/slides/_rels/slide1.xml.rels"), None);
        assert_eq!(slide_number("ppt/slideLayouts/slideLayout1.xml"), None);
        assert_eq!(slide_number("docProps/core.xml"), None);
    }

    #[test]
    fn paragraphs_join_runs_and_drop_empties() {
        let xml = r#"<p:sld xmlns:a="x" xmlns:p="y">
          <p:cSld><p:spTree>
            <p:sp><p:txBody>
              <a:p><a:r><a:t>Hello </a:t></a:r><a:r><a:t>World</a:t></a:r></a:p>
              <a:p></a:p>
              <a:p><a:r><a:t>Second line</a:t></a:r></a:p>
            </p:txBody></p:sp>
          </p:spTree></p:cSld>
        </p:sld>"#;
        let paras = slide_paragraphs(xml);
        assert_eq!(paras, vec!["Hello World", "Second line"]);
    }

    #[test]
    fn escaped_entities_are_unescaped() {
        let xml = r#"<a:p><a:r><a:t>a &amp; b &lt;c&gt;</a:t></a:r></a:p>"#;
        assert_eq!(slide_paragraphs(xml), vec!["a & b <c>"]);
    }
}
