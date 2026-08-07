//! File operations on whole paths: copy, move, rename, create, trash (ADR 0017).
//!
//! Sucher is a viewfinder. It changes *where files are*, never *what they
//! contain* (ADR 0017 D1), so this module deals in paths and never opens a byte
//! for inspection. The work is split into three stages, and the split is the
//! whole design:
//!
//! ```text
//!   collect  ->  plan  ->  execute
//!   (impure)    (pure)     (impure)
//!                             ^
//!   journal  ---------------  |  execute_undo
//! ```
//!
//!   * [`collect`] is the one place that looks at the filesystem before an
//!     operation. It turns the selected paths into fully enumerated [`Source`]
//!     trees and reports the ones that vanished since they were marked. It is
//!     deliberately thin: it decides nothing, it only finds out.
//!   * [`plan`] is pure. Given those sources plus a [`PlanCtx`] snapshot of the
//!     world, it produces a [`Plan`] in which every source already knows its
//!     final destination name, or a [`Refusal`] naming the reason it will not
//!     happen. No filesystem, no clock.
//!   * `execute` (in `exec`) replays a plan on a background thread and streams
//!     progress, having no decisions left to make.
//!
//! Undo is a fourth thing, and it enters at the executor rather than at the top:
//! it has no paths to collect and nothing to decide, because the [`Journal`] a
//! finished run left behind already names every step that actually happened
//! (ADR 0017 D8). [`start_undo`] therefore feeds that journal to the *same*
//! worker, streaming the same [`Msg`] and ending in the same [`Report`] as a
//! forward run. That is not tidiness: reversing a cross-device move copies a
//! whole tree home, so an undo run inline from the key handler would freeze the
//! browser for as long as the original move took, and the browser would need a
//! second message pump beside the one it already has.
//!
//! The pure seam exists because the difficult part of a file manager is the
//! decision matrix, not the syscall: name collisions, a destination nested
//! inside its own source, the user standing in the directory being moved. With
//! `plan` pure, that entire matrix is unit-tested with no temp directory, and
//! the confirm overlay can show the exact outcome before a byte moves
//! (ADR 0017 D5).
//!
//! Both bounds and both refusals follow ADR 0009's doctrine: an honest error,
//! never a silent partial. A tree past [`MAX_TREE_ITEMS`] or [`MAX_TREE_DEPTH`]
//! is refused *before* anything is mutated rather than copied halfway.

mod exec;
mod plan;

// The front door: the browser reaches the whole engine through `fileop::`, not
// through its two private halves.
pub use plan::{plan, Conflict, Kind, Node, NodeKind, Op, Plan, PlanCtx, Refusal, Source, Step};
// The executor's surface. Both directions leave through the same door and hand
// back the same [`Run`], so the browser drives an undo with the pump it already
// wrote for a paste.
//
// The `Trash` and `Rename` seams themselves stay private to `exec`: the browser
// asks for an operation, it does not get to choose where deleted things go,
// which is what keeps ADR 0017 D7 a property of the engine rather than a
// convention the callers have to remember.
// `Failure` is named only by the browser's tests, which build one to check how a
// report renders; the browser itself reads `Report::failures` and its fields
// without ever spelling the type. So the export is real but unused outside test
// builds, and the narrow `cfg_attr` says exactly that, the same way `format.rs`
// scopes its allow to the builds where the item genuinely has no caller.
#[cfg_attr(not(test), allow(unused_imports))]
pub use exec::{start, start_undo, Direction, Failure, Journal, Msg, Report, Run, Undoable};

use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// Hard cap on the number of filesystem entries one operation may span.
pub const MAX_TREE_ITEMS: usize = 50_000;

/// Hard cap on how deep a source directory may be walked, counted in levels
/// below the selected root.
pub const MAX_TREE_DEPTH: usize = 64;

/// What [`collect`] found: the enumerated sources, and the selected paths that
/// were no longer there.
#[derive(Debug)]
pub struct Collected {
    pub sources: Vec<Source>,
    /// Marked paths that have since vanished. Reported to the user rather than
    /// silently dropped (ADR 0017 D3).
    pub missing: Vec<PathBuf>,
}

/// Enumerate `paths` into source trees, pruning the ones that no longer exist.
///
/// This is the module's only pre-operation filesystem access, kept thin on
/// purpose: it establishes facts, and [`plan`] decides what they mean.
///
/// Note the walker is plain [`std::fs`] and **not** the `ignore` crate. That
/// crate is ripgrep's gitignore-aware walker and is exactly right for search
/// (ADR 0007), where the user is looking for source files. A copy is the
/// opposite intent: a directory must arrive at its destination whole, including
/// every `target/`, `node_modules/` and `.env` its `.gitignore` hides. A walker
/// that quietly skipped those would produce a copy that is missing files the
/// user can see in the browser.
pub fn collect(paths: &[PathBuf]) -> Result<Collected, Refusal> {
    collect_bounded(paths, MAX_TREE_ITEMS, MAX_TREE_DEPTH)
}

/// The body of [`collect`] with its limits as parameters, so the caps can be
/// exercised against a three-file directory instead of building fifty thousand
/// of them in a test.
fn collect_bounded(
    paths: &[PathBuf],
    max_items: usize,
    max_depth: usize,
) -> Result<Collected, Refusal> {
    let mut walk = Walk {
        budget: max_items,
        max_items,
        max_depth,
    };
    let mut sources = Vec::with_capacity(paths.len());
    let mut missing = Vec::new();

    for path in paths {
        // `symlink_metadata`, never `metadata`: a symlink must be seen as a
        // link, not as whatever it points at (ADR 0017 D5). Following one here
        // would be enough to let a copy escape the source tree.
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(path.clone());
                continue;
            }
            Err(e) => return Err(io_refusal(path, &e)),
        };
        let kind = kind_of(&meta);
        // The root of a source counts against the budget like any other entry.
        walk.charge()?;
        let (nodes, bytes) = if kind == NodeKind::Dir {
            walk.tree(path)?
        } else {
            (Vec::new(), size_of(&meta, kind))
        };
        sources.push(Source {
            path: path.clone(),
            kind,
            items: 1 + nodes.len(),
            nodes,
            bytes,
        });
    }

    Ok(Collected { sources, missing })
}

/// The budget and limits carried through one [`collect`] call. The item budget
/// spans the whole call, not each source, because the cap is about the size of
/// the operation the user is about to authorise.
struct Walk {
    budget: usize,
    max_items: usize,
    max_depth: usize,
}

impl Walk {
    /// Spend one entry of the budget, or refuse. Checked before each entry is
    /// recorded, so the refusal arrives while nothing has been mutated.
    fn charge(&mut self) -> Result<(), Refusal> {
        if self.budget == 0 {
            return Err(Refusal::TooLarge {
                limit: self.max_items,
            });
        }
        self.budget -= 1;
        Ok(())
    }

    /// Enumerate everything below `root`, returning the nodes and their total
    /// payload bytes.
    fn tree(&mut self, root: &Path) -> Result<(Vec<Node>, u64), Refusal> {
        let mut nodes = Vec::new();
        let mut bytes = 0;
        self.descend(root, Path::new(""), 1, &mut nodes, &mut bytes)?;
        Ok((nodes, bytes))
    }

    /// Depth-first pre-order, children sorted by file name.
    ///
    /// The order is load-bearing rather than cosmetic: the executor replays
    /// `nodes` straight down the list, so a directory has to appear before
    /// anything inside it or there would be nowhere to put the contents. Sorting
    /// by name makes two runs over the same tree agree, which is what lets a
    /// plan shown in the overlay be the plan that runs.
    ///
    /// Recursion is safe here because `max_depth` bounds it long before the
    /// thread stack is at risk.
    fn descend(
        &mut self,
        dir: &Path,
        rel: &Path,
        depth: usize,
        nodes: &mut Vec<Node>,
        bytes: &mut u64,
    ) -> Result<(), Refusal> {
        if depth > self.max_depth {
            return Err(Refusal::TooDeep {
                path: dir.to_path_buf(),
                limit: self.max_depth,
            });
        }
        // A directory we cannot read is a real error, not something to skip: a
        // copy that quietly omitted an unreadable subtree would be a lie about
        // what it did.
        let entries = fs::read_dir(dir).map_err(|e| io_refusal(dir, &e))?;
        let mut children: Vec<(OsString, PathBuf)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| io_refusal(dir, &e))?;
            children.push((entry.file_name(), entry.path()));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));

        for (name, path) in children {
            let meta = fs::symlink_metadata(&path).map_err(|e| io_refusal(&path, &e))?;
            let kind = kind_of(&meta);
            self.charge()?;
            let child_rel = rel.join(&name);
            let size = size_of(&meta, kind);
            *bytes += size;
            // The node records what a thing is and where it sits, not how big it
            // is: the executor learns each file's real size from the copy itself,
            // and the plan's total is already summed into `Source::bytes` here.
            // Carrying a per-node size as well would be a second number for the
            // same fact, free to drift and read by nobody.
            nodes.push(Node {
                rel: child_rel.clone(),
                kind,
            });
            // Only a real directory is descended into. A symlink to a directory
            // is recorded as a link and left alone, which closes symlink loops
            // and the "the copy escaped the source tree" surprise in one rule
            // (ADR 0017 D5).
            if kind == NodeKind::Dir {
                self.descend(&path, &child_rel, depth + 1, nodes, bytes)?;
            }
        }
        Ok(())
    }
}

/// Classify from `symlink_metadata`, so the link check comes first: a symlink to
/// a directory is a symlink, not a directory.
fn kind_of(meta: &Metadata) -> NodeKind {
    let ft = meta.file_type();
    if ft.is_symlink() {
        NodeKind::Symlink
    } else if ft.is_dir() {
        NodeKind::Dir
    } else {
        NodeKind::File
    }
}

/// Payload bytes for the progress denominator. A directory contributes 0: its
/// on-disk length is bookkeeping, and counting it would make the total disagree
/// with the size the browser shows for the same selection.
fn size_of(meta: &Metadata, kind: NodeKind) -> u64 {
    match kind {
        NodeKind::Dir => 0,
        _ => meta.len(),
    }
}

fn io_refusal(path: &Path, e: &io::Error) -> Refusal {
    Refusal::Io {
        path: path.to_path_buf(),
        msg: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    /// Create `rel` under `root` with `bytes` bytes of content, parents included.
    fn touch(root: &Path, rel: &str, bytes: usize) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        let mut f = File::create(&path).expect("create file");
        f.write_all(&vec![b'x'; bytes]).expect("write file");
    }

    fn mkdir(root: &Path, rel: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(&path).expect("create dir");
        path
    }

    /// The relative paths of a source's nodes, in walk order.
    fn rels(source: &Source) -> Vec<String> {
        source
            .nodes
            .iter()
            .map(|n| n.rel.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_missing_path_is_reported_rather_than_failing_the_call() {
        let tmp = TempDir::new().expect("tempdir");
        touch(tmp.path(), "here.txt", 3);
        let gone = tmp.path().join("gone.txt");
        let found = collect(&[tmp.path().join("here.txt"), gone.clone()]).expect("collect");
        assert_eq!(found.sources.len(), 1);
        assert_eq!(found.sources[0].path, tmp.path().join("here.txt"));
        // ADR 0017 D3: a stale mark is surfaced, never silently dropped.
        assert_eq!(found.missing, vec![gone]);
    }

    #[test]
    fn a_file_source_is_one_item_with_no_nodes() {
        let tmp = TempDir::new().expect("tempdir");
        touch(tmp.path(), "a.bin", 128);
        let found = collect(&[tmp.path().join("a.bin")]).expect("collect");
        let source = &found.sources[0];
        assert_eq!(source.kind, NodeKind::File);
        assert!(source.nodes.is_empty());
        assert_eq!(source.items, 1);
        assert_eq!(source.bytes, 128);
    }

    #[test]
    fn nested_directories_list_parents_before_children_deterministically() {
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        touch(&root, "c.txt", 1);
        touch(&root, "a.txt", 2);
        touch(&root, "b/b1.txt", 4);
        touch(&root, "b/b2/deep.txt", 8);
        let expected = vec!["a.txt", "b", "b/b1.txt", "b/b2", "b/b2/deep.txt", "c.txt"];

        let first = collect(std::slice::from_ref(&root)).expect("collect");
        assert_eq!(rels(&first.sources[0]), expected);
        // Two runs must agree, or the plan shown in the overlay would not be the
        // plan that runs.
        let second = collect(std::slice::from_ref(&root)).expect("collect");
        assert_eq!(rels(&second.sources[0]), expected);

        let source = &first.sources[0];
        assert_eq!(source.kind, NodeKind::Dir);
        // The root itself plus its six descendants.
        assert_eq!(source.items, 7);
        assert_eq!(source.bytes, 15);
        // Directories carry no payload of their own, which is why the 15 bytes
        // above are exactly the four files and nothing else.
        let b = source
            .nodes
            .iter()
            .find(|n| n.rel == Path::new("b"))
            .expect("b is enumerated");
        assert_eq!(b.kind, NodeKind::Dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_is_recorded_as_a_link_and_never_followed() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        let target = mkdir(tmp.path(), "target");
        touch(&target, "hidden.txt", 5);
        symlink(&target, root.join("link")).expect("symlink");

        let found = collect(std::slice::from_ref(&root)).expect("collect");
        let source = &found.sources[0];
        assert_eq!(rels(source), vec!["link"]);
        assert_eq!(source.nodes[0].kind, NodeKind::Symlink);
        // The walk stopped at the link: nothing from the target came along.
        assert!(!rels(source).iter().any(|r| r.contains("hidden")));
        assert_eq!(source.items, 2);

        // The same rule at the top level: a selected symlink is a leaf.
        let direct = collect(&[root.join("link")]).expect("collect");
        assert_eq!(direct.sources[0].kind, NodeKind::Symlink);
        assert!(direct.sources[0].nodes.is_empty());
        assert_eq!(direct.sources[0].items, 1);
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_loop_terminates_instead_of_recursing() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        // A link pointing at its own parent is the classic walker trap.
        symlink(&root, root.join("loop")).expect("symlink");
        let found = collect(&[root]).expect("collect terminates");
        assert_eq!(rels(&found.sources[0]), vec!["loop"]);
    }

    #[test]
    fn the_item_cap_refuses_the_whole_operation() {
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        touch(&root, "a", 1);
        touch(&root, "b", 1);
        touch(&root, "c", 1);
        // Root plus three children is four entries, one past the cap.
        let err = collect_bounded(std::slice::from_ref(&root), 3, MAX_TREE_DEPTH)
            .expect_err("an explicit refusal, never a silent partial");
        assert_eq!(err, Refusal::TooLarge { limit: 3 });
        // One more entry of headroom and the same tree is fine.
        assert!(collect_bounded(&[root], 4, MAX_TREE_DEPTH).is_ok());
    }

    #[test]
    fn the_item_cap_spans_the_whole_call_not_each_source() {
        let tmp = TempDir::new().expect("tempdir");
        touch(tmp.path(), "a", 1);
        touch(tmp.path(), "b", 1);
        let paths = vec![tmp.path().join("a"), tmp.path().join("b")];
        assert_eq!(
            collect_bounded(&paths, 1, MAX_TREE_DEPTH).expect_err("two files, one slot"),
            Refusal::TooLarge { limit: 1 }
        );
        assert!(collect_bounded(&paths, 2, MAX_TREE_DEPTH).is_ok());
    }

    #[test]
    fn the_depth_cap_refuses_the_whole_operation() {
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        touch(&root, "a/b/deep.txt", 1);
        let err = collect_bounded(std::slice::from_ref(&root), MAX_TREE_ITEMS, 2)
            .expect_err("deeper than we agreed to walk");
        assert!(
            matches!(err, Refusal::TooDeep { limit: 2, .. }),
            "expected TooDeep, got {err:?}"
        );
        // Three levels of headroom covers root/a, root/a/b, root/a/b/deep.txt.
        assert!(collect_bounded(&[root], MAX_TREE_ITEMS, 3).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn an_unreadable_directory_is_surfaced_not_swallowed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().expect("tempdir");
        let root = mkdir(tmp.path(), "root");
        let locked = mkdir(&root, "locked");
        touch(&locked, "inside.txt", 1);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");

        let result = collect(std::slice::from_ref(&root));
        // Root ignores permissions, so on a root test runner there is nothing to
        // assert; restore the mode and leave rather than fake a pass.
        let readable = fs::read_dir(&locked).is_ok();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
        if readable {
            return;
        }
        let err = result.expect_err("an unreadable directory is a real error");
        assert!(
            matches!(err, Refusal::Io { .. }),
            "expected Io, got {err:?}"
        );
    }
}
