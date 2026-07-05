// The single file-classification registry (ADR 0001).
//
// One `Format` enum answers both questions the app used to answer with two
// diverging tables: *which viewer opens a file* and *how the browser presents
// it* (colour / glyph / label). Adding a file type touches exactly one place.
//
// Classification is a PURE function `classify(ext, is_dir, head)` — extension
// first, with a byte `head` disambiguating only unknown / extension-less files —
// unit-tested without the filesystem. The thin `classify_path` wrapper does the
// only IO (reading the head when the extension can't decide).

use crate::highlight;
use crate::theme;
use ratatui::style::Color;
use std::fs;
use std::io::Read;
use std::path::Path;

/// How many bytes of a file's head disambiguate an unknown extension.
const HEAD_BYTES: usize = 8 * 1024;

/// A classified file. Each variant answers "which viewer opens me" (`opens`)
/// and "how does the browser show me" (`color` / `glyph` / `label`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Directory,
    Markdown,
    Text,
    Sheet,
    Image,
    Svg,
    Pdf,
    Video,
    Docx,
    Pptx,
    Keynote,
    Doc,
    Audio,
    Archive,
    Binary,
}

/// Classify a file from its (already-lowercased) extension, its directory-ness,
/// and an optional byte `head`. PURE — no IO.
///
/// A known extension wins outright and ignores `head`. For an unknown or empty
/// extension we consult `head`: textual → [`Format::Text`], otherwise
/// [`Format::Binary`]. `head == None` (the directory list, which classifies by
/// extension only) also yields `Binary` for an unknown extension.
pub fn classify(ext: &str, is_dir: bool, head: Option<&[u8]>) -> Format {
    if is_dir {
        return Format::Directory;
    }
    match ext {
        "md" | "markdown" | "mdx" => Format::Markdown,
        // Tabular data — including csv/tsv — belongs in the grid viewer.
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" | "csv" | "tsv" => Format::Sheet,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tiff" | "tif" | "ico" => Format::Image,
        "pdf" => Format::Pdf,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "m4v" => Format::Video,
        "docx" => Format::Docx,
        "pptx" => Format::Pptx,
        "key" => Format::Keynote,
        "doc" | "rtf" | "odt" | "ppt" => Format::Doc,
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" => Format::Audio,
        "zip" | "gz" | "tar" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst" => Format::Archive,
        // SVG is XML markup we can now both rasterise and show as source.
        "svg" => Format::Svg,
        // Any other known source / plain-text extension.
        e if highlight::is_text_ext(e) => Format::Text,
        // Unknown or empty extension: the head decides text vs binary.
        _ => match head {
            Some(h) if looks_textual(h) => Format::Text,
            _ => Format::Binary,
        },
    }
}

/// True when `head` looks like text: no NUL byte AND valid UTF-8, tolerating an
/// incomplete final multi-byte sequence split by the read boundary (its
/// `valid_up_to()` falls within the last 3 bytes of the head). Empty head → false.
fn looks_textual(head: &[u8]) -> bool {
    if head.is_empty() || head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // Accept only a truncated trailing char (a UTF-8 sequence is at most 4
        // bytes, so a boundary split leaves ≤ 3 valid-but-incomplete bytes).
        Err(e) => e.valid_up_to() >= head.len().saturating_sub(3),
    }
}

/// IO wrapper around [`classify`]: computes `is_dir` and the lowercased
/// extension, and — only when the extension can't decide — reads up to
/// [`HEAD_BYTES`] of the file to distinguish text from binary.
pub fn classify_path(path: &Path) -> Format {
    let is_dir = path.is_dir();
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // `classify` with no head returns `Binary` exactly for an unknown/empty
    // extension; that's the only case worth a file read.
    match classify(&ext, is_dir, None) {
        Format::Binary => classify(&ext, is_dir, read_head(path).as_deref()),
        other => other,
    }
}

/// Read up to [`HEAD_BYTES`] of a file's head; None on any IO error.
fn read_head(path: &Path) -> Option<Vec<u8>> {
    let mut f = fs::File::open(path).ok()?;
    let mut buf = vec![0u8; HEAD_BYTES];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

impl Format {
    /// Human-readable category name (browser label + "no viewer for …" text).
    pub fn label(&self) -> &'static str {
        match self {
            Format::Directory => "Directory",
            Format::Markdown => "Markdown",
            Format::Text => "Text",
            Format::Sheet => "Spreadsheet",
            Format::Image => "Image",
            Format::Svg => "SVG",
            Format::Pdf => "PDF",
            Format::Video => "Video",
            Format::Docx => "Word Document",
            Format::Pptx => "Presentation",
            Format::Keynote => "Keynote",
            Format::Doc => "Document",
            Format::Audio => "Audio",
            Format::Archive => "Archive",
            Format::Binary => "File",
        }
    }

    /// Single-column glyph for the browser list; ASCII-safe Unicode.
    pub fn glyph(&self) -> &'static str {
        match self {
            Format::Directory => "▸",
            Format::Image | Format::Svg => "▦",
            Format::Video => "▶",
            Format::Audio => "♪",
            Format::Pdf => "▤",
            Format::Sheet => "▤",
            Format::Keynote => "▦",
            Format::Markdown | Format::Docx | Format::Pptx | Format::Doc => "▢",
            Format::Text => "◇",
            Format::Archive => "▣",
            Format::Binary => "·",
        }
    }

    /// Palette colour for the browser (see `crate::theme`).
    pub fn color(&self) -> Color {
        match self {
            Format::Directory => theme::DIR,
            Format::Image | Format::Svg | Format::Keynote => theme::IMAGE,
            Format::Video | Format::Audio => theme::VIDEO,
            Format::Pdf => theme::PDF,
            Format::Sheet => theme::SHEET,
            Format::Markdown | Format::Docx | Format::Pptx | Format::Doc => theme::DOC,
            Format::Text => theme::CODE,
            Format::Archive => theme::ARCHIVE,
            Format::Binary => theme::OTHER,
        }
    }

    /// Does Sucher have a viewer that opens this format?
    pub fn opens(&self) -> bool {
        matches!(
            self,
            Format::Markdown
                | Format::Text
                | Format::Sheet
                | Format::Image
                | Format::Svg
                | Format::Pdf
                | Format::Video
                | Format::Docx
                | Format::Pptx
                | Format::Keynote
                | Format::Archive
                | Format::Binary
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Classify a file by extension only (as the directory list does).
    fn by_ext(ext: &str) -> Format {
        classify(ext, false, None)
    }

    #[test]
    fn directories_win_over_any_extension() {
        assert_eq!(classify("rs", true, None), Format::Directory);
        assert_eq!(classify("", true, None), Format::Directory);
    }

    #[test]
    fn tabular_text_is_a_sheet() {
        // ADR-0001 divergence: browser said "Spreadsheet", Enter opened Markdown.
        assert_eq!(by_ext("csv"), Format::Sheet);
        assert_eq!(by_ext("tsv"), Format::Sheet);
        assert_eq!(by_ext("xlsx"), Format::Sheet);
    }

    #[test]
    fn svg_is_its_own_format() {
        // Now rasterisable (resvg) *and* shown as source — its own viewer.
        assert_eq!(by_ext("svg"), Format::Svg);
        assert!(by_ext("svg").opens());
    }

    #[test]
    fn office_binaries_are_doc_not_markdown() {
        // ADR-0001 divergence: read_to_string on binary → Markdown garbage.
        assert_eq!(by_ext("doc"), Format::Doc);
        assert_eq!(by_ext("rtf"), Format::Doc);
        assert_eq!(by_ext("ppt"), Format::Doc);
    }

    #[test]
    fn pptx_and_keynote_have_their_own_viewers() {
        // pptx → markdown conversion; key → embedded-preview image.
        assert_eq!(by_ext("pptx"), Format::Pptx);
        assert_eq!(by_ext("key"), Format::Keynote);
        assert!(by_ext("pptx").opens());
        assert!(by_ext("key").opens());
    }

    #[test]
    fn source_code_is_text_not_markdown() {
        // ADR-0001 divergence: any code fell through to Markdown, mangled.
        assert_eq!(by_ext("rs"), Format::Text);
        assert_eq!(by_ext("py"), Format::Text);
        assert_eq!(by_ext("json"), Format::Text);
    }

    #[test]
    fn known_media_and_doc_extensions() {
        assert_eq!(by_ext("md"), Format::Markdown);
        assert_eq!(by_ext("docx"), Format::Docx);
        assert_eq!(by_ext("png"), Format::Image);
        assert_eq!(by_ext("pdf"), Format::Pdf);
        assert_eq!(by_ext("mp4"), Format::Video);
        assert_eq!(by_ext("mp3"), Format::Audio);
        assert_eq!(by_ext("zip"), Format::Archive);
    }

    #[test]
    fn unknown_extension_without_head_is_binary() {
        // The directory list passes no head: unknown ext → Binary (by extension).
        assert_eq!(by_ext("wat"), Format::Binary);
        assert_eq!(by_ext(""), Format::Binary);
    }

    #[test]
    fn unknown_extension_with_textual_head_is_text() {
        let head = b"hello, this is plain text\n";
        assert_eq!(classify("", false, Some(head)), Format::Text);
        assert_eq!(classify("wat", false, Some(head)), Format::Text);
    }

    #[test]
    fn unknown_extension_with_nul_head_is_binary() {
        let head = b"\x89PNG\x00\x01\x02binary";
        assert_eq!(classify("", false, Some(head)), Format::Binary);
        assert_eq!(classify("wat", false, Some(head)), Format::Binary);
    }

    #[test]
    fn known_extension_ignores_head() {
        // A `.rs` stays Text even with binary bytes; extension wins.
        assert_eq!(classify("rs", false, Some(b"\x00\x01")), Format::Text);
    }

    #[test]
    fn looks_textual_boundary_split_char() {
        // "é" is 0xC3 0xA9; drop the trailing byte to simulate a read that split
        // a multi-byte char at the head boundary. Still textual.
        let mut head = "text ends with é".as_bytes().to_vec();
        head.pop(); // remove 0xA9, leaving a lone 0xC3 lead byte
        assert!(looks_textual(&head));
    }

    #[test]
    fn looks_textual_rejects_nul_and_empty() {
        assert!(!looks_textual(b"has a \x00 nul"));
        assert!(!looks_textual(b""));
        assert!(looks_textual(b"plain ascii"));
    }

    #[test]
    fn opens_matches_the_viewable_set() {
        for f in [
            Format::Markdown,
            Format::Text,
            Format::Sheet,
            Format::Image,
            Format::Svg,
            Format::Pdf,
            Format::Video,
            Format::Docx,
            Format::Pptx,
            Format::Keynote,
            Format::Archive,
            Format::Binary,
        ] {
            assert!(f.opens(), "{f:?} should open");
        }
        for f in [Format::Directory, Format::Doc, Format::Audio] {
            assert!(!f.opens(), "{f:?} should not open");
        }
    }
}
