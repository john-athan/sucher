// Git status signals for the browser gutter (ADR 0004, D2).
//
// PHASE-5 STUB. Only the `GitStatus` classification and its glyph/colour live
// here for now, so `EntryListView` can carry an optional git map and the pane
// renderer can reserve + draw a gutter column. The subprocess reads
// (`git rev-parse` / `git status --porcelain`) and the pure `resolve(...)`
// mapping described in D2 land in the git phase; until then the browser always
// passes `git: None`, so the gutter branch is dormant and no gutter is drawn.

use crate::theme;
use ratatui::style::Color;

/// One entry's git working-tree state, mapped from porcelain `XY` codes (D2).
/// The gutter draws one glyph in one palette colour per variant; a clean file
/// carries no `GitStatus` at all (absent from the map), so it gets no marker.
///
/// The variants are constructed by the phase-git `resolve` step, not yet wired
/// here — hence `dead_code` is allowed on the type for this phase only.
#[allow(dead_code)]
pub enum GitStatus {
    /// Not tracked by git (porcelain `??`).
    Untracked,
    /// Staged addition (`A`).
    Added,
    /// Modified in the work tree or index (`M`).
    Modified,
    /// Deleted (`D`).
    Deleted,
    /// Renamed (`R`).
    Renamed,
    /// Unmerged / conflicted (`U`, `AA`, `DD`, …).
    Conflict,
}

impl GitStatus {
    /// The single-cell marker drawn in the gutter column.
    pub fn glyph(&self) -> &'static str {
        match self {
            GitStatus::Untracked => "?",
            GitStatus::Added => "+",
            GitStatus::Modified => "~",
            GitStatus::Deleted => "-",
            GitStatus::Renamed => "»",
            GitStatus::Conflict => "!",
        }
    }

    /// The palette colour the marker is drawn in — reusing the file-kind roles
    /// so the gutter stays in the same visual family as the rest of the browser.
    pub fn color(&self) -> Color {
        let p = theme::palette();
        match self {
            GitStatus::Untracked => p.dim,
            GitStatus::Added => p.sheet,    // green, like "new/good"
            GitStatus::Modified => p.doc,   // yellow, like "changed"
            GitStatus::Deleted => p.pdf,    // red
            GitStatus::Renamed => p.accent, // cyan accent
            GitStatus::Conflict => p.video, // hot pink/red — hard to miss
        }
    }
}
