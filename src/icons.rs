// Per-extension icons + accent tints (ADR 0003, D5).
//
// This module answers a question `Format` deliberately does not: *what glyph and
// colour identify THIS specific language / file type* — a `.rs` and a `.py` are
// both [`Format::Text`], yet they should read as Rust and Python. So icons layer
// ABOVE `Format`: we key on the lowercased extension for a rich, per-language
// look and fall back to the file's `Format` for anything unlisted.
//
// Both functions are PURE (no IO) and unit-tested without a terminal, matching
// the single-registry, extension-first ethos of `format.rs` (ADR 0001): adding a
// file type is one table row here, mirrored against the same lowercased-extension
// convention `classify_path` uses.
//
// The glyphs are Nerd Font code points (Unicode Private Use Area) — the widely
// shared `nf-*` set used by eza / lsd / vscode-icons. They render only when the
// terminal is using a patched Nerd Font, which is exactly why they sit behind the
// opt-in [`crate::config::IconMode::Nerd`] (D5): guessing wrong prints mojibake.
//
// Colour policy (D5): a language's brand colour is *identity*, not a theme role,
// so per-language tints are literal `Color::Rgb(...)` — they should look like
// "Rust orange" / "TypeScript blue" under every palette, not shift with the
// theme. Anything without a brand identity (images, video, archives, plain docs,
// unknown types) instead falls back to `fmt.color()`, which DOES read from
// `theme::palette()` — so those keep tracking the active theme's roles.

use crate::format::Format;
use ratatui::style::Color;

/// The Nerd Font glyph identifying a file, keyed on its lowercased extension with
/// a [`Format`] fallback. PURE.
///
/// Directories win outright (a folder can carry a dotted name), then the
/// per-extension table, then a `Format`-based default. Never returns an empty
/// string or a bare ASCII letter — an unlisted extension still yields a
/// meaningful category glyph via [`fallback_glyph`].
pub fn nerd_glyph(ext: &str, fmt: Format) -> &'static str {
    if fmt == Format::Directory {
        return "\u{f07b}"; // nf-fa-folder
    }
    match ext {
        // --- Systems / compiled languages ---
        "rs" => "\u{e7a8}",           // nf-seti-rust
        "go" => "\u{e627}",           // nf-seti-go
        "c" | "h" => "\u{e61e}",      // nf-custom-c
        "cpp" | "hpp" => "\u{e61d}",  // nf-custom-cpp
        "swift" => "\u{e755}",        // nf-seti-swift
        "java" => "\u{e738}",         // nf-dev-java
        "kt" => "\u{e634}",           // nf-seti-kotlin
        "cs" => "\u{e648}",           // nf-seti-c_sharp
        // --- Scripting / dynamic languages ---
        "py" => "\u{e606}",           // nf-seti-python
        "rb" => "\u{e739}",           // nf-seti-ruby
        "php" => "\u{e73d}",          // nf-dev-php
        "lua" => "\u{e620}",          // nf-seti-lua
        "sh" | "bash" | "zsh" => "\u{e795}", // nf-seti-shell
        "vim" => "\u{e7c5}",          // nf-dev-vim
        // --- Web / JS family ---
        "js" => "\u{e74e}",           // nf-dev-javascript
        "ts" => "\u{e628}",           // nf-seti-typescript
        "jsx" | "tsx" => "\u{e7ba}",  // nf-dev-react
        "html" => "\u{e736}",         // nf-dev-html5
        "css" => "\u{e749}",          // nf-dev-css3
        "scss" => "\u{e74b}",         // nf-dev-sass
        // --- Data / config / markup ---
        "json" => "\u{e60b}",         // nf-seti-json
        "yaml" | "yml" => "\u{e615}", // nf-seti-config
        "toml" => "\u{e615}",         // nf-seti-config
        "xml" => "\u{e619}",          // nf-seti-xml
        "sql" => "\u{e706}",          // nf-dev-database
        "md" | "markdown" => "\u{e73e}", // nf-dev-markdown
        "txt" => "\u{f0f6}",          // nf-fa-file_text_o
        "lock" => "\u{f023}",         // nf-fa-lock
        // --- Media / documents (glyph is category, colour is the theme role) ---
        "pdf" => "\u{f1c1}",          // nf-fa-file_pdf_o
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => "\u{f1c5}", // file_image_o
        "mp4" | "mov" | "mkv" | "webm" => "\u{f1c8}", // nf-fa-file_video_o
        "mp3" | "wav" | "flac" => "\u{f1c7}",         // nf-fa-file_audio_o
        "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" => "\u{f1c6}", // file_archive_o
        "docx" | "doc" => "\u{f1c2}", // nf-fa-file_word_o
        "xlsx" | "xls" | "csv" => "\u{f1c3}", // nf-fa-file_excel_o
        "pptx" | "ppt" | "key" => "\u{f1c4}", // nf-fa-file_powerpoint_o
        // Unlisted: fall back to the file's Format category.
        _ => fallback_glyph(fmt),
    }
}

/// The category glyph for a [`Format`] when the extension isn't in the table.
/// Mirrors `Format::glyph`'s coverage but in Nerd Font code points.
fn fallback_glyph(fmt: Format) -> &'static str {
    match fmt {
        Format::Directory => "\u{f07b}",         // folder
        Format::Markdown => "\u{e73e}",          // markdown
        Format::Text => "\u{f1c9}",              // nf-fa-file_code_o
        Format::Sheet => "\u{f1c3}",             // excel
        Format::Image | Format::Svg => "\u{f1c5}", // image
        Format::Pdf => "\u{f1c1}",               // pdf
        Format::Video => "\u{f1c8}",             // video
        Format::Audio => "\u{f1c7}",             // audio
        Format::Docx | Format::Doc => "\u{f1c2}", // word
        Format::Pptx | Format::Keynote => "\u{f1c4}", // powerpoint
        Format::Archive => "\u{f1c6}",           // archive
        Format::Binary => "\u{f471}",            // nf-oct-file_binary
    }
}

/// The accent colour identifying a file, keyed on its lowercased extension with a
/// [`Format`] fallback. PURE.
///
/// Listed languages get a literal brand tint (identity — see the module note);
/// everything else defers to [`Format::color`], which reads the active
/// [`crate::theme::palette`] so image / video / archive / doc rows still track
/// the theme. Tints are chosen to stay legible on a dark background.
pub fn nerd_color(ext: &str, fmt: Format) -> Color {
    match ext {
        "rs" => Color::Rgb(222, 165, 132),   // Rust orange-tan
        "py" => Color::Rgb(75, 139, 209),    // Python blue
        "go" => Color::Rgb(0, 173, 216),     // Go cyan
        "c" | "h" => Color::Rgb(101, 154, 210), // C blue
        "cpp" | "hpp" => Color::Rgb(243, 101, 140), // C++ pink
        "swift" => Color::Rgb(240, 120, 96), // Swift orange
        "java" => Color::Rgb(215, 163, 74),  // Java tan
        "kt" => Color::Rgb(169, 123, 255),   // Kotlin purple
        "cs" => Color::Rgb(106, 168, 79),    // C# green
        "rb" => Color::Rgb(215, 72, 65),     // Ruby red
        "php" => Color::Rgb(140, 150, 205),  // PHP indigo
        "lua" => Color::Rgb(81, 160, 207),   // Lua blue
        "sh" | "bash" | "zsh" => Color::Rgb(137, 224, 81), // shell green
        "vim" => Color::Rgb(108, 181, 111),  // Vim green
        "js" => Color::Rgb(240, 219, 79),    // JavaScript yellow
        "ts" => Color::Rgb(49, 120, 198),    // TypeScript blue
        "jsx" | "tsx" => Color::Rgb(97, 218, 251), // React cyan
        "html" => Color::Rgb(227, 101, 66),  // HTML orange
        "css" => Color::Rgb(100, 140, 220),  // CSS blue
        "scss" => Color::Rgb(198, 83, 140),  // Sass pink
        "json" => Color::Rgb(203, 203, 90),  // JSON yellow
        "yaml" | "yml" | "toml" => Color::Rgb(180, 160, 120), // config sand
        "xml" => Color::Rgb(227, 140, 66),   // XML orange
        "sql" => Color::Rgb(240, 180, 90),   // SQL amber
        // Unlisted: defer to the theme role for this Format.
        _ => fmt.color(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_get_nonempty_distinct_glyphs() {
        let langs = ["rs", "py", "js", "ts", "go", "c", "rb", "html"];
        let glyphs: Vec<&str> = langs
            .iter()
            .map(|e| nerd_glyph(e, Format::Text))
            .collect();
        // None empty and none a bare ASCII byte.
        for (e, g) in langs.iter().zip(&glyphs) {
            assert!(!g.is_empty(), "{e} glyph is empty");
            assert!(g.chars().all(|c| c as u32 > 0x7f), "{e} glyph is ASCII");
        }
        // Distinct languages get distinct glyphs.
        for i in 0..glyphs.len() {
            for j in (i + 1)..glyphs.len() {
                assert_ne!(glyphs[i], glyphs[j], "{} and {} collide", langs[i], langs[j]);
            }
        }
    }

    #[test]
    fn unknown_extension_falls_back_to_format_glyph() {
        // A `.wat` file classified Text should show the Text category glyph, which
        // is exactly the fallback for an unlisted extension.
        assert_eq!(nerd_glyph("wat", Format::Text), fallback_glyph(Format::Text));
        assert_eq!(
            nerd_glyph("nope", Format::Binary),
            fallback_glyph(Format::Binary)
        );
        // The empty extension (extension-less file) also falls back.
        assert_eq!(nerd_glyph("", Format::Binary), "\u{f471}");
    }

    #[test]
    fn directory_always_gets_the_folder_glyph() {
        assert_eq!(nerd_glyph("", Format::Directory), "\u{f07b}");
        // Even a directory whose name has a dotted extension stays a folder.
        assert_eq!(nerd_glyph("rs", Format::Directory), "\u{f07b}");
    }

    #[test]
    fn known_extensions_get_brand_colours() {
        assert_eq!(nerd_color("rs", Format::Text), Color::Rgb(222, 165, 132));
        assert_eq!(nerd_color("go", Format::Text), Color::Rgb(0, 173, 216));
        assert_eq!(nerd_color("ts", Format::Text), Color::Rgb(49, 120, 198));
        assert_eq!(nerd_color("html", Format::Text), Color::Rgb(227, 101, 66));
        // Two Format::Text languages get DISTINCT tints — the whole point of D5.
        assert_ne!(
            nerd_color("rs", Format::Text),
            nerd_color("py", Format::Text)
        );
    }

    #[test]
    fn unknown_extension_falls_back_to_format_colour() {
        // Unlisted → the Format's palette role (identical to Format::color()).
        assert_eq!(nerd_color("wat", Format::Binary), Format::Binary.color());
        assert_eq!(nerd_color("", Format::Image), Format::Image.color());
        // A directory (empty ext) tracks the dir role.
        assert_eq!(nerd_color("", Format::Directory), Format::Directory.color());
    }
}
