// Small shared formatting helpers used across viewers (the directory browser's
// list/preview, and the "no viewer" metadata line). Kept here so there is one
// implementation rather than per-module copies that could drift.

use quick_xml::events::{BytesRef, BytesText};
use std::io::Read;
use std::path::PathBuf;
use std::time::SystemTime;

/// Extract embedded raster images living under `dir_prefix` (e.g. `word/media/`
/// for docx, `ppt/media/` for pptx) from an OOXML zip into a per-process temp
/// directory, returning the written paths sorted by archive name. Best-effort:
/// any error (not a zip, unreadable member, write failure) is skipped, and an
/// archive with no media yields an empty vec. The temp files live for the
/// viewer's lifetime; they are small and bounded by the document's own media.
pub fn extract_ooxml_media(archive: &str, dir_prefix: &str) -> Vec<PathBuf> {
    let Ok(file) = std::fs::File::open(archive) else {
        return Vec::new();
    };
    let Ok(mut zip) = zip::ZipArchive::new(file) else {
        return Vec::new();
    };
    let mut names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().map(|f| f.name().to_string()))
        .filter(|n| n.starts_with(dir_prefix) && is_raster_name(n))
        .collect();
    names.sort();
    if names.is_empty() {
        return Vec::new();
    }
    let dir = std::env::temp_dir().join(format!("sucher-media-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names {
        let Ok(mut f) = zip.by_name(&name) else {
            continue;
        };
        let mut bytes = Vec::new();
        if f.read_to_end(&mut bytes).is_err() {
            continue;
        }
        let file_name = std::path::Path::new(&name)
            .file_name()
            .map(|s| s.to_owned())
            .unwrap_or_default();
        let dest = dir.join(file_name);
        if std::fs::write(&dest, &bytes).is_ok() {
            out.push(dest);
        }
    }
    out
}

/// Does this archive member name end in a raster image extension the `image`
/// crate can decode? (SVG/EMF/WMF vector media are skipped — no in-tree decoder.)
fn is_raster_name(name: &str) -> bool {
    let n = name.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".bmp", ".tiff", ".tif", ".webp",
    ]
    .iter()
    .any(|e| n.ends_with(e))
}

/// Decode an XML `Text` event's bytes to a string. quick-xml (≥ 0.37) emits
/// entity references as separate [`Event::GeneralRef`](quick_xml::events::Event)
/// events, so a `Text` event never contains `&…;` — decoding is all that's
/// needed here; see [`xml_ref`] for the entity side. Empty on a decode error.
pub fn xml_text(t: &BytesText) -> String {
    t.decode().map(|c| c.into_owned()).unwrap_or_default()
}

/// Resolve an XML entity reference (`Event::GeneralRef`) to its text: the five
/// predefined entities (`amp`/`lt`/`gt`/`quot`/`apos`) and numeric char refs
/// (`#65`, `#x41`). We rebuild the `&name;` form and reuse quick-xml's own
/// unescaper so the mapping stays authoritative. Unknown entities → empty.
pub fn xml_ref(r: &BytesRef) -> String {
    let name = r.decode().map(|c| c.into_owned()).unwrap_or_default();
    quick_xml::escape::unescape(&format!("&{name};"))
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

/// Human-readable byte size (e.g. `1.2K`, `340 B`).
pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut f = n as f64;
    let mut i = 0;
    while f >= 1024.0 && i < 4 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1}{}", U[i])
}

/// Coarse relative time since `t` (e.g. `just now`, `3d ago`).
pub fn rel_time(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s if s < 86_400 * 30 => format!("{}d ago", s / 86_400),
        s if s < 86_400 * 365 => format!("{}mo ago", s / (86_400 * 30)),
        s => format!("{}y ago", s / (86_400 * 365)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0K");
        assert_eq!(human_size(1024 * 1024), "1.0M");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn rel_time_buckets() {
        let now = SystemTime::now();
        assert_eq!(rel_time(now), "just now");
        assert_eq!(rel_time(now - Duration::from_secs(120)), "2m ago");
        assert_eq!(rel_time(now - Duration::from_secs(3 * 86_400)), "3d ago");
    }

    #[test]
    fn extracts_embedded_images_from_samples() {
        let d = extract_ooxml_media("samples/sample.docx", "word/media/");
        assert_eq!(d.len(), 1, "docx should have 1 image");
        let p = extract_ooxml_media("samples/deck.pptx", "ppt/media/");
        assert_eq!(p.len(), 2, "pptx should have 2 images");
        assert!(p.iter().all(|x| x.exists()), "extracted files exist");
    }
}
