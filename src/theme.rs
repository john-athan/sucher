// Central palette: the single source of truth for Sucher's colours.
//
// Per-file-kind colours (used by the directory browser to tint entries and
// previews) plus the token colours for the syntax highlighter. Keeping them
// here means one edit re-themes the whole UI. Dependency flows theme -> highlight
// (for the `TokenKind` type only), never the reverse.

use crate::highlight::TokenKind;
use ratatui::style::Color;

/// Directories.
pub const DIR: Color = Color::Rgb(96, 165, 250);
/// Raster/vector images.
pub const IMAGE: Color = Color::Rgb(196, 160, 250);
/// Video (audio reuses this colour).
pub const VIDEO: Color = Color::Rgb(244, 114, 182);
/// PDFs.
pub const PDF: Color = Color::Rgb(248, 113, 113);
/// Spreadsheets.
pub const SHEET: Color = Color::Rgb(74, 222, 128);
/// Rich documents + markdown.
pub const DOC: Color = Color::Rgb(252, 211, 77);
/// Source code.
pub const CODE: Color = Color::Rgb(134, 239, 172);
/// Archives.
pub const ARCHIVE: Color = Color::Rgb(251, 146, 60);
/// Everything else (also the plain-text preview colour).
pub const OTHER: Color = Color::Rgb(205, 205, 215);
/// Muted secondary text (metadata, hints).
pub const DIM: Color = Color::Rgb(120, 120, 132);
/// Accent for the breadcrumb / active chrome.
pub const ACCENT: Color = Color::Rgb(125, 211, 252);

/// Colour a highlighter [`TokenKind`], cohesive with the palette above.
pub fn token_color(kind: TokenKind) -> Color {
    match kind {
        TokenKind::Keyword => Color::Rgb(147, 197, 253), // light blue
        TokenKind::Str => DOC,                           // yellow
        TokenKind::Comment => DIM,
        TokenKind::Number => IMAGE, // purple
        TokenKind::Plain => OTHER,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_kinds_map_to_distinct_colors() {
        let colors = [
            token_color(TokenKind::Keyword),
            token_color(TokenKind::Str),
            token_color(TokenKind::Comment),
            token_color(TokenKind::Number),
            token_color(TokenKind::Plain),
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(colors[i], colors[j], "colours {i} and {j} collide");
            }
        }
    }
}
