// Git status signals for the browser gutter (ADR 0004, D2).
//
// Pure-core / thin-IO (ADR 0001 ethos): the ONLY IO is two `git` subprocess
// calls in [`status_map`]; every mapping and aggregation decision lives in the
// pure, unit-tested [`resolve`] / [`status_from_xy`] below. When git isn't on
// `PATH` or the directory isn't a repo, [`status_map`] returns `None` and the
// browser draws no gutter (the width is reclaimed by the name — see
// `render_entry_list`), so non-repo output is byte-for-byte the pre-git render.

use crate::theme;
use ratatui::style::Color;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// One entry's git working-tree state, mapped from porcelain `XY` codes (D2).
/// The gutter draws one glyph in one palette colour per variant; a clean file
/// carries no `GitStatus` at all (absent from the map), so it gets no marker.
///
/// For a directory entry the state is *aggregated* from its descendants: any
/// tracked change reads as [`GitStatus::Modified`] ("has changes"), a wholly
/// untracked directory as [`GitStatus::Untracked`], and a conflict inside it
/// bubbles up as [`GitStatus::Conflict`] (see [`resolve`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitStatus {
    /// Not tracked by git (porcelain `??`).
    Untracked,
    /// Staged addition / copy (`A`, `C`).
    Added,
    /// Modified or type-changed in the work tree or index (`M`, `T`, `MM`, …).
    Modified,
    /// Deleted (`D`).
    Deleted,
    /// Renamed (`R`).
    Renamed,
    /// Unmerged / conflicted (`U*`, `AA`, `DD`, …).
    Conflict,
}

impl GitStatus {
    /// The single-cell marker drawn in the gutter column. Kept to one display
    /// cell so the reserved two-cell slot (`"X "`) stays column-aligned.
    pub fn glyph(&self) -> &'static str {
        match self {
            GitStatus::Untracked => "?",
            GitStatus::Added => "+",
            GitStatus::Modified => "●",
            GitStatus::Deleted => "✗",
            GitStatus::Renamed => "»",
            GitStatus::Conflict => "!",
        }
    }

    /// The palette colour the marker is drawn in — reusing the file-kind roles
    /// so the gutter stays in the same visual family as the rest of the browser.
    pub fn color(&self) -> Color {
        let p = theme::palette();
        match self {
            GitStatus::Untracked => p.dim,  // muted — not yet part of the tree
            GitStatus::Added => p.sheet,    // green, like "new/good"
            GitStatus::Modified => p.doc,   // yellow, like "changed"
            GitStatus::Deleted => p.pdf,    // red
            GitStatus::Renamed => p.image,  // purple
            GitStatus::Conflict => p.video, // hot pink/red — hard to miss
        }
    }

    /// Aggregation precedence for merging a directory's descendant signals: a
    /// higher number wins. `Conflict > Modified > Added > Deleted > Renamed >
    /// Untracked`, so a directory holding both a tracked-modified and an
    /// untracked child reads as `Modified`, and any conflict inside it wins (D2).
    fn precedence(self) -> u8 {
        match self {
            GitStatus::Conflict => 5,
            GitStatus::Modified => 4,
            GitStatus::Added => 3,
            GitStatus::Deleted => 2,
            GitStatus::Renamed => 1,
            GitStatus::Untracked => 0,
        }
    }
}

/// Build the name→status map for a directory, or `None` when it isn't a git
/// repo (or `git` is absent). The thin-IO boundary: two subprocess calls, then
/// all logic is delegated to the pure [`resolve`].
///
/// 1. `git -C <dir> rev-parse --show-toplevel --show-prefix` — a non-zero exit
///    or spawn error means "not a repo / git missing" ⇒ `None` (no gutter). The
///    second output line is the dir's root-relative *prefix* (empty at the repo
///    root, e.g. `src/` in a nested dir).
/// 2. `git -C <dir> status --porcelain=v1 -z --untracked-files=normal
///    --ignored=no` — NUL-separated, repo-root-relative `XY path` records.
///
/// The records are parsed (handling the `-z` rename quirk) and handed to
/// The repo's HEAD identity for the breadcrumb line (ADR 0004, D2 amendment):
/// which branch (or detached commit) the viewed directory is on, and how far it
/// sits from its upstream. Fetched once per `load()` alongside [`status_map`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RepoHead {
    /// Current branch name; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Abbreviated (7-char) HEAD commit id; `None` on an unborn branch
    /// (porcelain reports `branch.oid (initial)` before the first commit).
    pub oid_short: Option<String>,
    /// `(ahead, behind)` relative to the configured upstream; `None` when no
    /// upstream is set (porcelain omits the `branch.ab` header then).
    pub ahead_behind: Option<(u32, u32)>,
}

/// Read the repo HEAD for `dir`, or `None` when it isn't a repo (or `git` is
/// absent). Thin IO: ONE subprocess — `git status --porcelain=v2 --branch
/// --untracked-files=no -z` — whose `# branch.*` headers carry everything
/// ([`parse_head`] is the pure part). `--untracked-files=no` skips the
/// untracked-file walk, so this stays cheap even in huge dirty trees; the entry
/// records after the headers are simply ignored.
pub fn head_info(dir: &Path) -> Option<RepoHead> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=no",
            "-z",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(parse_head(text.split('\0')))
}

/// PURE: extract [`RepoHead`] from porcelain-v2 NUL-separated fields. Only the
/// `# branch.*` headers matter:
/// - `# branch.oid <oid|(initial)>` — full commit id, shortened to 7 chars;
///   `(initial)` (unborn branch) maps to `None`.
/// - `# branch.head <name|(detached)>` — branch name; `(detached)` maps to `None`.
/// - `# branch.ab +<ahead> -<behind>` — present only with an upstream.
///
/// Non-header fields (the status entries) are skipped, so the same stream that
/// feeds the gutter could feed this too.
pub fn parse_head<'a>(fields: impl Iterator<Item = &'a str>) -> RepoHead {
    let mut head = RepoHead {
        branch: None,
        oid_short: None,
        ahead_behind: None,
    };
    for field in fields {
        let Some(rest) = field.strip_prefix("# branch.") else {
            continue;
        };
        if let Some(oid) = rest.strip_prefix("oid ") {
            if oid != "(initial)" {
                let short: String = oid.chars().take(7).collect();
                head.oid_short = Some(short);
            }
        } else if let Some(name) = rest.strip_prefix("head ") {
            if name != "(detached)" {
                head.branch = Some(name.to_string());
            }
        } else if let Some(ab) = rest.strip_prefix("ab ") {
            // `+<ahead> -<behind>`; malformed input just leaves `None`.
            let mut parts = ab.split(' ');
            let ahead = parts.next().and_then(|s| s.strip_prefix('+')?.parse().ok());
            let behind = parts.next().and_then(|s| s.strip_prefix('-')?.parse().ok());
            if let (Some(a), Some(b)) = (ahead, behind) {
                head.ahead_behind = Some((a, b));
            }
        }
    }
    head
}

/// [`resolve`] with the prefix. Recomputed on every `load()` per D2.
pub fn status_map(dir: &Path) -> Option<HashMap<String, GitStatus>> {
    // 1. Locate the repo and this dir's prefix. Any failure ⇒ not a repo.
    let rev = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel", "--show-prefix"])
        .output()
        .ok()?;
    if !rev.status.success() {
        return None;
    }
    let rev_out = String::from_utf8_lossy(&rev.stdout);
    let mut lines = rev_out.lines();
    let _toplevel = lines.next()?; // line 1: repo root (only used to confirm a repo)
                                   // Line 2: the root-relative prefix. Empty at the repo root; `git` always
                                   // emits it with a trailing `/` for a nested dir (e.g. `src/`).
    let prefix = lines.next().unwrap_or("").trim();

    // 2. Read the porcelain status, NUL-separated so paths are literal.
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignored=no",
        ])
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    let records = parse_porcelain_z(&status.stdout);
    Some(resolve(&records, prefix))
}

/// Parse `-z` (NUL-separated) porcelain v1 into `(xy, path)` records, both as
/// owned `String`s. Each record field is `XY<space>PATH`; a rename/copy record
/// is TWO NUL fields — `XY <new>\0<old>` — so after such a record we consume the
/// following (old-path) field and keep the NEW path, which comes first (D2).
/// Paths are repo-root-relative and, thanks to `-z`, never quoted.
fn parse_porcelain_z(bytes: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(bytes);
    let mut fields = text.split('\0');
    let mut records = Vec::new();
    while let Some(field) = fields.next() {
        // The stream ends with a trailing NUL, yielding a final empty field.
        if field.len() < 3 {
            continue;
        }
        // Bytes 0,1 are the XY status letters and byte 2 is the separating
        // space — all ASCII, so byte 3 is a valid char boundary for the path.
        let xy = &field[..2];
        let path = &field[3..];
        // Renames/copies are index-side (`R`/`C` in the staged column) and carry
        // an extra source field; drop it so it isn't parsed as a bogus record.
        if matches!(xy.as_bytes()[0], b'R' | b'C') {
            let _ = fields.next();
        }
        records.push((xy.to_string(), path.to_string()));
    }
    records
}

/// PURE resolver (no IO): map porcelain records to the entries of ONE directory,
/// identified by its root-relative `prefix` (empty at the repo root).
///
/// For each record whose path is under `prefix`, the prefix is stripped to a
/// remainder relative to the viewed dir:
/// - No `/` in the remainder → a file directly in this dir: it takes the
///   record's own [`status_from_xy`].
/// - A `/` in the remainder → the change is inside a child directory: the first
///   path segment names that child, which is marked with a *directory signal* —
///   [`GitStatus::Untracked`] for an untracked entry (`??`, incl. the wholly
///   untracked `?? sub/` form), [`GitStatus::Conflict`] for a conflict, else
///   [`GitStatus::Modified`] (any tracked change collapses to "has changes").
///
/// When several records touch the same name, the higher [`GitStatus::precedence`]
/// wins, so a directory with both modified and untracked children reads as
/// `Modified`. Records outside `prefix` are ignored.
pub fn resolve(records: &[(String, String)], prefix: &str) -> HashMap<String, GitStatus> {
    let mut map: HashMap<String, GitStatus> = HashMap::new();
    for (xy, path) in records {
        let Some(rest) = path.strip_prefix(prefix) else {
            continue; // outside the viewed dir — ignore
        };
        if rest.is_empty() {
            continue;
        }
        let (name, status) = match rest.find('/') {
            None => (rest.to_string(), status_from_xy(xy)),
            Some(slash) => (rest[..slash].to_string(), dir_signal(status_from_xy(xy))),
        };
        merge(&mut map, name, status);
    }
    map
}

/// Insert `status` for `name`, keeping whichever of the existing/new status has
/// the higher [`GitStatus::precedence`] (the directory-aggregation rule, D2).
fn merge(map: &mut HashMap<String, GitStatus>, name: String, status: GitStatus) {
    map.entry(name)
        .and_modify(|cur| {
            if status.precedence() > cur.precedence() {
                *cur = status;
            }
        })
        .or_insert(status);
}

/// Collapse a descendant's own status into the signal it contributes to its
/// containing directory: untracked and conflict pass through (so a wholly
/// untracked child dir reads `Untracked` and a conflict bubbles up), every other
/// tracked change becomes `Modified` — a single, predictable "has changes" read.
fn dir_signal(child: GitStatus) -> GitStatus {
    match child {
        GitStatus::Untracked => GitStatus::Untracked,
        GitStatus::Conflict => GitStatus::Conflict,
        _ => GitStatus::Modified,
    }
}

/// Map a porcelain two-char `XY` code (staged `X`, worktree `Y`) to a
/// [`GitStatus`]. The rule, applied in order:
/// 1. `??` → `Untracked`.
/// 2. Any unmerged code → `Conflict`: a `U` in either column, or the both-sides
///    `AA` / `DD` pairs (covering `AU`, `UD`, `UA`, `DU`, `UU`, `AA`, `DD`).
/// 3. Otherwise pick the more meaningful column — the worktree `Y` if it isn't a
///    space, else the staged `X` — and map that letter: `A`/`C` → `Added`,
///    `D` → `Deleted`, `R` → `Renamed`, `M`/`T` (and anything else) → `Modified`.
fn status_from_xy(xy: &str) -> GitStatus {
    let bytes = xy.as_bytes();
    let x = bytes.first().copied().unwrap_or(b' ');
    let y = bytes.get(1).copied().unwrap_or(b' ');

    if xy == "??" {
        return GitStatus::Untracked;
    }
    if is_conflict(x, y) {
        return GitStatus::Conflict;
    }
    let letter = if y != b' ' { y } else { x };
    match letter {
        b'A' | b'C' => GitStatus::Added,
        b'D' => GitStatus::Deleted,
        b'R' => GitStatus::Renamed,
        _ => GitStatus::Modified, // M, T, and any residual tracked change
    }
}

/// Is `XY` an unmerged (conflict) code? A `U` on either side, or the both-added
/// / both-deleted pairs — the full porcelain conflict set (D2).
fn is_conflict(x: u8, y: u8) -> bool {
    x == b'U' || y == b'U' || (x == b'A' && y == b'A') || (x == b'D' && y == b'D')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(xy: &str, path: &str) -> (String, String) {
        (xy.to_string(), path.to_string())
    }

    #[test]
    fn xy_maps_the_status_table() {
        assert_eq!(status_from_xy("??"), GitStatus::Untracked);
        // Worktree column wins when non-space.
        assert_eq!(status_from_xy(" M"), GitStatus::Modified);
        assert_eq!(status_from_xy("M "), GitStatus::Modified);
        assert_eq!(status_from_xy("MM"), GitStatus::Modified);
        assert_eq!(status_from_xy(" T"), GitStatus::Modified); // type change
        assert_eq!(status_from_xy("A "), GitStatus::Added);
        assert_eq!(status_from_xy("C "), GitStatus::Added); // copy → added
        assert_eq!(status_from_xy(" D"), GitStatus::Deleted);
        assert_eq!(status_from_xy("D "), GitStatus::Deleted);
        assert_eq!(status_from_xy("R "), GitStatus::Renamed);
        // Staged add + worktree modify: Y non-space wins → Modified.
        assert_eq!(status_from_xy("AM"), GitStatus::Modified);
    }

    #[test]
    fn xy_detects_every_conflict_code() {
        for code in ["DD", "AU", "UD", "UA", "DU", "AA", "UU"] {
            assert_eq!(status_from_xy(code), GitStatus::Conflict, "code {code}");
        }
    }

    #[test]
    fn resolve_file_directly_in_dir_at_repo_root() {
        // Empty prefix = repo root; a top-level file keyed by its bare name.
        let recs = [rec(" M", "README.md")];
        let map = resolve(&recs, "");
        assert_eq!(map.get("README.md"), Some(&GitStatus::Modified));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn resolve_untracked_file() {
        let recs = [rec("??", "notes.txt")];
        let map = resolve(&recs, "");
        assert_eq!(map.get("notes.txt"), Some(&GitStatus::Untracked));
    }

    #[test]
    fn resolve_change_in_subdir_aggregates_to_modified() {
        // A modified file deep under `src/` marks the `src` directory entry.
        let recs = [rec(" M", "src/dir/mod.rs")];
        let map = resolve(&recs, "");
        assert_eq!(map.get("src"), Some(&GitStatus::Modified));
        assert!(!map.contains_key("mod.rs"));
    }

    #[test]
    fn resolve_wholly_untracked_subdir_is_untracked() {
        // Porcelain collapses a fully-untracked dir to `?? sub/`.
        let recs = [rec("??", "sub/")];
        let map = resolve(&recs, "");
        assert_eq!(map.get("sub"), Some(&GitStatus::Untracked));
    }

    #[test]
    fn resolve_precedence_modified_beats_untracked_in_a_dir() {
        // One tracked-modified child + one untracked child → the dir reads
        // Modified (Modified outranks Untracked), regardless of record order.
        let recs = [rec("??", "pkg/new.rs"), rec(" M", "pkg/old.rs")];
        let map = resolve(&recs, "");
        assert_eq!(map.get("pkg"), Some(&GitStatus::Modified));

        let recs_rev = [rec(" M", "pkg/old.rs"), rec("??", "pkg/new.rs")];
        assert_eq!(
            resolve(&recs_rev, "").get("pkg"),
            Some(&GitStatus::Modified)
        );
    }

    #[test]
    fn resolve_conflict_in_subdir_bubbles_up() {
        let recs = [rec(" M", "pkg/a.rs"), rec("UU", "pkg/b.rs")];
        // Conflict outranks Modified in the aggregation.
        assert_eq!(resolve(&recs, "").get("pkg"), Some(&GitStatus::Conflict));
    }

    #[test]
    fn resolve_honours_a_nested_prefix() {
        // Viewing `src/`: prefix strips, so `src/lib.rs` keys as `lib.rs`, and a
        // change under `src/ui/` marks the child dir `ui`.
        let recs = [
            rec("M ", "src/lib.rs"),
            rec("??", "src/ui/new.rs"),
            rec(" M", "docs/guide.md"), // outside the prefix
        ];
        let map = resolve(&recs, "src/");
        assert_eq!(map.get("lib.rs"), Some(&GitStatus::Modified));
        assert_eq!(map.get("ui"), Some(&GitStatus::Untracked));
        assert!(
            !map.contains_key("guide.md"),
            "outside-prefix record leaked in"
        );
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn resolve_ignores_records_outside_the_prefix() {
        let recs = [rec(" M", "other/file.rs")];
        assert!(resolve(&recs, "src/").is_empty());
    }

    #[test]
    fn parse_porcelain_z_handles_rename_two_field_record() {
        // `R  new\0old\0` — keep the NEW path, discard the old source field.
        let bytes = b"R  new.rs\x00old.rs\x00 M other.rs\x00";
        let recs = parse_porcelain_z(bytes);
        assert_eq!(
            recs,
            vec![
                ("R ".to_string(), "new.rs".to_string()),
                (" M".to_string(), "other.rs".to_string()),
            ]
        );
    }

    #[test]
    fn parse_head_reads_branch_oid_and_ab() {
        let fields = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaa bbb src/lib.rs", // entry: ignored
        ];
        let head = parse_head(fields.into_iter());
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert_eq!(head.oid_short.as_deref(), Some("0123456"));
        assert_eq!(head.ahead_behind, Some((2, 1)));
    }

    #[test]
    fn parse_head_detached_has_no_branch() {
        let fields = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head (detached)",
        ];
        let head = parse_head(fields.into_iter());
        assert_eq!(head.branch, None);
        assert_eq!(head.oid_short.as_deref(), Some("0123456"));
        assert_eq!(head.ahead_behind, None);
    }

    #[test]
    fn parse_head_unborn_branch_has_no_oid() {
        // Fresh `git init`: a named branch with no commit yet.
        let fields = ["# branch.oid (initial)", "# branch.head main"];
        let head = parse_head(fields.into_iter());
        assert_eq!(head.branch.as_deref(), Some("main"));
        assert_eq!(head.oid_short, None);
        assert_eq!(head.ahead_behind, None);
    }

    #[test]
    fn parse_head_no_upstream_leaves_ab_none() {
        let fields = [
            "# branch.oid 0123456789abcdef0123456789abcdef01234567",
            "# branch.head feat/x",
        ];
        assert_eq!(parse_head(fields.into_iter()).ahead_behind, None);
    }

    #[test]
    fn parse_porcelain_z_reads_simple_records() {
        let bytes = b" M a.rs\x00?? b.rs\x00";
        let recs = parse_porcelain_z(bytes);
        assert_eq!(
            recs,
            vec![
                (" M".to_string(), "a.rs".to_string()),
                ("??".to_string(), "b.rs".to_string()),
            ]
        );
    }
}
