// Central palette: the single source of truth for Sucher's colours.
//
// Per-file-kind colours (used by the directory browser to tint entries and
// previews) plus the token colours for the syntax highlighter. Keeping them
// here means one edit — or one config line — re-themes the whole UI. Dependency
// flows theme -> highlight (for the `TokenKind` type only), never the reverse.
//
// Per ADR 0003 (D1) the colours are no longer compile-time consts but a runtime
// [`Palette`] held in a process-global [`OnceLock`]. `main` resolves a palette
// from the user's config and calls [`init`] once at startup; every call site
// reads a field off [`palette`]. The palette is immutable for the process's
// life, so a global read is the honest model — it avoids threading `&Palette`
// through dozens of render signatures for a value that never changes.

use crate::highlight::TokenKind;
use ratatui::style::Color;
use std::sync::OnceLock;

/// Every colour the UI can draw, as a flat record of `ratatui` colours.
///
/// The first block is the per-file-kind colours the browser uses to tint
/// entries and previews (see [`crate::format::Format::color`]); `dim` and
/// `accent` are the chrome colours (muted metadata / active breadcrumb). The
/// syntax highlighter's token colours are derived from these fields — only
/// `keyword` is unique to the highlighter (see [`token_color`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Directories.
    pub dir: Color,
    /// Raster/vector images.
    pub image: Color,
    /// Video (audio reuses this colour).
    pub video: Color,
    /// PDFs.
    pub pdf: Color,
    /// Spreadsheets.
    pub sheet: Color,
    /// Rich documents + markdown.
    pub doc: Color,
    /// Source code.
    pub code: Color,
    /// Archives.
    pub archive: Color,
    /// Everything else (also the plain-text preview colour).
    pub other: Color,
    /// Muted secondary text (metadata, hints).
    pub dim: Color,
    /// Accent for the breadcrumb / active chrome.
    pub accent: Color,
    /// Selection row background — a soft, low-luma accent-ish tint that reads as
    /// "here" without the harshness of a reverse-video bar (per theme).
    pub selection: Color,
    /// Highlighter keyword colour (the one token colour with no file-kind twin).
    pub keyword: Color,
}

/// The process-global palette. Set once by [`init`]; read via [`palette`].
static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Install the resolved palette. Idempotent: the first call wins and any later
/// call is a no-op, matching `OnceLock` semantics. Called once from `main`
/// after the config resolves; tests and pre-init paths never need it because
/// [`palette`] falls back to the default dark palette on its own.
pub fn init(p: Palette) {
    let _ = PALETTE.set(p);
}

/// The active palette. Safe before [`init`] (and in tests): falls back to
/// [`Palette::sucher_dark`], so a colour read can never observe an empty cell.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(Palette::sucher_dark)
}

/// Build a [`Color`] from a packed `0xRRGGBB` literal — a compact, greppable
/// spelling for the curated palettes below.
const fn hex(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

impl Palette {
    /// Sucher's original dark palette — the default, so upgrading never
    /// re-skins anyone. These are the EXACT RGB values that shipped as the
    /// pre-config `theme::*` consts; keep them byte-for-byte.
    pub fn sucher_dark() -> Self {
        Palette {
            dir: Color::Rgb(96, 165, 250),
            image: Color::Rgb(196, 160, 250),
            video: Color::Rgb(244, 114, 182),
            pdf: Color::Rgb(248, 113, 113),
            sheet: Color::Rgb(74, 222, 128),
            doc: Color::Rgb(252, 211, 77),
            code: Color::Rgb(134, 239, 172),
            archive: Color::Rgb(251, 146, 60),
            other: Color::Rgb(205, 205, 215),
            dim: Color::Rgb(120, 120, 132),
            accent: Color::Rgb(125, 211, 252),
            // Dark desaturated blue-grey — a hair above the background so the
            // selected row lifts without shouting.
            selection: Color::Rgb(38, 44, 62),
            keyword: Color::Rgb(147, 197, 253),
        }
    }

    /// A legible light-background variant of `sucher-dark`: the same hue story
    /// (blue dirs, purple images, red PDFs, …) but darkened and saturated so it
    /// stays readable on a light terminal, with a near-black `other`/text.
    pub fn sucher_light() -> Self {
        Palette {
            dir: hex(0x2563eb),
            image: hex(0x7c3aed),
            video: hex(0xdb2777),
            pdf: hex(0xdc2626),
            sheet: hex(0x16a34a),
            doc: hex(0xca8a04),
            code: hex(0x0d9488),
            archive: hex(0xea580c),
            other: hex(0x1f2937),
            dim: hex(0x6b7280),
            accent: hex(0x0284c7),
            selection: hex(0xdbeafe), // pale blue tint, legible under dark text
            keyword: hex(0x4338ca),
        }
    }

    /// Catppuccin Mocha (the popular pastel-on-dark theme). Roles map to the
    /// canonical named colours: Blue dirs, Mauve images, Red PDFs, and so on.
    pub fn catppuccin_mocha() -> Self {
        Palette {
            dir: hex(0x89b4fa),       // Blue
            image: hex(0xcba6f7),     // Mauve
            video: hex(0xf5c2e7),     // Pink
            pdf: hex(0xf38ba8),       // Red
            sheet: hex(0xa6e3a1),     // Green
            doc: hex(0xf9e2af),       // Yellow
            code: hex(0x94e2d5),      // Teal
            archive: hex(0xfab387),   // Peach
            other: hex(0xcdd6f4),     // Text
            dim: hex(0x7f849c),       // Overlay1
            accent: hex(0x89dceb),    // Sky
            selection: hex(0x2c3149), // Surface0 nudged toward the blue accent
            keyword: hex(0xb4befe),   // Lavender
        }
    }

    /// Gruvbox (dark). Uses the "bright" set for legibility on the dark
    /// background, with the neutral greys for chrome.
    pub fn gruvbox_dark() -> Self {
        Palette {
            dir: hex(0x83a598),       // bright blue
            image: hex(0xd3869b),     // bright purple
            video: hex(0xfb4934),     // bright red
            pdf: hex(0xcc241d),       // neutral red
            sheet: hex(0xb8bb26),     // bright green
            doc: hex(0xfabd2f),       // bright yellow
            code: hex(0x8ec07c),      // bright aqua
            archive: hex(0xfe8019),   // bright orange
            other: hex(0xebdbb2),     // fg
            dim: hex(0x928374),       // gray
            accent: hex(0x689d6a),    // neutral aqua
            selection: hex(0x3c3836), // bg1 — the native gruvbox selection warmth
            keyword: hex(0x458588),   // neutral blue
        }
    }

    /// Tokyo Night (the "storm/night" dark theme).
    pub fn tokyo_night() -> Self {
        Palette {
            dir: hex(0x7aa2f7),       // blue
            image: hex(0xbb9af7),     // magenta
            video: hex(0xf7768e),     // red
            pdf: hex(0xdb4b4b),       // red1
            sheet: hex(0x9ece6a),     // green
            doc: hex(0xe0af68),       // yellow
            code: hex(0x73daca),      // teal
            archive: hex(0xff9e64),   // orange
            other: hex(0xc0caf5),     // fg
            dim: hex(0x565f89),       // comment
            accent: hex(0x7dcfff),    // cyan
            selection: hex(0x283457), // the theme's canonical selection blue
            keyword: hex(0x9d7cd8),   // purple
        }
    }

    /// Resolve a built-in palette by its kebab-case name (as written in the
    /// config / `--theme` flag). `None` for an unknown name so the caller can
    /// decide the fallback.
    pub fn by_name(name: &str) -> Option<Palette> {
        Some(match name {
            "sucher-dark" => Palette::sucher_dark(),
            "sucher-light" => Palette::sucher_light(),
            "catppuccin-mocha" => Palette::catppuccin_mocha(),
            "gruvbox-dark" => Palette::gruvbox_dark(),
            "tokyo-night" => Palette::tokyo_night(),
            _ => return None,
        })
    }

    /// The nine per-file-kind colours, in a fixed order. Used by tests to assert
    /// a palette keeps its kinds visually distinct.
    #[cfg(test)]
    fn kind_colors(&self) -> [Color; 9] {
        [
            self.dir,
            self.image,
            self.video,
            self.pdf,
            self.sheet,
            self.doc,
            self.code,
            self.archive,
            self.other,
        ]
    }
}

/// Colour a highlighter [`TokenKind`], cohesive with the active palette. Only
/// `Keyword` has its own colour; the rest reuse file-kind colours so code
/// previews stay in the same visual family as the browser.
pub fn token_color(kind: TokenKind) -> Color {
    let p = palette();
    match kind {
        TokenKind::Keyword => p.keyword,
        TokenKind::Str => p.doc,      // yellow, like documents
        TokenKind::Comment => p.dim,  // muted, like metadata
        TokenKind::Number => p.image, // purple, like images
        TokenKind::Plain => p.other,  // default text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A named built-in palette constructor, for table-driven tests.
    type NamedPalette = (&'static str, fn() -> Palette);

    /// Every built-in palette, for table-driven tests.
    const BUILTINS: &[NamedPalette] = &[
        ("sucher-dark", Palette::sucher_dark),
        ("sucher-light", Palette::sucher_light),
        ("catppuccin-mocha", Palette::catppuccin_mocha),
        ("gruvbox-dark", Palette::gruvbox_dark),
        ("tokyo-night", Palette::tokyo_night),
    ];

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

    #[test]
    fn every_builtin_has_distinct_kind_colors() {
        for (name, make) in BUILTINS {
            let colors = make().kind_colors();
            for i in 0..colors.len() {
                for j in (i + 1)..colors.len() {
                    assert_ne!(
                        colors[i], colors[j],
                        "{name}: kind colours {i} and {j} collide"
                    );
                }
            }
        }
    }

    #[test]
    fn by_name_round_trips_known_names() {
        for (name, make) in BUILTINS {
            assert_eq!(
                Palette::by_name(name),
                Some(make()),
                "{name} should resolve to its palette"
            );
        }
    }

    #[test]
    fn by_name_rejects_unknown() {
        assert_eq!(Palette::by_name("nonsense"), None);
        assert_eq!(Palette::by_name(""), None);
        assert_eq!(Palette::by_name("Sucher-Dark"), None); // case-sensitive kebab
    }
}
