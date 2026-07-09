// User configuration: the one place Sucher resolves "what theme and icons do I
// use", from (highest priority first) a CLI flag, an environment variable, a
// TOML file, and finally a built-in default (ADR 0003, D2). The output is a
// fully-resolved [`Config`] — the theme is already a concrete [`Palette`], not a
// name — so the rest of the app never re-derives it.
//
// A missing or malformed config file is deliberately non-fatal: a broken TOML
// line must never stop you opening a file, so every parse error silently falls
// back to the defaults (D2). The only IO here is reading that one file and, for
// `theme = "auto"`, a short best-effort background probe (D4).

use crate::theme::Palette;
use ratatui::style::Color;
use serde::Deserialize;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

/// The resolved configuration the app runs with. The theme is already a
/// concrete [`Palette`] (auto-detection and name lookup done); `icons` selects
/// the browser's glyph rendering (its use lands in a later phase — D5).
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub palette: Palette,
    pub icons: IconMode,
    pub layout: Layout,
    /// Whether the browser shows the git-status gutter (ADR 0004, D2). When
    /// off, the current pane never runs `git` and the gutter is fully absent.
    pub git: bool,
    /// Whether the browser captures the mouse for pointer navigation (ADR 0005,
    /// D2): clickable breadcrumb segments and wheel scrolling. On by default;
    /// `false` keeps the terminal's native click-drag text selection.
    pub mouse: bool,
    /// Whether navigation animations run (ADR 0006, D4): the ~120 ms fade-in of
    /// the current pane on a directory change (and, in a later phase, the
    /// full-view zoom). On by default; `false` makes every transition instant and
    /// no animation code executes — behaviour is byte-for-byte the pre-feature UI.
    pub animate: bool,
}

/// How the browser draws the per-entry glyph column (ADR 0003, D5). Nerd Font
/// presence can't be detected from inside a process, so it is a user choice with
/// a safe `Unicode` default that renders everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum IconMode {
    /// The built-in geometric glyphs — the default, renders in any font.
    #[default]
    Unicode,
    /// Per-extension Nerd Font glyphs (opt-in; needs a patched font).
    Nerd,
    /// No glyph column at all.
    None,
}

impl FromStr for IconMode {
    type Err = ();
    /// Parse the config/env/flag spelling. Case-insensitive; unknown → `Err`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "unicode" => Ok(IconMode::Unicode),
            "nerd" => Ok(IconMode::Nerd),
            "none" => Ok(IconMode::None),
            _ => Err(()),
        }
    }
}

/// Which pane layout the browser composes (ADR 0004, D1). `Auto` is Miller when
/// the frame is wide (and a parent exists), double-pane when narrow; `Miller`
/// and `Double` force the choice. The runtime `M` key cycles between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Layout {
    /// Miller when wide enough, double-pane when narrow — the friendly default.
    #[default]
    Auto,
    /// Always attempt `parent | current | preview` (still collapses to two when
    /// the frame is too narrow or there is no parent).
    Miller,
    /// Always the classic `current | preview` two-pane layout.
    Double,
}

impl FromStr for Layout {
    type Err = ();
    /// Parse the config/env/flag spelling. Case-insensitive; unknown → `Err`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "auto" => Ok(Layout::Auto),
            "miller" => Ok(Layout::Miller),
            "double" => Ok(Layout::Double),
            _ => Err(()),
        }
    }
}

impl Layout {
    /// The next layout for the runtime `M` toggle: `auto → miller → double → auto`.
    pub fn cycle(self) -> Self {
        match self {
            Layout::Auto => Layout::Miller,
            Layout::Miller => Layout::Double,
            Layout::Double => Layout::Auto,
        }
    }
}

/// The TOML file's shape. Everything is optional so a partial (or empty) file is
/// valid; unknown top-level keys are ignored by `serde` (no `deny_unknown`).
#[derive(Deserialize, Default)]
struct FileConfig {
    theme: Option<String>,
    icons: Option<String>,
    layout: Option<String>,
    /// Whether to draw the git gutter (ADR 0004, D2); default on when omitted.
    git: Option<bool>,
    /// Whether to capture the mouse (ADR 0005, D2); default on when omitted.
    mouse: Option<bool>,
    /// Whether navigation animations run (ADR 0006, D4); default on when omitted.
    animate: Option<bool>,
    /// Optional per-key hex overrides, applied last over the resolved palette.
    colors: Option<std::collections::HashMap<String, String>>,
}

/// Resolve the final [`Config`]. Precedence for each setting, highest first:
/// CLI flag → env (`SUCHER_THEME` / `SUCHER_ICONS`) → TOML file → built-in
/// default (`sucher-dark`, `unicode`). `[colors]` hex overrides from the file
/// are applied last, on top of whichever base palette won.
pub fn load(
    cli_theme: Option<String>,
    cli_icons: Option<String>,
    cli_layout: Option<String>,
    cli_git: Option<bool>,
    cli_mouse: Option<bool>,
    cli_animate: Option<bool>,
) -> Config {
    let file = read_file_config().unwrap_or_default();
    let env_theme = std::env::var("SUCHER_THEME").ok();
    let env_icons = std::env::var("SUCHER_ICONS").ok();
    let env_layout = std::env::var("SUCHER_LAYOUT").ok();
    let env_git = std::env::var("SUCHER_GIT").ok();
    let env_mouse = std::env::var("SUCHER_MOUSE").ok();
    let env_animate = std::env::var("SUCHER_ANIMATE").ok();

    let theme_name = resolve_theme_name(cli_theme, env_theme, file.theme.clone());
    let icons = resolve_icons(cli_icons, env_icons, file.icons.clone());
    let layout = resolve_layout(cli_layout, env_layout, file.layout.clone());
    let git = resolve_git(cli_git, env_git, file.git);
    let mouse = resolve_mouse(cli_mouse, env_mouse, file.mouse);
    let animate = resolve_animate(cli_animate, env_animate, file.animate);

    let mut palette = resolve_palette(&theme_name);
    apply_color_overrides(&mut palette, file.colors.as_ref());

    Config {
        palette,
        icons,
        layout,
        git,
        mouse,
        animate,
    }
}

/// Pure theme-name precedence: `cli` → `env` → `file` → `sucher-dark`. Kept free
/// of IO so the precedence rule itself is unit-tested.
fn resolve_theme_name(cli: Option<String>, env: Option<String>, file: Option<String>) -> String {
    cli.or(env)
        .or(file)
        .unwrap_or_else(|| "sucher-dark".to_string())
}

/// Pure icon-mode precedence: `cli` → `env` → `file` → `Unicode`. An
/// unparseable value at any level falls through to the default rather than
/// erroring, so a typo never blanks the glyph column.
fn resolve_icons(cli: Option<String>, env: Option<String>, file: Option<String>) -> IconMode {
    cli.or(env)
        .or(file)
        .and_then(|s| IconMode::from_str(&s).ok())
        .unwrap_or_default()
}

/// Pure layout precedence: `cli` → `env` → `file` → `Auto`. Like icons, an
/// unparseable value at the winning level falls through to the default rather
/// than erroring, so a typo never wedges the layout.
fn resolve_layout(cli: Option<String>, env: Option<String>, file: Option<String>) -> Layout {
    cli.or(env)
        .or(file)
        .and_then(|s| Layout::from_str(&s).ok())
        .unwrap_or_default()
}

/// Pure git-toggle precedence: `cli` (the `--no-git` flag) → env (`SUCHER_GIT`)
/// → file → default `true`. The env value is parsed by [`parse_bool`]; an
/// unrecognised spelling falls through to the file/default rather than
/// erroring, so a typo never silently disables the gutter unexpectedly.
fn resolve_git(cli: Option<bool>, env: Option<String>, file: Option<bool>) -> bool {
    if let Some(c) = cli {
        return c;
    }
    if let Some(b) = env.as_deref().and_then(parse_bool) {
        return b;
    }
    file.unwrap_or(true)
}

/// Pure mouse-toggle precedence: `cli` (the `--no-mouse` flag) → env
/// (`SUCHER_MOUSE`) → file → default `true`. Mirrors [`resolve_git`] exactly,
/// reusing [`parse_bool`]; an unrecognised env spelling falls through to the
/// file/default rather than erroring, so a typo never silently disables mouse.
fn resolve_mouse(cli: Option<bool>, env: Option<String>, file: Option<bool>) -> bool {
    if let Some(c) = cli {
        return c;
    }
    if let Some(b) = env.as_deref().and_then(parse_bool) {
        return b;
    }
    file.unwrap_or(true)
}

/// Pure animate-toggle precedence: `cli` (the `--no-animate` flag) → env
/// (`SUCHER_ANIMATE`) → file → default `true` (ADR 0006, D4). Mirrors
/// [`resolve_mouse`]/[`resolve_git`] exactly, reusing [`parse_bool`]; an
/// unrecognised env spelling falls through to the file/default rather than
/// erroring, so a typo never silently disables animations unexpectedly.
fn resolve_animate(cli: Option<bool>, env: Option<String>, file: Option<bool>) -> bool {
    if let Some(c) = cli {
        return c;
    }
    if let Some(b) = env.as_deref().and_then(parse_bool) {
        return b;
    }
    file.unwrap_or(true)
}

/// Parse a boolean config/env spelling, case-insensitively: `1/true/yes/on` →
/// `true`, `0/false/no/off` → `false`, anything else → `None`.
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Turn a theme name into a [`Palette`]. `"auto"` picks dark/light from the
/// terminal background (D4); any other name is looked up, falling back to
/// `sucher-dark` if it isn't a built-in.
fn resolve_palette(name: &str) -> Palette {
    if name == "auto" {
        return if detect_dark() {
            Palette::sucher_dark()
        } else {
            Palette::sucher_light()
        };
    }
    Palette::by_name(name).unwrap_or_else(Palette::sucher_dark)
}

/// Apply `[colors]` hex overrides onto an already-resolved palette. Unknown keys
/// and un-parseable hex are ignored silently (a typo shouldn't blank a colour).
fn apply_color_overrides(
    p: &mut Palette,
    colors: Option<&std::collections::HashMap<String, String>>,
) {
    let Some(colors) = colors else { return };
    for (key, val) in colors {
        let Some(c) = parse_hex(val) else { continue };
        match key.as_str() {
            "dir" => p.dir = c,
            "image" => p.image = c,
            "video" => p.video = c,
            "pdf" => p.pdf = c,
            "sheet" => p.sheet = c,
            "doc" => p.doc = c,
            "code" => p.code = c,
            "archive" => p.archive = c,
            "other" => p.other = c,
            "dim" => p.dim = c,
            "accent" => p.accent = c,
            "selection" => p.selection = c,
            "keyword" => p.keyword = c,
            _ => {}
        }
    }
}

/// Read and parse the config file, or `None` if it is absent or malformed.
/// Never propagates an error — a broken config uses defaults (D2).
fn read_file_config() -> Option<FileConfig> {
    let path = config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// `$XDG_CONFIG_HOME/sucher/config.toml`, else `~/.config/sucher/config.toml`.
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(base.join("sucher").join("config.toml"))
}

/// Parse `#rrggbb` or `rrggbb` into an RGB [`Color`]; `None` for anything else.
fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

// --- Light/dark background detection (D4) --------------------------------------
//
// The decision is a pure function of an (r,g,b) background; only the source of
// that background touches the terminal. Order: `COLORFGBG` (cheap, no IO), then
// a short OSC 11 query, then assume dark. Every failure path returns dark, the
// safe default for a viewer built around a dark palette.

/// Is a background colour dark? Rec. 601 luma below the midpoint (D4).
/// A pure function so the policy is unit-testable without a terminal.
pub fn is_dark(r: u8, g: u8, b: u8) -> bool {
    let luma = 0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32;
    luma < 128.0
}

/// Best-effort "is the terminal background dark?", run once before the alternate
/// screen. Reads `COLORFGBG` if set, else queries OSC 11 with a short timeout,
/// else assumes dark. Any error or timeout → `true`.
pub fn detect_dark() -> bool {
    if let Some(dark) = from_colorfgbg() {
        return dark;
    }
    if let Some((r, g, b)) = query_osc11() {
        return is_dark(r, g, b);
    }
    true
}

/// Interpret `COLORFGBG` (e.g. `"15;0"` or `"15;default;0"`): the last field is
/// the background as an ANSI colour index. Indices 0–6 and 8 are the dark
/// backgrounds. `None` when the variable is absent or unparseable.
fn from_colorfgbg() -> Option<bool> {
    let raw = std::env::var("COLORFGBG").ok()?;
    let bg = raw.rsplit(';').next()?.trim();
    let idx: u32 = bg.parse().ok()?;
    Some(matches!(idx, 0..=6 | 8))
}

/// Query the terminal background via OSC 11 and parse its `rgb:…` reply.
///
/// Writes `ESC ] 11 ; ? BEL` to the controlling terminal and reads the answer
/// on a helper thread guarded by a ~100 ms timeout, so it can never hang. Only
/// attempted when both stdin and stdout are TTYs. `None` on any failure/timeout.
///
/// Caveat (shared with `termbg`-style probes): if the terminal never answers,
/// the helper read stays parked; this runs once at startup before the UI draws,
/// and terminals that ignore OSC 11 also send nothing later, so in practice it
/// idles harmlessly rather than stealing input.
fn query_osc11() -> Option<(u8, u8, u8)> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }

    // Raw mode so the reply arrives byte-by-byte, not line-buffered. Restored on
    // every exit path by the guard.
    crossterm::terminal::enable_raw_mode().ok()?;
    struct RawGuard;
    impl Drop for RawGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let _guard = RawGuard;

    {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b]11;?\x07").ok()?;
        out.flush().ok()?;
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        while stdin.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
            buf.push(byte[0]);
            // Terminators: BEL, or ST (`ESC \`). Cap length as a backstop.
            if byte[0] == 0x07
                || (buf.len() >= 2 && buf[buf.len() - 2] == 0x1b && byte[0] == b'\\')
                || buf.len() > 64
            {
                break;
            }
        }
        let _ = tx.send(buf);
    });

    let reply = rx.recv_timeout(Duration::from_millis(100)).ok()?;
    parse_osc11(&reply)
}

/// Parse an OSC 11 reply body, extracting the `rgb:RRRR/GGGG/BBBB` colour and
/// scaling each (1–4 hex-digit) component down to 8 bits. `None` if malformed.
fn parse_osc11(reply: &[u8]) -> Option<(u8, u8, u8)> {
    let s = std::str::from_utf8(reply).ok()?;
    let after = s.split("rgb:").nth(1)?;
    // Component list ends at the terminator (BEL / ESC) if present.
    let list = after.split(['\x07', '\x1b']).next()?.trim();
    let mut parts = list.split('/');
    let r = scale_component(parts.next()?)?;
    let g = scale_component(parts.next()?)?;
    let b = scale_component(parts.next()?)?;
    Some((r, g, b))
}

/// Scale a 1–4 hex-digit colour component to an 8-bit value (e.g. `ffff`→255,
/// `00`→0). `None` if empty or non-hex.
fn scale_component(g: &str) -> Option<u8> {
    let g = g.trim();
    if g.is_empty() || g.len() > 4 || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(g, 16).ok()?;
    let max = (1u32 << (4 * g.len())) - 1;
    Some((v * 255 / max) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_mode_parses_case_insensitively() {
        assert_eq!(IconMode::from_str("unicode"), Ok(IconMode::Unicode));
        assert_eq!(IconMode::from_str("Nerd"), Ok(IconMode::Nerd));
        assert_eq!(IconMode::from_str(" NONE "), Ok(IconMode::None));
        assert_eq!(IconMode::from_str("sparkles"), Err(()));
        assert_eq!(IconMode::default(), IconMode::Unicode);
    }

    #[test]
    fn parse_hex_accepts_both_spellings_and_rejects_junk() {
        assert_eq!(parse_hex("#7dd3fc"), Some(Color::Rgb(0x7d, 0xd3, 0xfc)));
        assert_eq!(parse_hex("7dd3fc"), Some(Color::Rgb(0x7d, 0xd3, 0xfc)));
        assert_eq!(parse_hex("  #FFFFFF "), Some(Color::Rgb(255, 255, 255)));
        assert_eq!(parse_hex("#fff"), None); // too short
        assert_eq!(parse_hex("#gggggg"), None); // non-hex
        assert_eq!(parse_hex("nope"), None);
    }

    #[test]
    fn is_dark_black_white_and_boundary() {
        assert!(is_dark(0, 0, 0)); // black → dark
        assert!(!is_dark(255, 255, 255)); // white → light
                                          // Mid-grey 127 luma = 127 < 128 → dark; 128 → light.
        assert!(is_dark(127, 127, 127));
        assert!(!is_dark(128, 128, 128));
    }

    #[test]
    fn scale_component_widths() {
        assert_eq!(scale_component("ffff"), Some(255));
        assert_eq!(scale_component("0000"), Some(0));
        assert_eq!(scale_component("ff"), Some(255));
        assert_eq!(scale_component("00"), Some(0));
        assert_eq!(scale_component(""), None);
        assert_eq!(scale_component("xyz"), None);
    }

    #[test]
    fn parse_osc11_extracts_rgb() {
        let reply = b"\x1b]11;rgb:1a1a/1b1b/2626\x07";
        assert_eq!(parse_osc11(reply), Some((0x1a, 0x1b, 0x26)));
    }

    #[test]
    fn colorfgbg_last_field_is_background() {
        // Values are ANSI indices: 0 = black bg (dark), 15 = white bg (light).
        let saved = std::env::var("COLORFGBG").ok();
        std::env::set_var("COLORFGBG", "15;0");
        assert_eq!(from_colorfgbg(), Some(true));
        std::env::set_var("COLORFGBG", "0;15");
        assert_eq!(from_colorfgbg(), Some(false));
        std::env::set_var("COLORFGBG", "15;default;0");
        assert_eq!(from_colorfgbg(), Some(true));
        std::env::set_var("COLORFGBG", "garbage");
        assert_eq!(from_colorfgbg(), None);
        match saved {
            Some(v) => std::env::set_var("COLORFGBG", v),
            None => std::env::remove_var("COLORFGBG"),
        }
    }

    #[test]
    fn theme_precedence_cli_over_env_over_file_over_default() {
        // CLI wins outright.
        assert_eq!(
            resolve_theme_name(
                Some("tokyo-night".into()),
                Some("gruvbox-dark".into()),
                Some("sucher-light".into())
            ),
            "tokyo-night"
        );
        // No CLI → env wins over the file.
        assert_eq!(
            resolve_theme_name(
                None,
                Some("gruvbox-dark".into()),
                Some("sucher-light".into())
            ),
            "gruvbox-dark"
        );
        // No CLI/env → the file value.
        assert_eq!(
            resolve_theme_name(None, None, Some("sucher-light".into())),
            "sucher-light"
        );
        // Nothing set → the built-in default.
        assert_eq!(resolve_theme_name(None, None, None), "sucher-dark");
    }

    #[test]
    fn icons_precedence_and_default() {
        assert_eq!(
            resolve_icons(
                Some("nerd".into()),
                Some("none".into()),
                Some("unicode".into())
            ),
            IconMode::Nerd
        );
        // env over file.
        assert_eq!(
            resolve_icons(None, Some("none".into()), Some("unicode".into())),
            IconMode::None
        );
        // Unparseable at the winning level → default (does not fall through to file).
        assert_eq!(
            resolve_icons(None, Some("bogus".into()), Some("nerd".into())),
            IconMode::Unicode
        );
        // Nothing set → default.
        assert_eq!(resolve_icons(None, None, None), IconMode::Unicode);
    }

    #[test]
    fn layout_parses_case_insensitively() {
        assert_eq!(Layout::from_str("auto"), Ok(Layout::Auto));
        assert_eq!(Layout::from_str("Miller"), Ok(Layout::Miller));
        assert_eq!(Layout::from_str(" DOUBLE "), Ok(Layout::Double));
        assert_eq!(Layout::from_str("triple"), Err(()));
        assert_eq!(Layout::default(), Layout::Auto);
    }

    #[test]
    fn layout_cycle_wraps_auto_miller_double() {
        assert_eq!(Layout::Auto.cycle(), Layout::Miller);
        assert_eq!(Layout::Miller.cycle(), Layout::Double);
        assert_eq!(Layout::Double.cycle(), Layout::Auto);
    }

    #[test]
    fn layout_precedence_and_default() {
        // CLI wins outright.
        assert_eq!(
            resolve_layout(
                Some("miller".into()),
                Some("double".into()),
                Some("auto".into())
            ),
            Layout::Miller
        );
        // env over file.
        assert_eq!(
            resolve_layout(None, Some("double".into()), Some("auto".into())),
            Layout::Double
        );
        // No CLI/env → the file value.
        assert_eq!(
            resolve_layout(None, None, Some("miller".into())),
            Layout::Miller
        );
        // Unparseable at the winning level → default (does not fall through to file).
        assert_eq!(
            resolve_layout(None, Some("bogus".into()), Some("miller".into())),
            Layout::Auto
        );
        // Nothing set → default.
        assert_eq!(resolve_layout(None, None, None), Layout::Auto);
    }

    #[test]
    fn parse_bool_accepts_the_spellings() {
        for s in ["1", "true", "YES", " on ", "True"] {
            assert_eq!(parse_bool(s), Some(true), "{s}");
        }
        for s in ["0", "false", "NO", " off ", "False"] {
            assert_eq!(parse_bool(s), Some(false), "{s}");
        }
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn git_precedence_and_default() {
        // CLI (the --no-git flag) wins outright.
        assert_eq!(
            resolve_git(Some(false), Some("true".into()), Some(true)),
            false
        );
        // No CLI → a recognised env spelling wins over the file.
        assert_eq!(resolve_git(None, Some("no".into()), Some(true)), false);
        // Unparseable env → falls through to the file value (forgiving).
        assert_eq!(resolve_git(None, Some("bogus".into()), Some(false)), false);
        // No CLI/env → the file value.
        assert_eq!(resolve_git(None, None, Some(false)), false);
        // Nothing set → the built-in default (on).
        assert_eq!(resolve_git(None, None, None), true);
    }

    #[test]
    fn mouse_precedence_and_default() {
        // CLI (the --no-mouse flag) wins outright.
        assert_eq!(
            resolve_mouse(Some(false), Some("true".into()), Some(true)),
            false
        );
        // No CLI → a recognised env spelling wins over the file.
        assert_eq!(resolve_mouse(None, Some("off".into()), Some(true)), false);
        // Unparseable env → falls through to the file value (forgiving).
        assert_eq!(
            resolve_mouse(None, Some("bogus".into()), Some(false)),
            false
        );
        // No CLI/env → the file value.
        assert_eq!(resolve_mouse(None, None, Some(false)), false);
        // Nothing set → the built-in default (on).
        assert_eq!(resolve_mouse(None, None, None), true);
    }

    #[test]
    fn animate_precedence_and_default() {
        // CLI (the --no-animate flag) wins outright.
        assert_eq!(
            resolve_animate(Some(false), Some("true".into()), Some(true)),
            false
        );
        // No CLI → a recognised env spelling wins over the file.
        assert_eq!(resolve_animate(None, Some("off".into()), Some(true)), false);
        // Unparseable env → falls through to the file value (forgiving).
        assert_eq!(
            resolve_animate(None, Some("bogus".into()), Some(false)),
            false
        );
        // No CLI/env → the file value.
        assert_eq!(resolve_animate(None, None, Some(false)), false);
        // Nothing set → the built-in default (on).
        assert_eq!(resolve_animate(None, None, None), true);
    }

    #[test]
    fn overrides_apply_over_base_and_ignore_junk() {
        let mut p = Palette::sucher_dark();
        let mut map = std::collections::HashMap::new();
        map.insert("accent".to_string(), "#123456".to_string());
        map.insert("dir".to_string(), "not-a-color".to_string()); // ignored
        map.insert("bogus".to_string(), "#ffffff".to_string()); // unknown key
        apply_color_overrides(&mut p, Some(&map));
        assert_eq!(p.accent, Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(p.dir, Palette::sucher_dark().dir); // bad hex left untouched
    }

    #[test]
    fn malformed_toml_yields_no_file_config() {
        // The file reader swallows parse errors; simulate by parsing directly.
        let broken: Result<FileConfig, _> = toml::from_str("theme = = =");
        assert!(broken.is_err());
        // load() would fall back to defaults in this case.
    }
}
