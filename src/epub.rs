// EPUB -> markdown. An .epub is a zip of XHTML: `META-INF/container.xml` points
// at an OPF package file whose `<manifest>` lists every part (id → href +
// media-type) and whose `<spine>` gives the ordered `<itemref>` reading sequence.
// We resolve the spine to its content documents, read each one, reduce it with
// the shared HTML→markdown reducer (`html::markdown_from_str` — epub content is
// XHTML, so it applies directly), and concatenate the chapters in spine order so
// the existing markdown TUI renders the book — no new UI (mirrors docx/pptx/html).
//
// The href-resolution logic (container → OPF path, manifest+spine join) is
// factored into PURE functions unit-tested against inline XML; `to_markdown` and
// `media` are the thin zip-IO wrappers.

use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use std::collections::HashMap;
use std::path::PathBuf;

/// Embedded raster images from an .epub, extracted to temp files for the viewer's
/// image gallery. Empty when the book has none. epub images sit under no fixed
/// prefix, so every raster member in the zip is taken (see `util::extract_epub_media`).
pub fn media(path: &str) -> Vec<PathBuf> {
    crate::util::extract_epub_media(path)
}

pub fn to_markdown(path: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;

    // 1. Locate the OPF package via the mandatory META-INF/container.xml.
    let container = {
        let member = zip
            .by_name("META-INF/container.xml")
            .map_err(|_| "not an epub (no META-INF/container.xml)".to_string())?;
        // Byte-cap every decompressed member (ADR 0009): a zip bomb hidden in any
        // part would otherwise inflate unbounded through `read_to_string`.
        crate::util::read_to_string_capped(member, crate::util::MAX_DECODE_BYTES)?
    };
    let opf_path = opf_path_from_container(&container)
        .ok_or_else(|| "epub container.xml names no rootfile (OPF)".to_string())?;

    // 2. Read + parse the OPF for the spine's ordered content documents, resolved
    //    relative to the OPF's own directory.
    let opf_xml = {
        let member = zip
            .by_name(&opf_path)
            .map_err(|_| format!("epub OPF missing: {opf_path}"))?;
        crate::util::read_to_string_capped(member, crate::util::MAX_DECODE_BYTES)?
    };
    let hrefs = spine_hrefs(&opf_xml, opf_dir_of(&opf_path));
    if hrefs.is_empty() {
        return Err("epub has no spine content".to_string());
    }

    // 3. Reduce each chapter to markdown and concatenate in spine order, a `---`
    //    rule between chapters. The *total* output is bounded too (ADR 0009): a
    //    book with thousands of chapters — each individually under the cap — could
    //    still blow up unbounded, so we stop appending past the cap and mark it.
    let mut out = String::new();
    let mut truncated = false;
    for href in &hrefs {
        let Ok(member) = zip.by_name(href) else {
            continue; // spine referenced a href not present in the zip — skip it.
        };
        let Ok(xml) = crate::util::read_to_string_capped(member, crate::util::MAX_DECODE_BYTES)
        else {
            continue; // an over-cap / non-UTF-8 chapter is skipped, not fatal.
        };
        let md = crate::html::markdown_from_str(&xml);
        if md.trim().is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("---\n\n");
        }
        out.push_str(md.trim_end());
        out.push_str("\n\n");
        if out.len() > crate::util::MAX_DECODE_BYTES {
            truncated = true;
            break;
        }
    }
    if truncated {
        out.push_str("… (truncated)\n");
    }
    if out.trim().is_empty() {
        return Err("epub has no readable text".to_string());
    }
    Ok(out)
}

/// An element attribute's value by exact (unqualified) key. epub's container/OPF
/// attributes (`full-path`, `href`, `media-type`, `id`, `idref`) carry no
/// namespace prefix, so a plain byte match suffices. None when absent.
fn attr(e: &BytesStart, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// The OPF package file's full path from `META-INF/container.xml`: the
/// `full-path` of the first `<rootfile>`. PURE — unit-tested. None when the XML
/// has no rootfile (a malformed / non-epub container).
fn opf_path_from_container(xml: &str) -> Option<String> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() == b"rootfile" {
                    if let Some(p) = attr(&e, b"full-path") {
                        return Some(p);
                    }
                }
            }
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    None
}

/// The directory portion of an OPF full-path (`OEBPS/content.opf` → `OEBPS`,
/// a top-level `content.opf` → ``). Spine hrefs resolve relative to it.
fn opf_dir_of(full_path: &str) -> &str {
    match full_path.rfind('/') {
        Some(i) => &full_path[..i],
        None => "",
    }
}

/// The ordered, zip-relative hrefs of the spine's content documents. PURE —
/// unit-tested. Joins the `<manifest>` (id → href + media-type) with the
/// `<spine>` (`<itemref idref=…>` order): each itemref is resolved to its
/// manifest item, kept only when that item is an XHTML/HTML content document, and
/// its href resolved relative to `opf_dir`. An itemref whose idref is missing
/// from the manifest (or points at a non-content part like the NCX toc) is
/// skipped, preserving reading order for the rest.
fn spine_hrefs(opf_xml: &str, opf_dir: &str) -> Vec<String> {
    let mut r = Reader::from_str(opf_xml);
    r.config_mut().trim_text(true);
    let mut buf = Vec::new();
    // id → (href, media-type)
    let mut manifest: HashMap<String, (String, String)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"item" => {
                    if let (Some(id), Some(href)) = (attr(&e, b"id"), attr(&e, b"href")) {
                        let mt = attr(&e, b"media-type").unwrap_or_default();
                        manifest.insert(id, (href, mt));
                    }
                }
                b"itemref" => {
                    if let Some(idref) = attr(&e, b"idref") {
                        order.push(idref);
                    }
                }
                _ => {}
            },
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    order
        .iter()
        .filter_map(|idref| {
            let (href, media_type) = manifest.get(idref)?;
            is_content(media_type, href).then(|| resolve_href(opf_dir, href))
        })
        .collect()
}

/// Whether a manifest item is a spine *content* document: an XHTML/HTML
/// media-type, or (when the media-type is absent) an XHTML/HTML file extension.
/// Filters out stylesheets, images, and the NCX toc that a spine may reference.
fn is_content(media_type: &str, href: &str) -> bool {
    let mt = media_type.to_ascii_lowercase();
    if mt == "application/xhtml+xml" || mt == "text/html" {
        return true;
    }
    let h = href.to_ascii_lowercase();
    mt.is_empty() && (h.ends_with(".xhtml") || h.ends_with(".html") || h.ends_with(".htm"))
}

/// Resolve a manifest href against the OPF's directory into a zip-relative member
/// name, collapsing `.`/`..` segments (a nested `OEBPS/content.opf` with an href
/// of `../images/x.xhtml` resolves cleanly). Forward-slash paths throughout, as
/// zip member names are.
fn resolve_href(opf_dir: &str, href: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !opf_dir.is_empty() {
        parts.extend(opf_dir.split('/').filter(|s| !s.is_empty()));
    }
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_yields_opf_full_path() {
        let xml = r#"<?xml version="1.0"?>
          <container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
            <rootfiles>
              <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
            </rootfiles>
          </container>"#;
        assert_eq!(
            opf_path_from_container(xml).as_deref(),
            Some("OEBPS/content.opf")
        );
    }

    #[test]
    fn container_without_rootfile_is_none() {
        assert_eq!(opf_path_from_container("<container></container>"), None);
    }

    #[test]
    fn opf_dir_is_the_parent_of_the_package() {
        assert_eq!(opf_dir_of("OEBPS/content.opf"), "OEBPS");
        assert_eq!(opf_dir_of("content.opf"), "");
        assert_eq!(opf_dir_of("a/b/c/pkg.opf"), "a/b/c");
    }

    const OPF: &str = r#"<?xml version="1.0"?>
      <package xmlns="http://www.idpf.org/2007/opf" version="3.0">
        <manifest>
          <item id="c1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
          <item id="c2" href="text/chapter2.xhtml" media-type="application/xhtml+xml"/>
          <item id="css" href="style.css" media-type="text/css"/>
          <item id="cover" href="images/cover.png" media-type="image/png"/>
          <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
        </manifest>
        <spine toc="ncx">
          <itemref idref="c1"/>
          <itemref idref="c2"/>
          <itemref idref="css"/>
          <itemref idref="missing"/>
        </spine>
      </package>"#;

    #[test]
    fn spine_orders_content_and_resolves_relative_to_opf_dir() {
        // Only the two XHTML content docs survive, in spine order, resolved under
        // the OPF's OEBPS/ directory — the css (wrong type), the missing idref,
        // and the image/ncx (never in the spine) drop out.
        let hrefs = spine_hrefs(OPF, "OEBPS");
        assert_eq!(
            hrefs,
            vec![
                "OEBPS/chapter1.xhtml".to_string(),
                "OEBPS/text/chapter2.xhtml".to_string(),
            ]
        );
    }

    #[test]
    fn spine_at_root_has_no_directory_prefix() {
        let hrefs = spine_hrefs(OPF, "");
        assert_eq!(hrefs, vec!["chapter1.xhtml", "text/chapter2.xhtml"]);
    }

    #[test]
    fn href_resolution_collapses_dot_segments() {
        assert_eq!(
            resolve_href("OEBPS", "chapter1.xhtml"),
            "OEBPS/chapter1.xhtml"
        );
        assert_eq!(
            resolve_href("OEBPS", "text/ch.xhtml"),
            "OEBPS/text/ch.xhtml"
        );
        assert_eq!(resolve_href("OEBPS/text", "../ch.xhtml"), "OEBPS/ch.xhtml");
        assert_eq!(resolve_href("OEBPS", "./ch.xhtml"), "OEBPS/ch.xhtml");
        assert_eq!(resolve_href("", "ch.xhtml"), "ch.xhtml");
    }

    #[test]
    fn content_type_gate_accepts_xhtml_only() {
        assert!(is_content("application/xhtml+xml", "a.xhtml"));
        assert!(is_content("text/html", "a.html"));
        // Absent media-type falls back to the extension.
        assert!(is_content("", "a.xhtml"));
        assert!(!is_content("", "a.css"));
        // A declared non-content type is refused regardless of extension.
        assert!(!is_content("text/css", "a.xhtml"));
        assert!(!is_content("image/png", "cover.png"));
    }
}
