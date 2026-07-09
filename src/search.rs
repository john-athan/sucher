//! Recursive, streaming, content-aware file search (ADR 0007).
//!
//! Where the local filter (`/`) narrows the *current directory's* listing in
//! memory with zero IO, recursive search answers "where is this, anywhere below
//! here?" — it walks the tree from a root downward and streams matching hits to
//! the UI as they are found (ADR 0007 D1). The walk itself is ripgrep's own
//! [`ignore`] parallel walker; content matching is ripgrep's own
//! [`grep_searcher`] line searcher driven by a [`grep_regex`] matcher (D3/D4).
//! Running the real engine — not a hand-rolled `walkdir` loop — is the concrete
//! form of the ADR's "more performant than all others" claim.
//!
//! ## Shape of the module
//! [`start`] spawns exactly one owning thread and returns a [`Search`] handle
//! immediately, so the UI never blocks (D3). That thread drives a *parallel*
//! walk (many internal worker threads) and, per accepted entry, sends a
//! [`Msg::Hit`] over an `mpsc` channel; when the walk ends it sends one
//! [`Msg::Done`]. The UI drains the channel each loop iteration ([`Search::drain`])
//! and appends hits live.
//!
//! ## The per-entry pipeline (the interesting part)
//! For every entry the walker visits we:
//!   1. skip the root itself,
//!   2. derive `name`/`is_dir`/`size`/`modified`/`ext` exactly as
//!      `dir::read_entries` does — classification is by extension only, no
//!      per-file read (mirrors ADR 0001's cheap listing path),
//!   3. apply [`Query::matches`] — the **pure, metadata-only** predicate the
//!      local filter also uses (ADR 0007 D2) — as a cheap reject,
//!   4. only if a `content:` term is present *and* the cheap predicates already
//!      passed do we open the file and grep it (D2: a metadata-only query never
//!      touches file bytes; a content query never opens a directory).
//!
//! ## Discipline boundaries
//! Cancellation and the result cap are shared atomics the visitor polls, so a
//! superseded query or a pathological tree stops the walk promptly rather than
//! running to completion off-screen (D3). All IO lives at the edge here; the two
//! genuinely pure helpers ([`cap_snippet`] and the pipeline's reliance on
//! `Query::matches`) are unit-tested without a walk.
//!
//! This module is the search *engine*: it exposes [`start`]/[`Search`]/[`Hit`]/
//! [`Msg`] for the browser's `Mode::Search` arm (ADR 0007 D3/D5) to drive. Until
//! that integration lands, nothing in the binary calls it, so we allow dead_code
//! module-wide — the API is exercised only by this module's own tests for now.
#![allow(dead_code)]

use crate::format::Format;
use crate::query::Query;
use grep_regex::RegexMatcher;
use grep_searcher::sinks::Lossy;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder};
use ignore::{WalkBuilder, WalkState};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;

/// Maximum hits reported before the walk stops itself (ADR 0007 D3). Mirrors the
/// xlsx row cap: it bounds pathological trees (a home directory, `/`) so search
/// can never stream unboundedly into the UI. Hitting it is surfaced via
/// [`Msg::Done`]'s `capped` flag, never silently swallowed.
const CAP: usize = 5000;

/// Longest snippet line kept for a content hit, in characters. A minified JS
/// bundle or a data blob can be one multi-megabyte "line"; capping here keeps a
/// single pathological match from blowing up the result row.
const SNIPPET_CAP: usize = 200;

/// One streamed search result.
pub struct Hit {
    /// Absolute path to the match.
    pub path: PathBuf,
    /// Path relative to the search root, for display (ADR 0007 D5 draws rows
    /// specialised around this — a flat listing has no relative path).
    pub rel: String,
    pub kind: Format,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// For a `content:` match: `(1-based line number, the matched line trimmed &
    /// length-capped)`. `None` for a pure name/metadata match.
    pub snippet: Option<(u64, String)>,
}

/// A message from the background walk to the UI thread.
pub enum Msg {
    Hit(Hit),
    /// The walk ended. `capped` is true when [`CAP`] was reached and some matches
    /// went unreported (surface it in the UI, never truncate silently — D3).
    Done { capped: bool },
}

/// A running search. Owns the receiver plus the cancel handle; dropping it (or
/// calling [`Search::cancel`]) signals the background walk to stop promptly
/// (ADR 0007 D3 — no zombie walkers when the query is superseded).
pub struct Search {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl Search {
    /// Drain every message available right now, non-blocking. The UI calls this
    /// once per loop iteration to append new hits and notice completion.
    pub fn drain(&self) -> Vec<Msg> {
        self.rx.try_iter().collect()
    }

    /// Signal the walk to stop. Idempotent; also invoked on drop.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Search {
    /// A dropped `Search` must not leave its walk running — the UI drops the old
    /// handle when the query changes, and that alone has to stop the old walk.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Start a recursive search from `root` for `query`, spawning a background thread
/// and returning immediately. Honours `show_hidden` (`false` = skip dotfiles, the
/// browser default) and `.gitignore` (always on). Caller guarantees
/// `!query.is_empty()` — an empty query would walk the whole tree for nothing
/// (see [`Query::is_empty`]).
pub fn start(root: PathBuf, query: Query, show_hidden: bool) -> Search {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));

    // Handles the outer thread shares with the parallel walk.
    let cancel_walk = Arc::clone(&cancel);
    std::thread::spawn(move || {
        run_walk(root, query, show_hidden, tx, cancel_walk);
    });

    Search { rx, cancel }
}

/// The body of the owning background thread: build the walker + (optional)
/// content matcher, drive the parallel walk to completion (or cancellation /
/// cap), then send exactly one [`Msg::Done`]. Split out from [`start`] so the
/// spawn site stays a one-liner and this reads top-to-bottom.
fn run_walk(
    root: PathBuf,
    query: Query,
    show_hidden: bool,
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
) {
    // Compile the content matcher once for the whole walk (never per file, ADR
    // 0007 D4). `fixed_strings(true)` makes the pattern a literal substring — no
    // regex metacharacter escaping needed; `case_smart(true)` is smart-case:
    // case-insensitive unless the pattern itself contains an uppercase letter.
    // With fixed strings a build failure is effectively impossible, but if it
    // ever happens we must not fall through to reporting metadata-only hits for
    // what the user asked to be a content search — so we report an empty,
    // completed search instead.
    let matcher = match query.content() {
        Some(pat) => match grep_regex::RegexMatcherBuilder::new()
            .case_smart(true)
            .fixed_strings(true)
            .build(pat)
        {
            Ok(m) => Some(Arc::new(m)),
            Err(_) => {
                let _ = tx.send(Msg::Done { capped: false });
                return;
            }
        },
        None => None,
    };

    // Shared walk state. `count` bounds total hits; `capped` records that the
    // cap was hit so the final `Done` can surface it.
    let count = Arc::new(AtomicUsize::new(0));
    let capped = Arc::new(AtomicBool::new(false));

    let walker = WalkBuilder::new(&root)
        .hidden(!show_hidden) // hidden(true) = SKIP dotfiles; browser default skips them
        .git_ignore(true)
        .build_parallel();

    // `run`'s `mkf` closure is called once per internal worker thread: that is
    // the right place to make the per-thread `Searcher` (a `Searcher` is not
    // shareable across threads) and to clone the `Sender` (an `mpsc::Sender` is
    // `Send` but not `Sync`, so each worker needs its own clone). Everything
    // read-only (`root`, `query`, the compiled `matcher`, the atomics) is
    // borrowed/cloned in — `run` blocks until the walk finishes, so borrows of
    // this stack frame outlive the walk.
    // Borrow the read-only state as references the per-thread closures copy in
    // (references are `Copy`, so each worker gets its own copy of the borrow;
    // the borrows outlive the walk because `run` blocks until it completes).
    let root = &root;
    let query = &query;
    let matcher = matcher.as_deref();
    walker.run(|| {
        let tx = tx.clone();
        let cancel = Arc::clone(&cancel);
        let count = Arc::clone(&count);
        let capped = Arc::clone(&capped);
        // Line numbers on (for snippets); binary files auto-skipped so a content
        // search never streams NUL-laden garbage.
        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .binary_detection(BinaryDetection::quit(0))
            .build();

        Box::new(move |result| {
            visit(
                result,
                root,
                query,
                matcher,
                &mut searcher,
                &tx,
                &cancel,
                &count,
                &capped,
            )
        })
    });

    // Exactly one terminal message, whether the walk finished naturally, was
    // cancelled, or hit the cap. (If the receiver is already gone this is a
    // no-op — the UI moved on.)
    let _ = tx.send(Msg::Done {
        capped: capped.load(Ordering::Relaxed),
    });
}

/// Per-entry visitor: the pipeline from ADR 0007 D2/D3. Returns the
/// [`WalkState`] telling the walker whether to keep going, stop this branch, or
/// quit the whole walk. All the `&Arc<...>` are the shared walk state; taking
/// them by reference keeps the hot path allocation-free.
#[allow(clippy::too_many_arguments)]
fn visit(
    result: Result<ignore::DirEntry, ignore::Error>,
    root: &Path,
    query: &Query,
    matcher: Option<&RegexMatcher>,
    searcher: &mut Searcher,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
    count: &AtomicUsize,
    capped: &AtomicBool,
) -> WalkState {
    // Cancelled (superseded query, or the handle was dropped) → stop promptly.
    if cancel.load(Ordering::Relaxed) {
        return WalkState::Quit;
    }

    // A per-entry error (permission denied on a subtree, a broken symlink) is
    // not fatal: skip this entry and keep walking the rest of the tree.
    let entry = match result {
        Ok(e) => e,
        Err(_) => return WalkState::Continue,
    };

    // Skip the root entry itself — it is visited at depth 0 and is not a "result".
    if entry.depth() == 0 {
        return WalkState::Continue;
    }

    let path = entry.path();
    let name = entry.file_name().to_string_lossy().into_owned();
    // `file_type()` is None only for stdin, which the tree walk never yields;
    // treat the absent case as "not a directory".
    let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
    let meta = entry.metadata().ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta.and_then(|m| m.modified().ok());
    // Classify by extension only (pass no head) — identical to the browser's
    // listing path (`dir::read_entries`); no per-entry file read for kind.
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let kind = crate::format::classify(&ext, is_dir, None);

    // Cheap, pure metadata reject (name fuzzy + kind/ext/size/age). This is the
    // same predicate the local filter runs; it gates every entry before any file
    // is ever opened (ADR 0007 D2).
    if !query.matches(&name, kind, size, modified) {
        return WalkState::Continue;
    }

    // Content matching: only when a `content:` term is present, and only for
    // files — a directory has no bytes to grep, so it can never be a content hit.
    let snippet = match matcher {
        Some(m) => {
            if is_dir {
                return WalkState::Continue;
            }
            match grep_first_match(searcher, m, path) {
                Some(hit) => Some(hit),
                // File matched metadata but contains no occurrence of the term
                // (or errored / is binary) → not a hit.
                None => return WalkState::Continue,
            }
        }
        None => None,
    };

    // Reserve a slot under the cap. `fetch_add` returns the *previous* count, so
    // the first CAP entries (previous 0..CAP) are reported and everything after
    // is refused — bounding total sends to at most CAP even across worker
    // threads racing on the counter.
    if count.fetch_add(1, Ordering::Relaxed) >= CAP {
        capped.store(true, Ordering::Relaxed);
        return WalkState::Quit;
    }

    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();

    let hit = Hit {
        path: path.to_path_buf(),
        rel,
        kind,
        size,
        modified,
        snippet,
    };

    // A send error means the UI dropped the receiver (it moved on). Treat it like
    // cancellation: flag it so sibling workers stop too, and quit.
    if tx.send(Msg::Hit(hit)).is_err() {
        cancel.store(true, Ordering::Relaxed);
        return WalkState::Quit;
    }

    WalkState::Continue
}

/// Grep `path` for the first line matching `matcher`, returning
/// `(1-based line number, capped snippet)`. `None` when there is no match, the
/// file is binary, or it errors on open/read — a search must never crash on one
/// bad file (ADR 0007 D4).
///
/// Uses a [`Lossy`] sink (invalid UTF-8 in the matched line degrades to `�`
/// rather than erroring) whose closure captures the first hit and returns
/// `Ok(false)` to stop the searcher immediately — we only ever want the first
/// line for the snippet, not every match in the file.
fn grep_first_match(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    path: &Path,
) -> Option<(u64, String)> {
    let mut found: Option<(u64, String)> = None;
    let sink = Lossy(|lnum: u64, line: &str| {
        found = Some((lnum, cap_snippet(line)));
        Ok(false) // stop after the first matching line
    });
    // Ignore the search error deliberately: an unreadable file is simply not a
    // content hit, never a crash.
    let _ = searcher.search_path(matcher, path, sink);
    found
}

/// Trim a matched line and cap its length for display. Pure — unit-tested
/// without a search. Strips surrounding whitespace (grep hands us the line with
/// its trailing newline), then truncates to [`SNIPPET_CAP`] *characters* (not
/// bytes, so multi-byte text is never split mid-char), appending `…` when cut.
fn cap_snippet(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() > SNIPPET_CAP {
        let mut s: String = trimmed.chars().take(SNIPPET_CAP).collect();
        s.push('…');
        s
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// Per-process, per-test-unique temp directory that cleans itself up on drop
    /// — no reliance on an external tempfile crate, and parallel tests never
    /// collide (`process id` + a monotonic counter).
    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Fixture {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "sucher-search-test-{}-{}",
                std::process::id(),
                n
            ));
            // Start clean if a previous crashed run left it behind.
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Fixture { root }
        }

        /// Create a file at `rel` (nested dirs auto-created) with `body`.
        fn file(&self, rel: &str, body: &str) {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, body).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// The standard fixture tree used by most tests (mirrors the ADR test plan):
    /// ```text
    /// root/
    ///   a.rs           "fn main() { let TODO = 1; }"
    ///   notes.md       "a NEEDLE here"
    ///   sub/b.rs       "other"
    ///   sub/deep/c.txt "needle lower"
    ///   .hidden.txt    "secret needle"
    /// ```
    fn standard_tree() -> Fixture {
        let fx = Fixture::new();
        fx.file("a.rs", "fn main() { let TODO = 1; }");
        fx.file("notes.md", "a NEEDLE here");
        fx.file("sub/b.rs", "other");
        fx.file("sub/deep/c.txt", "needle lower");
        fx.file(".hidden.txt", "secret needle");
        fx
    }

    /// Collect every hit of a search synchronously: drive `start`, drain until
    /// `Done`, and return `(hits, capped)`. Bounded by a wall-clock timeout and a
    /// max iteration count so a bug can never hang the test suite — it panics
    /// with a clear message if `Done` never arrives.
    fn collect(fx: &Fixture, raw: &str, show_hidden: bool) -> (Vec<Hit>, bool) {
        let search = start(fx.root.clone(), query::parse(raw), show_hidden);
        let mut hits = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        for _ in 0..100_000 {
            for msg in search.drain() {
                match msg {
                    Msg::Hit(h) => hits.push(h),
                    Msg::Done { capped } => return (hits, capped),
                }
            }
            assert!(
                Instant::now() < deadline,
                "search did not finish within timeout for query {raw:?}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("search did not finish within iteration bound for query {raw:?}");
    }

    /// Just the sorted `rel` strings, for order-independent set assertions.
    fn rels(hits: &[Hit]) -> Vec<String> {
        let mut v: Vec<String> = hits.iter().map(|h| h.rel.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn descends_recursively_for_a_name_match() {
        // A name query for `b` must find `sub/b.rs` — proving the walk descends
        // below the root, which the local filter cannot do.
        let fx = standard_tree();
        let (hits, _) = collect(&fx, "b", false);
        let rels = rels(&hits);
        assert!(
            rels.contains(&"sub/b.rs".to_string()),
            "expected sub/b.rs, got {rels:?}"
        );
    }

    #[test]
    fn ext_predicate_applies_across_depths() {
        // `ext:rs` finds both .rs files at different depths, and nothing else.
        let fx = standard_tree();
        let (hits, _) = collect(&fx, "ext:rs", false);
        assert_eq!(rels(&hits), vec!["a.rs".to_string(), "sub/b.rs".to_string()]);
    }

    #[test]
    fn content_match_is_smart_case_and_recursive() {
        // Lowercase pattern → case-insensitive: matches notes.md ("NEEDLE") and
        // the deep c.txt ("needle"). .hidden.txt also contains "needle" but is
        // hidden, so it must not appear.
        let fx = standard_tree();
        let (hits, _) = collect(&fx, "content:needle", false);
        assert_eq!(
            rels(&hits),
            vec!["notes.md".to_string(), "sub/deep/c.txt".to_string()]
        );

        // The notes.md hit carries the right line number + snippet text.
        let notes = hits.iter().find(|h| h.rel == "notes.md").unwrap();
        assert_eq!(notes.snippet, Some((1, "a NEEDLE here".to_string())));
    }

    #[test]
    fn content_match_smart_case_goes_sensitive_on_uppercase() {
        // Uppercase pattern → case-sensitive: only notes.md ("NEEDLE") matches,
        // not c.txt ("needle lower").
        let fx = standard_tree();
        let (hits, _) = collect(&fx, "content:NEEDLE", false);
        assert_eq!(rels(&hits), vec!["notes.md".to_string()]);
    }

    #[test]
    fn hidden_files_respect_the_toggle() {
        let fx = standard_tree();
        // Default (show_hidden = false): the dotfile is invisible even though it
        // contains the term.
        let (hidden_off, _) = collect(&fx, "content:secret", false);
        assert!(
            hidden_off.is_empty(),
            "hidden file should be skipped, got {:?}",
            rels(&hidden_off)
        );
        // show_hidden = true: now the walk descends into dotfiles and finds it.
        let (hidden_on, _) = collect(&fx, "content:secret", true);
        assert_eq!(rels(&hidden_on), vec![".hidden.txt".to_string()]);
    }

    #[test]
    fn content_queries_never_return_a_directory() {
        // `sub` is a directory whose name would fuzzy-match, but a content query
        // must skip directories entirely (they have no bytes). Give the dir a
        // name that also appears as file content to be sure the *directory* is
        // excluded, not merely absent.
        let fx = Fixture::new();
        fx.file("needle_dir/inside.txt", "needle");
        fx.file("needle_top.txt", "needle");
        let (hits, _) = collect(&fx, "content:needle", false);
        // Only the two files, never the `needle_dir` directory entry.
        for h in &hits {
            assert!(
                h.kind != Format::Directory,
                "content query returned a directory: {}",
                h.rel
            );
        }
        assert_eq!(
            rels(&hits),
            vec![
                "needle_dir/inside.txt".to_string(),
                "needle_top.txt".to_string()
            ]
        );
    }

    #[test]
    fn cap_snippet_trims_and_truncates() {
        // Trims surrounding whitespace (grep hands us the trailing newline).
        assert_eq!(cap_snippet("  hello world \n"), "hello world");
        // Short lines pass through untouched, no ellipsis.
        assert_eq!(cap_snippet("short"), "short");
        // Over the cap → truncated to SNIPPET_CAP chars + ellipsis.
        let long = "x".repeat(SNIPPET_CAP + 50);
        let capped = cap_snippet(&long);
        assert_eq!(capped.chars().count(), SNIPPET_CAP + 1); // + the '…'
        assert!(capped.ends_with('…'));
    }
}
