//! The browser's multi-select set: which paths the next file operation acts on.
//!
//! Per ADR 0017 D3 the set is **global**. It is keyed by absolute path and is
//! not cleared on a directory change, so a user can gather three files here, two
//! in a sibling folder, and act on all five at once. That workflow is the entire
//! reason multi-select beats operating on the cursor; a set that reset on every
//! `h`/`l` would only be a slower cursor.
//!
//! The module is deliberately pure: no filesystem, no ratatui, no crossterm, no
//! clock. It is a set with an order and a running byte total, which is what makes
//! the whole of it unit-testable without a temp directory. Everything that
//! actually touches the disk lives in `fileop`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// One marked entry: the absolute path plus the metadata the listing showed at
/// the moment it was marked.
///
/// `size` and `is_dir` are a **snapshot**, not the truth. The set outlives
/// directory changes and can be held for minutes while other processes write to
/// the disk, so by the time an operation runs the file may have grown, shrunk,
/// changed type, or vanished. These fields exist only to render the status line
/// (`N marked · <size>`) and to size the confirm overlay without stat-ing every
/// mark on every frame. The operation engine in `src/fileop` re-checks each path
/// against the real filesystem before acting, and prunes what disappeared at plan
/// time rather than dropping it silently (ADR 0017 D3).
pub struct Mark {
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

/// The mark set: an ordered set of absolute paths with a running byte total.
///
/// Two containers hold the same membership on purpose, because the two access
/// patterns pull in opposite directions:
///
/// - `order` is the record of *when* each path was marked, and it is what
///   [`Marks::marks`] hands to the planner. Operations apply in this order, so
///   determinism here is a correctness property rather than a nicety: the same
///   marks in the same sequence must always produce the same plan, including the
///   ` (2)`-style collision suffixes the planner assigns as it walks the batch.
/// - `index` answers "is this row marked?" in constant time. The entry list asks
///   that once per visible row per frame while drawing the mark gutter, so a
///   linear scan of `order` would make rendering cost grow with the size of the
///   selection. Removal is the mirror image: it pays an O(n) scan of `order`, but
///   it happens once per keystroke, not once per row per frame.
///
/// The two are only ever mutated together, by the methods below, which is what
/// keeps them from disagreeing.
#[derive(Default)]
pub struct Marks {
    /// Marks in mark order. Stable: removing one entry never reorders the rest.
    order: Vec<Mark>,
    /// Membership index over the same paths, for O(1) [`Marks::contains`].
    index: HashSet<PathBuf>,
    /// Running total of `Mark::size`, maintained incrementally so the status line
    /// costs nothing to draw. Kept exact by only ever adding on a genuine insert
    /// and subtracting on a genuine removal.
    bytes: u64,
}

impl Marks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle `path`. Returns whether it is marked **after** the call, which is
    /// exactly what the caller needs to redraw the gutter for that row.
    pub fn toggle(&mut self, path: &Path, size: u64, is_dir: bool) -> bool {
        if self.remove(path) {
            false
        } else {
            self.insert(path, size, is_dir);
            true
        }
    }

    /// Mark `path` if it is not already marked. Returns whether it was newly
    /// inserted, so a re-mark is a no-op: it neither duplicates the entry nor
    /// adds its bytes a second time. A path already in the set keeps the metadata
    /// captured the first time; both snapshots are equally provisional, and
    /// leaving the entry untouched keeps its position in the order.
    pub fn insert(&mut self, path: &Path, size: u64, is_dir: bool) -> bool {
        if !self.index.insert(path.to_path_buf()) {
            return false;
        }
        self.order.push(Mark {
            path: path.to_path_buf(),
            size,
            is_dir,
        });
        self.bytes = self.bytes.saturating_add(size);
        true
    }

    /// Unmark `path`. Returns whether it had been marked. The survivors keep
    /// their relative order, so removing one mark never changes the sequence a
    /// later operation will apply to the others.
    pub fn remove(&mut self, path: &Path) -> bool {
        if !self.index.remove(path) {
            return false;
        }
        if let Some(i) = self.order.iter().position(|m| m.path == path) {
            let gone = self.order.remove(i);
            self.bytes = self.bytes.saturating_sub(gone.size);
        }
        true
    }

    /// Is this exact path marked? Exact, never a prefix: `/a/b` being marked says
    /// nothing about `/a` or `/a/bc`, and marking a directory does not implicitly
    /// mark what is inside it.
    pub fn contains(&self, path: &Path) -> bool {
        self.index.contains(path)
    }

    /// Drop every mark (the `Esc` binding, ADR 0017 D4).
    pub fn clear(&mut self) {
        self.order.clear();
        self.index.clear();
        self.bytes = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Total bytes of the marked entries as captured at mark time.
    ///
    /// This is the byte total of the marked **files**. Listings give directories
    /// a size of 0 in this codebase, so a marked directory contributes nothing
    /// here and the figure says nothing about what its tree contains. A selection
    /// of one large folder therefore reports 0 B, which is honest about what has
    /// been measured; only the planner, which walks the tree, can give a real
    /// total for a recursive copy.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The marks in mark order. The planner consumes them in this sequence, so it
    /// is stable and deterministic by construction (see the type's comment).
    pub fn marks(&self) -> &[Mark] {
        &self.order
    }

    /// Mark every listed entry that is not already marked (the "mark all" key).
    ///
    /// Entries already in the set keep their earlier position, and the newcomers
    /// are appended in listing order, so pressing the key twice changes nothing.
    /// Only the given rows are touched: marks held from other directories are
    /// untouched, because the set is global (ADR 0017 D3).
    pub fn mark_all<'a>(&mut self, visible: impl Iterator<Item = (&'a Path, u64, bool)>) {
        for (path, size, is_dir) in visible {
            self.insert(path, size, is_dir);
        }
    }

    /// Toggle every listed entry (the "invert" key): what was marked becomes
    /// unmarked and the rest become marked, in listing order. Marks held in other
    /// directories are outside the listing and so survive untouched.
    pub fn invert<'a>(&mut self, visible: impl Iterator<Item = (&'a Path, u64, bool)>) {
        for (path, size, is_dir) in visible {
            self.toggle(path, size, is_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mark `path` with a plausible file size, the common case in these tests.
    fn file(m: &mut Marks, path: &str, size: u64) -> bool {
        m.insert(Path::new(path), size, false)
    }

    fn paths(m: &Marks) -> Vec<&str> {
        m.marks().iter().filter_map(|k| k.path.to_str()).collect()
    }

    /// A listing row as the browser hands it over: (path, size, is_dir).
    fn listing<'a>(
        rows: &'a [(&'a str, u64, bool)],
    ) -> impl Iterator<Item = (&'a Path, u64, bool)> {
        rows.iter().map(|(p, s, d)| (Path::new(*p), *s, *d))
    }

    #[test]
    fn empty_set_reports_empty() {
        let m = Marks::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
        assert_eq!(m.bytes(), 0);
        assert!(m.marks().is_empty());
        assert!(!m.contains(Path::new("/a/b.txt")));
    }

    #[test]
    fn toggle_round_trips_and_reports_state_after() {
        let mut m = Marks::new();
        // `toggle` answers "is it marked now?", which is what the gutter draws.
        assert!(m.toggle(Path::new("/a/b.txt"), 10, false));
        assert!(m.contains(Path::new("/a/b.txt")));
        assert_eq!(m.len(), 1);
        assert!(!m.toggle(Path::new("/a/b.txt"), 10, false));
        assert!(!m.contains(Path::new("/a/b.txt")));
        assert_eq!(m.len(), 0);
        assert_eq!(m.bytes(), 0);
    }

    #[test]
    fn reinserting_is_idempotent_in_count_and_bytes() {
        let mut m = Marks::new();
        assert!(file(&mut m, "/a/b.txt", 100));
        // A second insert of the same path is a no-op, and must not double-count.
        assert!(!file(&mut m, "/a/b.txt", 100));
        // Even a stale size from a re-listing must not be added a second time.
        assert!(!file(&mut m, "/a/b.txt", 999));
        assert_eq!(m.len(), 1);
        assert_eq!(m.bytes(), 100);
        assert_eq!(paths(&m), vec!["/a/b.txt"]);
    }

    #[test]
    fn mark_order_is_stable_and_survives_unrelated_removal() {
        let mut m = Marks::new();
        file(&mut m, "/z/first", 1);
        file(&mut m, "/a/second", 2);
        file(&mut m, "/m/third", 3);
        // Order is mark order, not sorted order (ADR 0017: operations apply in it).
        assert_eq!(paths(&m), vec!["/z/first", "/a/second", "/m/third"]);
        // Removing the middle mark leaves the survivors in their original order.
        assert!(m.remove(Path::new("/a/second")));
        assert_eq!(paths(&m), vec!["/z/first", "/m/third"]);
        // Re-marking it appends at the end; it does not return to its old slot.
        file(&mut m, "/a/second", 2);
        assert_eq!(paths(&m), vec!["/z/first", "/m/third", "/a/second"]);
    }

    #[test]
    fn remove_reports_whether_it_had_been_marked() {
        let mut m = Marks::new();
        file(&mut m, "/a/b.txt", 7);
        assert!(m.remove(Path::new("/a/b.txt")));
        assert!(!m.remove(Path::new("/a/b.txt")));
        assert!(!m.remove(Path::new("/never/marked")));
    }

    #[test]
    fn bytes_accounting_through_every_mutator() {
        let mut m = Marks::new();
        file(&mut m, "/a/one", 100);
        file(&mut m, "/a/two", 250);
        assert_eq!(m.bytes(), 350);
        // toggle off subtracts exactly what was added.
        assert!(!m.toggle(Path::new("/a/one"), 100, false));
        assert_eq!(m.bytes(), 250);
        // toggle on adds again.
        assert!(m.toggle(Path::new("/a/one"), 100, false));
        assert_eq!(m.bytes(), 350);
        assert!(m.remove(Path::new("/a/two")));
        assert_eq!(m.bytes(), 100);
        m.clear();
        assert_eq!(m.bytes(), 0);
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn directories_contribute_no_bytes() {
        let mut m = Marks::new();
        // Listings give directories size 0, so `bytes()` counts files only.
        assert!(m.insert(Path::new("/a/dir"), 0, true));
        assert!(m.insert(Path::new("/a/file"), 42, false));
        assert_eq!(m.len(), 2);
        assert_eq!(m.bytes(), 42);
        assert!(m.marks()[0].is_dir);
        assert!(!m.marks()[1].is_dir);
    }

    #[test]
    fn mark_all_skips_marked_and_appends_the_rest_in_listing_order() {
        let mut m = Marks::new();
        file(&mut m, "/d/b", 2);
        let rows = [("/d/a", 1, false), ("/d/b", 2, false), ("/d/c", 0, true)];
        m.mark_all(listing(&rows));
        // `/d/b` keeps its earlier slot; the new ones follow in listing order.
        assert_eq!(paths(&m), vec!["/d/b", "/d/a", "/d/c"]);
        assert_eq!(m.len(), 3);
        assert_eq!(m.bytes(), 3);
    }

    #[test]
    fn mark_all_is_idempotent() {
        let mut m = Marks::new();
        let rows = [("/d/a", 1, false), ("/d/b", 2, false)];
        m.mark_all(listing(&rows));
        m.mark_all(listing(&rows));
        assert_eq!(m.len(), 2);
        assert_eq!(m.bytes(), 3);
    }

    #[test]
    fn invert_unmarks_the_marked_and_marks_the_rest() {
        let mut m = Marks::new();
        file(&mut m, "/d/b", 2);
        let rows = [("/d/a", 1, false), ("/d/b", 2, false), ("/d/c", 4, false)];
        m.invert(listing(&rows));
        assert_eq!(paths(&m), vec!["/d/a", "/d/c"]);
        assert_eq!(m.bytes(), 5);
        // Inverting twice returns to the starting set, though not its order.
        m.invert(listing(&rows));
        assert_eq!(paths(&m), vec!["/d/b"]);
        assert_eq!(m.bytes(), 2);
    }

    #[test]
    fn invert_leaves_marks_outside_the_listing_alone() {
        // The set is global (ADR 0017 D3), so a mark from another folder must
        // survive an invert that never mentions it.
        let mut m = Marks::new();
        file(&mut m, "/elsewhere/keep", 9);
        let rows = [("/d/a", 1, false)];
        m.invert(listing(&rows));
        assert_eq!(paths(&m), vec!["/elsewhere/keep", "/d/a"]);
        assert_eq!(m.bytes(), 10);
    }

    #[test]
    fn contains_is_an_exact_path_match_not_a_prefix() {
        let mut m = Marks::new();
        file(&mut m, "/a/bc.txt", 1);
        assert!(m.contains(Path::new("/a/bc.txt")));
        // A parent, a component prefix, and a longer path are all unmarked.
        assert!(!m.contains(Path::new("/a")));
        assert!(!m.contains(Path::new("/a/b")));
        assert!(!m.contains(Path::new("/a/bc.txt.bak")));
        assert!(!m.contains(Path::new("/a/bc.txt/inner")));
    }

    #[test]
    fn default_matches_new() {
        let m = Marks::default();
        assert!(m.is_empty());
        assert_eq!(m.bytes(), 0);
    }
}
