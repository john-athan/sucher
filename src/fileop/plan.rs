//! The pure half of the file-operation engine (ADR 0017 D5).
//!
//! Everything in this file is a total function of its arguments. It never opens
//! a file, never stats a path, never reads the clock. An intended operation
//! ([`Op`]) plus a snapshot of the world ([`PlanCtx`]) goes in, and either a
//! fully resolved [`Plan`], every source already mapped to its final destination
//! name, or a [`Refusal`] naming exactly what is wrong comes out.
//!
//! That purity is the point of the seam. The interesting part of a file manager
//! is not the `fs::copy` call, it is the matrix of collisions and refusals
//! around it: what happens when two marked files share a name, when the
//! destination lives inside the source, when the user is standing in the
//! directory being moved. Deciding all of that without a filesystem means the
//! whole matrix is unit-tested at the bottom of this file with no temp
//! directory, and the executor is left with nothing to decide.
//!
//! Two rules from ADR 0017 D5 shape the output:
//!
//!   * Nothing is overwritten silently. A colliding name is suffixed into
//!     `foo (2).txt` and the step is flagged [`Step::renamed`], so the confirm
//!     overlay can show what it did before a byte moves. `overwrite: true` is
//!     reachable only through an explicit [`Conflict::Overwrite`] toggle.
//!   * A refusal is a named sentence, never a silent partial. This is ADR 0009's
//!     "an honest error, never a silent truncation" carried over from decoding
//!     into mutation.
//!
//! If a check here ever wants to stat something, it belongs in `collect`
//! instead: `collect` is the module's one impure edge, and it hands its findings
//! over as [`Source`] values.

use crate::util::human_size;
use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

/// How far the collision suffix will count before giving up. A directory holding
/// ten thousand `foo (n).txt` siblings is not a case worth grinding through, and
/// an unbounded search loop in a pure function is a hang waiting to happen, so
/// exhaustion becomes an honest [`Refusal::NameTaken`].
const MAX_SUFFIX: usize = 10_000;

/// What a path is, decided once by `collect` with `symlink_metadata` so a
/// symlink can never be mistaken for the thing it points at (ADR 0017 D5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    File,
    Dir,
    Symlink,
}

/// One entry inside a source tree, relative to that source's root.
#[derive(Clone, Debug)]
pub struct Node {
    pub rel: PathBuf,
    pub kind: NodeKind,
    /// Payload bytes. A directory carries 0: its on-disk length is bookkeeping,
    /// not content, and the browser's size column already reads it that way.
    pub size: u64,
}

/// One top-level selected path, with its subtree fully enumerated.
#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub kind: NodeKind,
    /// The subtree in a deterministic, parents-before-children order. Empty for
    /// a file or symlink.
    pub nodes: Vec<Node>,
    /// The root itself plus every node under it, so a lone file counts as 1.
    pub items: usize,
    pub bytes: u64,
}

/// What to do when a destination name is already spoken for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conflict {
    /// Collisions get a " (2)"-style suffix. The default.
    Rename,
    /// Collisions replace the existing entry. Only ever reached by an explicit
    /// user toggle.
    Overwrite,
}

/// Which operation a [`Plan`] describes. Mirrors [`Op`] without its payload, so
/// the overlay and the status line can name the verb.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Copy,
    Move,
    Rename,
    Create,
    Trash,
}

impl Kind {
    /// The verb for the status line, lowercase because it is read mid-sentence.
    fn verb(self) -> &'static str {
        match self {
            Kind::Copy => "copy",
            Kind::Move => "move",
            Kind::Rename => "rename",
            Kind::Create => "create",
            Kind::Trash => "trash",
        }
    }
}

/// An intended operation, with its sources already enumerated by `collect`.
pub enum Op {
    Copy {
        sources: Vec<Source>,
        dest: PathBuf,
    },
    Move {
        sources: Vec<Source>,
        dest: PathBuf,
    },
    Rename {
        source: Source,
        new_name: String,
    },
    /// A trailing '/' on `name` means "make a directory" (ADR 0017 D4).
    Create {
        parent: PathBuf,
        name: String,
    },
    Trash {
        sources: Vec<Source>,
    },
}

/// One resolved unit of work: this source path becomes this destination path.
#[derive(Clone, Debug)]
pub struct Step {
    /// Empty for [`Kind::Create`], which has no source to speak of.
    pub src: PathBuf,
    /// Empty for [`Kind::Trash`], whose destination is the OS trash and not a
    /// path sucher gets to name (ADR 0017 D7).
    pub dest: PathBuf,
    pub kind: NodeKind,
    pub nodes: Vec<Node>,
    pub items: usize,
    pub bytes: u64,
    /// The destination name was suffixed to dodge a collision.
    pub renamed: bool,
    /// The destination already exists and will be replaced.
    pub overwrite: bool,
}

/// A fully resolved operation, ready to be shown in the confirm overlay and then
/// replayed by the executor without any further decisions.
#[derive(Clone, Debug)]
pub struct Plan {
    pub kind: Kind,
    /// The destination directory. Empty for [`Kind::Trash`].
    pub dest: PathBuf,
    pub steps: Vec<Step>,
    /// Paths that were marked but had vanished by the time `collect` looked.
    /// Carried into the plan so the overlay can report them: a stale mark is
    /// never silently dropped (ADR 0017 D3).
    pub missing: Vec<PathBuf>,
    pub policy: Conflict,
}

impl Plan {
    /// Every filesystem entry this plan will touch, roots included.
    pub fn items(&self) -> usize {
        self.steps.iter().map(|s| s.items).sum()
    }

    /// Payload bytes across the whole plan, for the progress bar's denominator.
    pub fn bytes(&self) -> u64 {
        self.steps.iter().map(|s| s.bytes).sum()
    }

    /// How many steps were suffixed to dodge a collision.
    pub fn renamed(&self) -> usize {
        self.steps.iter().filter(|s| s.renamed).count()
    }

    /// How many steps will replace something that already exists.
    pub fn overwrites(&self) -> usize {
        self.steps.iter().filter(|s| s.overwrite).count()
    }

    /// One-line human summary for the status line, e.g. `copy 3 items, 1.2M`.
    ///
    /// The size is reported through [`human_size`], the same formatter the
    /// browser's size column uses, so the two never disagree about what a
    /// megabyte looks like. Rename and overwrite counts are appended when they
    /// are non-zero, because D5's promise is that the user sees the outcome
    /// before authorising it, and the status line is the smallest place that
    /// promise has to hold.
    pub fn summary(&self) -> String {
        let items = self.items();
        let noun = if items == 1 { "item" } else { "items" };
        let mut out = format!("{} {items} {noun}", self.kind.verb());
        let bytes = self.bytes();
        if bytes > 0 {
            out.push_str(&format!(", {}", human_size(bytes)));
        }
        let renamed = self.renamed();
        if renamed > 0 {
            out.push_str(&format!(", {renamed} renamed"));
        }
        let overwrites = self.overwrites();
        if overwrites > 0 {
            out.push_str(&format!(", {overwrites} overwritten"));
        }
        out
    }
}

/// Everything [`plan`] is allowed to know about the world. Passed in so [`plan`]
/// stays pure.
pub struct PlanCtx<'a> {
    /// The file names already present in the destination directory.
    pub dest_listing: &'a [String],
    /// The browser's current directory, used to refuse operating on an ancestor
    /// of where the user is standing.
    pub cwd: &'a Path,
    /// Marked paths that had already vanished when `collect` looked. Handed in
    /// rather than discovered, so [`Plan::missing`] is complete without [`plan`]
    /// ever touching the filesystem.
    pub missing: &'a [PathBuf],
    pub policy: Conflict,
}

/// Why an operation will not happen. One variant per reason, each rendering a
/// single honest sentence for the status line and the confirm overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No marks and nothing under the cursor.
    NothingSelected,
    /// Moving or copying a directory into something below itself.
    IntoOwnDescendant { src: PathBuf, dest: PathBuf },
    /// The destination is the source itself.
    SameLocation { path: PathBuf },
    /// A move whose source already sits in the destination directory.
    AlreadyThere { path: PathBuf },
    /// A rename that changes nothing.
    SameName { name: String },
    /// Empty, `.`, `..`, or containing a path separator.
    BadName { name: String },
    /// A rename or create onto an existing entry.
    NameTaken { name: String },
    /// The filesystem root, which has no parent to be moved within.
    FilesystemRoot,
    /// The directory the user is standing in, or one above it.
    AncestorOfCwd { path: PathBuf },
    /// More entries than one operation may span.
    TooLarge { limit: usize },
    /// Nested deeper than one operation may walk.
    TooDeep { path: PathBuf, limit: usize },
    /// The filesystem said no while enumerating.
    Io { path: PathBuf, msg: String },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NothingSelected => write!(f, "nothing selected"),
            Refusal::IntoOwnDescendant { src, dest } => write!(
                f,
                "cannot put {} inside itself (destination {})",
                src.display(),
                dest.display()
            ),
            Refusal::SameLocation { path } => {
                write!(f, "{} is already the destination", path.display())
            }
            Refusal::AlreadyThere { path } => write!(
                f,
                "{} is already in the destination directory",
                path.display()
            ),
            Refusal::SameName { name } => write!(f, "\"{name}\" is already its name"),
            Refusal::BadName { name } => write!(
                f,
                "\"{name}\" is not a usable name: it cannot be empty, \".\", \"..\", or contain a path separator"
            ),
            Refusal::NameTaken { name } => write!(f, "\"{name}\" already exists here"),
            Refusal::FilesystemRoot => write!(f, "refusing to operate on the filesystem root"),
            Refusal::AncestorOfCwd { path } => write!(
                f,
                "{} holds the directory you are in, so it cannot be operated on from here",
                path.display()
            ),
            Refusal::TooLarge { limit } => {
                write!(f, "refusing: this spans more than {limit} items")
            }
            Refusal::TooDeep { path, limit } => write!(
                f,
                "refusing: {} is nested deeper than {limit} levels",
                path.display()
            ),
            Refusal::Io { path, msg } => write!(f, "{}: {msg}", path.display()),
        }
    }
}

/// Resolve an intended operation against a snapshot of the world.
///
/// Every source is validated before a single destination name is allocated, so
/// a batch is refused whole rather than half-planned: the confirm overlay should
/// never show a plan that was quietly trimmed of its illegal parts.
pub fn plan(op: Op, ctx: &PlanCtx) -> Result<Plan, Refusal> {
    match op {
        Op::Copy { sources, dest } => transfer(Kind::Copy, sources, dest, ctx),
        Op::Move { sources, dest } => transfer(Kind::Move, sources, dest, ctx),
        Op::Rename { source, new_name } => rename(source, new_name, ctx),
        Op::Create { parent, name } => create(parent, name, ctx),
        Op::Trash { sources } => trash(sources, ctx),
    }
}

/// Copy and move differ in exactly two places: the verb, and whether a source
/// already sitting in the destination is a duplicate (copy) or a no-op (move).
/// Everything else, including the collision naming, is shared.
fn transfer(
    kind: Kind,
    sources: Vec<Source>,
    dest: PathBuf,
    ctx: &PlanCtx,
) -> Result<Plan, Refusal> {
    if sources.is_empty() {
        return Err(Refusal::NothingSelected);
    }
    for source in &sources {
        guard(&source.path, ctx)?;
        // Equality first: a path always starts with itself, so the descendant
        // test below would otherwise swallow this clearer reason.
        if source.path == dest {
            return Err(Refusal::SameLocation {
                path: source.path.clone(),
            });
        }
        if dest.starts_with(&source.path) {
            return Err(Refusal::IntoOwnDescendant {
                src: source.path.clone(),
                dest: dest.clone(),
            });
        }
        // Copying into the source's own directory is a duplicate, a real intent
        // that resolves through the collision naming below. Moving there would
        // move a file onto itself, which is nothing at all, so say so.
        if kind == Kind::Move && source.path.parent() == Some(dest.as_path()) {
            return Err(Refusal::AlreadyThere {
                path: source.path.clone(),
            });
        }
    }

    let mut names = Names::new(ctx.dest_listing);
    let mut steps = Vec::with_capacity(sources.len());
    for source in sources {
        let name = leaf(&source.path)?;
        let (final_name, renamed, overwrite) = names.take(&name, source.kind, ctx.policy)?;
        steps.push(Step {
            dest: dest.join(final_name),
            src: source.path,
            kind: source.kind,
            nodes: source.nodes,
            items: source.items,
            bytes: source.bytes,
            renamed,
            overwrite,
        });
    }
    Ok(Plan {
        kind,
        dest,
        steps,
        missing: ctx.missing.to_vec(),
        policy: ctx.policy,
    })
}

/// A rename onto an existing entry is **refused**, never auto-suffixed. Pasting
/// says "put this here too" and a suffix is a helpful answer; renaming says
/// "call it this", and answering with a different name would silently defy the
/// one instruction the user typed.
fn rename(source: Source, new_name: String, ctx: &PlanCtx) -> Result<Plan, Refusal> {
    guard(&source.path, ctx)?;
    let Some(parent) = source.path.parent() else {
        return Err(Refusal::FilesystemRoot);
    };
    validate(&new_name)?;
    if leaf(&source.path)? == new_name {
        return Err(Refusal::SameName { name: new_name });
    }
    if ctx.dest_listing.contains(&new_name) {
        return Err(Refusal::NameTaken { name: new_name });
    }
    let dest_dir = parent.to_path_buf();
    let step = Step {
        dest: dest_dir.join(&new_name),
        src: source.path,
        kind: source.kind,
        nodes: source.nodes,
        items: source.items,
        bytes: source.bytes,
        // `renamed` means "suffixed to dodge a collision", which a deliberate
        // rename is not, however tempting the word looks here.
        renamed: false,
        overwrite: false,
    };
    Ok(Plan {
        kind: Kind::Rename,
        dest: dest_dir,
        steps: vec![step],
        missing: ctx.missing.to_vec(),
        policy: ctx.policy,
    })
}

/// A trailing '/' asks for a directory (ADR 0017 D4). Exactly one trailing slash
/// is stripped, so `dir/` is a directory while `a/b/` still carries a separator
/// and is refused: create names one entry in one directory, never a path.
///
/// Creating onto an existing name is refused for the same reason renaming is.
fn create(parent: PathBuf, name: String, ctx: &PlanCtx) -> Result<Plan, Refusal> {
    let as_dir = name.ends_with('/');
    let bare = if as_dir {
        &name[..name.len() - 1]
    } else {
        name.as_str()
    };
    validate(bare)?;
    if ctx.dest_listing.iter().any(|e| e == bare) {
        return Err(Refusal::NameTaken {
            name: bare.to_string(),
        });
    }
    let step = Step {
        src: PathBuf::new(),
        dest: parent.join(bare),
        kind: if as_dir {
            NodeKind::Dir
        } else {
            NodeKind::File
        },
        nodes: Vec::new(),
        items: 1,
        bytes: 0,
        renamed: false,
        overwrite: false,
    };
    Ok(Plan {
        kind: Kind::Create,
        dest: parent,
        steps: vec![step],
        missing: ctx.missing.to_vec(),
        policy: ctx.policy,
    })
}

/// Trash has no destination path to resolve, so it is only the guards plus a
/// step per source. Delete is recoverable (ADR 0017 D7), which is why there is
/// no extra refusal here beyond the ones that protect the user's own footing.
fn trash(sources: Vec<Source>, ctx: &PlanCtx) -> Result<Plan, Refusal> {
    if sources.is_empty() {
        return Err(Refusal::NothingSelected);
    }
    for source in &sources {
        guard(&source.path, ctx)?;
    }
    let steps = sources
        .into_iter()
        .map(|source| Step {
            src: source.path,
            dest: PathBuf::new(),
            kind: source.kind,
            nodes: source.nodes,
            items: source.items,
            bytes: source.bytes,
            renamed: false,
            overwrite: false,
        })
        .collect();
    Ok(Plan {
        kind: Kind::Trash,
        dest: PathBuf::new(),
        steps,
        missing: ctx.missing.to_vec(),
        policy: ctx.policy,
    })
}

/// The two refusals every source faces, whatever the operation (ADR 0017 D5).
///
/// The current directory counts as an ancestor of itself here: moving or
/// trashing the folder you are standing in pulls the ground out from under the
/// browser exactly as moving its parent would, so one test covers both.
fn guard(path: &Path, ctx: &PlanCtx) -> Result<(), Refusal> {
    if path.parent().is_none() {
        return Err(Refusal::FilesystemRoot);
    }
    if ctx.cwd.starts_with(path) {
        return Err(Refusal::AncestorOfCwd {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// The final component of a source path, as the name it will carry into the
/// destination. `path.file_name()` is `None` only for a root or a path ending in
/// `..`, neither of which is a thing to copy.
fn leaf(path: &Path) -> Result<String, Refusal> {
    match path.file_name() {
        Some(name) => Ok(name.to_string_lossy().into_owned()),
        None => Err(Refusal::BadName {
            name: path.to_string_lossy().into_owned(),
        }),
    }
}

/// A typed name must name one entry in one directory. Path separators are tested
/// with [`std::path::is_separator`], which is `/` everywhere and additionally
/// `\` on Windows, so the rule matches what the platform would actually do with
/// the name rather than a guess about it.
fn validate(name: &str) -> Result<(), Refusal> {
    if name.is_empty() || name == "." || name == ".." || name.chars().any(std::path::is_separator) {
        return Err(Refusal::BadName {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// The names already spoken for in the destination, split into what was there
/// before and what this batch has claimed so far.
///
/// The split matters under [`Conflict::Overwrite`]: that toggle is a statement
/// about the destination's *existing* contents, not a licence for two sources in
/// one paste to clobber each other. So a batch-internal collision is still
/// suffixed even when overwriting is on.
struct Names {
    existing: HashSet<String>,
    allocated: HashSet<String>,
}

impl Names {
    fn new(listing: &[String]) -> Self {
        Self {
            existing: listing.iter().cloned().collect(),
            allocated: HashSet::new(),
        }
    }

    /// Claim `name`, returning the name actually taken plus the `(renamed,
    /// overwrite)` flags for the step.
    fn take(
        &mut self,
        name: &str,
        kind: NodeKind,
        policy: Conflict,
    ) -> Result<(String, bool, bool), Refusal> {
        let in_batch = self.allocated.contains(name);
        let in_dest = self.existing.contains(name);
        if !in_batch && !in_dest {
            self.allocated.insert(name.to_string());
            return Ok((name.to_string(), false, false));
        }
        if policy == Conflict::Overwrite && !in_batch {
            self.allocated.insert(name.to_string());
            return Ok((name.to_string(), false, true));
        }
        for n in 2..=MAX_SUFFIX {
            let candidate = suffixed(name, n, kind);
            if !self.existing.contains(&candidate) && !self.allocated.contains(&candidate) {
                self.allocated.insert(candidate.clone());
                return Ok((candidate, true, false));
            }
        }
        Err(Refusal::NameTaken {
            name: name.to_string(),
        })
    }
}

/// `foo.txt` at `n = 2` becomes `foo (2).txt`.
///
/// The counter is inserted before the **last** extension only, so `foo.tar.gz`
/// becomes `foo.tar (2).gz` rather than `foo (2).tar.gz`. Both readings are
/// defensible; this one is chosen because "the extension" is what the OS, the
/// browser's own classifier, and every file dialog mean by the characters after
/// the final dot, and treating `.tar.gz` as one unit would need a list of
/// blessed double extensions that would then be wrong for `.tar.zst` next week.
///
/// Two names have no split to make: a dotfile like `.gitignore` is all name and
/// no extension (the leading dot is not a separator), and a directory has no
/// extension to preserve at all, so `v1.2` stays `v1.2 (2)` instead of being cut
/// into `v1 (2).2`.
fn suffixed(name: &str, n: usize, kind: NodeKind) -> String {
    if kind != NodeKind::Dir {
        if let Some(dot) = name.rfind('.') {
            if dot > 0 && dot + 1 < name.len() {
                return format!("{} ({n}).{}", &name[..dot], &name[dot + 1..]);
            }
        }
    }
    format!("{name} ({n})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A source as `collect` would return it for a plain file.
    fn file(path: &str, bytes: u64) -> Source {
        Source {
            path: PathBuf::from(path),
            kind: NodeKind::File,
            nodes: Vec::new(),
            items: 1,
            bytes,
        }
    }

    /// A source as `collect` would return it for a directory holding `nodes`.
    fn dir(path: &str, nodes: &[(&str, NodeKind, u64)]) -> Source {
        let nodes: Vec<Node> = nodes
            .iter()
            .map(|(rel, kind, size)| Node {
                rel: PathBuf::from(rel),
                kind: *kind,
                size: *size,
            })
            .collect();
        Source {
            path: PathBuf::from(path),
            kind: NodeKind::Dir,
            items: 1 + nodes.len(),
            bytes: nodes.iter().map(|n| n.size).sum(),
            nodes,
        }
    }

    /// The default world: nothing marked missing, standing well away from the
    /// paths under test, suffixing on collision.
    fn ctx<'a>(listing: &'a [String], cwd: &'a str) -> PlanCtx<'a> {
        PlanCtx {
            dest_listing: listing,
            cwd: Path::new(cwd),
            missing: &[],
            policy: Conflict::Rename,
        }
    }

    fn dests(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| s.dest.to_string_lossy().into_owned())
            .collect()
    }

    fn copy(sources: Vec<Source>, dest: &str, ctx: &PlanCtx) -> Result<Plan, Refusal> {
        plan(
            Op::Copy {
                sources,
                dest: PathBuf::from(dest),
            },
            ctx,
        )
    }

    #[test]
    fn free_names_are_kept_verbatim() {
        let listing = names(&["other.txt"]);
        let p = copy(vec![file("/src/a.txt", 10)], "/dst", &ctx(&listing, "/dst"))
            .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a.txt"]);
        assert!(!p.steps[0].renamed);
        assert!(!p.steps[0].overwrite);
        assert_eq!(p.renamed(), 0);
        assert_eq!(p.kind, Kind::Copy);
        assert_eq!(p.dest, PathBuf::from("/dst"));
    }

    #[test]
    fn collision_suffix_goes_before_the_last_extension() {
        let listing = names(&["a.txt", "b.tar.gz"]);
        let p = copy(
            vec![file("/src/a.txt", 1), file("/src/b.tar.gz", 2)],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a (2).txt", "/dst/b.tar (2).gz"]);
        assert!(p.steps.iter().all(|s| s.renamed));
        assert_eq!(p.renamed(), 2);
    }

    #[test]
    fn collision_counts_up_until_a_free_name() {
        let listing = names(&["a.txt", "a (2).txt", "a (3).txt"]);
        let p = copy(vec![file("/src/a.txt", 1)], "/dst", &ctx(&listing, "/dst"))
            .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a (4).txt"]);
    }

    #[test]
    fn dotfiles_have_no_extension_to_split() {
        let listing = names(&[".gitignore"]);
        let p = copy(
            vec![file("/src/.gitignore", 1)],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/.gitignore (2)"]);
    }

    #[test]
    fn directories_keep_their_dots() {
        let listing = names(&["docs", "v1.2"]);
        let p = copy(
            vec![dir("/src/docs", &[]), dir("/src/v1.2", &[])],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/docs (2)", "/dst/v1.2 (2)"]);
    }

    #[test]
    fn a_trailing_dot_is_not_an_extension() {
        let listing = names(&["odd."]);
        let p = copy(vec![file("/src/odd.", 1)], "/dst", &ctx(&listing, "/dst"))
            .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/odd. (2)"]);
    }

    #[test]
    fn two_sources_sharing_a_name_never_collide_with_each_other() {
        // Marked in two different folders, pasted once (ADR 0017 D3).
        let listing = names(&[]);
        let p = copy(
            vec![file("/one/a.txt", 1), file("/two/a.txt", 2)],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a.txt", "/dst/a (2).txt"]);
        assert!(!p.steps[0].renamed);
        assert!(p.steps[1].renamed);
    }

    #[test]
    fn overwrite_policy_keeps_the_name_and_flags_the_step() {
        let listing = names(&["a.txt"]);
        let world = PlanCtx {
            dest_listing: &listing,
            cwd: Path::new("/dst"),
            missing: &[],
            policy: Conflict::Overwrite,
        };
        let p = copy(vec![file("/src/a.txt", 1)], "/dst", &world).expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a.txt"]);
        assert!(p.steps[0].overwrite);
        assert!(!p.steps[0].renamed);
        assert_eq!(p.overwrites(), 1);
        assert_eq!(p.policy, Conflict::Overwrite);
    }

    #[test]
    fn overwrite_policy_still_separates_two_sources_in_one_batch() {
        // Overwrite is about what is already in the destination, never a licence
        // for the batch to eat itself.
        let listing = names(&["a.txt"]);
        let world = PlanCtx {
            dest_listing: &listing,
            cwd: Path::new("/dst"),
            missing: &[],
            policy: Conflict::Overwrite,
        };
        let p = copy(
            vec![file("/one/a.txt", 1), file("/two/a.txt", 2)],
            "/dst",
            &world,
        )
        .expect("plan should resolve");
        assert_eq!(dests(&p), vec!["/dst/a.txt", "/dst/a (2).txt"]);
        assert!(p.steps[0].overwrite);
        assert!(p.steps[1].renamed);
        assert!(!p.steps[1].overwrite);
    }

    #[test]
    fn copying_into_the_sources_own_directory_duplicates_it() {
        let listing = names(&["a.txt"]);
        let p = copy(
            vec![file("/dst/a.txt", 1)],
            "/dst",
            &ctx(&listing, "/elsewhere"),
        )
        .expect("a duplicate is a real intent");
        assert_eq!(dests(&p), vec!["/dst/a (2).txt"]);
    }

    #[test]
    fn moving_into_the_sources_own_directory_is_refused() {
        let listing = names(&["a.txt"]);
        let err = plan(
            Op::Move {
                sources: vec![file("/dst/a.txt", 1)],
                dest: PathBuf::from("/dst"),
            },
            &ctx(&listing, "/elsewhere"),
        )
        .expect_err("moving a file onto itself is nothing");
        assert_eq!(
            err,
            Refusal::AlreadyThere {
                path: PathBuf::from("/dst/a.txt")
            }
        );
    }

    #[test]
    fn an_empty_selection_is_refused() {
        let listing = names(&[]);
        assert_eq!(
            copy(vec![], "/dst", &ctx(&listing, "/dst")).expect_err("nothing to do"),
            Refusal::NothingSelected
        );
        assert_eq!(
            plan(Op::Trash { sources: vec![] }, &ctx(&listing, "/dst")).expect_err("nothing to do"),
            Refusal::NothingSelected
        );
    }

    #[test]
    fn a_destination_inside_the_source_is_refused() {
        let listing = names(&[]);
        let err = copy(
            vec![dir("/src/tree", &[("leaf", NodeKind::File, 1)])],
            "/src/tree/deep",
            &ctx(&listing, "/elsewhere"),
        )
        .expect_err("a tree cannot contain itself");
        assert_eq!(
            err,
            Refusal::IntoOwnDescendant {
                src: PathBuf::from("/src/tree"),
                dest: PathBuf::from("/src/tree/deep"),
            }
        );
    }

    #[test]
    fn a_destination_equal_to_the_source_is_refused() {
        let listing = names(&[]);
        let err = copy(
            vec![dir("/src/tree", &[])],
            "/src/tree",
            &ctx(&listing, "/elsewhere"),
        )
        .expect_err("the source is not its own destination");
        assert_eq!(
            err,
            Refusal::SameLocation {
                path: PathBuf::from("/src/tree")
            }
        );
    }

    #[test]
    fn the_filesystem_root_is_refused() {
        let listing = names(&[]);
        let err = copy(
            vec![dir("/", &[])],
            "/dst",
            &PlanCtx {
                dest_listing: &listing,
                // `cwd` deliberately not under the source, so this is the root
                // rule firing and not the ancestor rule.
                cwd: Path::new("/"),
                missing: &[],
                policy: Conflict::Rename,
            },
        )
        .expect_err("/ is not a thing to copy");
        assert_eq!(err, Refusal::FilesystemRoot);
    }

    #[test]
    fn an_ancestor_of_the_current_directory_is_refused() {
        let listing = names(&[]);
        let err = copy(
            vec![dir("/home/me/project", &[])],
            "/dst",
            &ctx(&listing, "/home/me/project/src/deep"),
        )
        .expect_err("that folder holds the user's own footing");
        assert_eq!(
            err,
            Refusal::AncestorOfCwd {
                path: PathBuf::from("/home/me/project")
            }
        );
    }

    #[test]
    fn the_current_directory_itself_counts_as_an_ancestor() {
        let listing = names(&[]);
        let err = plan(
            Op::Trash {
                sources: vec![dir("/home/me/project", &[])],
            },
            &ctx(&listing, "/home/me/project"),
        )
        .expect_err("trashing the folder you stand in is the same surprise");
        assert_eq!(
            err,
            Refusal::AncestorOfCwd {
                path: PathBuf::from("/home/me/project")
            }
        );
    }

    #[test]
    fn rename_maps_the_source_to_a_sibling_path() {
        let listing = names(&["old.txt", "other.txt"]);
        let p = plan(
            Op::Rename {
                source: file("/dst/old.txt", 42),
                new_name: "new.txt".to_string(),
            },
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(p.kind, Kind::Rename);
        assert_eq!(dests(&p), vec!["/dst/new.txt"]);
        assert_eq!(p.steps[0].src, PathBuf::from("/dst/old.txt"));
        assert_eq!(p.dest, PathBuf::from("/dst"));
        // The step was not suffixed to dodge anything, so the flag stays clear.
        assert!(!p.steps[0].renamed);
        assert_eq!(p.bytes(), 42);
    }

    #[test]
    fn rename_onto_an_existing_name_is_refused_not_suffixed() {
        let listing = names(&["old.txt", "taken.txt"]);
        let err = plan(
            Op::Rename {
                source: file("/dst/old.txt", 1),
                new_name: "taken.txt".to_string(),
            },
            &ctx(&listing, "/dst"),
        )
        .expect_err("a rename says what the name must be");
        assert_eq!(
            err,
            Refusal::NameTaken {
                name: "taken.txt".to_string()
            }
        );
    }

    #[test]
    fn rename_to_the_same_name_is_refused() {
        let listing = names(&["old.txt"]);
        let err = plan(
            Op::Rename {
                source: file("/dst/old.txt", 1),
                new_name: "old.txt".to_string(),
            },
            &ctx(&listing, "/dst"),
        )
        .expect_err("changing nothing is not an operation");
        assert_eq!(
            err,
            Refusal::SameName {
                name: "old.txt".to_string()
            }
        );
    }

    #[test]
    fn bad_names_are_refused_for_rename_and_create() {
        let listing = names(&[]);
        let world = ctx(&listing, "/dst");
        for bad in ["", ".", "..", "a/b", "/abs"] {
            let err = plan(
                Op::Rename {
                    source: file("/dst/old.txt", 1),
                    new_name: bad.to_string(),
                },
                &world,
            )
            .expect_err("not a usable name");
            assert_eq!(
                err,
                Refusal::BadName {
                    name: bad.to_string()
                },
                "rename {bad:?}"
            );
            let err = plan(
                Op::Create {
                    parent: PathBuf::from("/dst"),
                    name: bad.to_string(),
                },
                &world,
            )
            .expect_err("not a usable name");
            assert_eq!(
                err,
                Refusal::BadName {
                    name: bad.to_string()
                },
                "create {bad:?}"
            );
        }
        // A bare "/" is the directory form of the empty name, so it lands on the
        // same refusal with the trailing slash already stripped.
        assert_eq!(
            plan(
                Op::Create {
                    parent: PathBuf::from("/dst"),
                    name: "/".to_string(),
                },
                &world,
            )
            .expect_err("an unnamed directory"),
            Refusal::BadName {
                name: String::new()
            }
        );
    }

    #[test]
    #[cfg(windows)]
    fn a_backslash_is_a_separator_on_windows() {
        let listing = names(&[]);
        assert_eq!(
            plan(
                Op::Create {
                    parent: PathBuf::from("/dst"),
                    name: "a\\b".to_string(),
                },
                &ctx(&listing, "/dst"),
            )
            .expect_err("a name is not a path"),
            Refusal::BadName {
                name: "a\\b".to_string()
            }
        );
    }

    #[test]
    fn create_makes_a_file_or_a_directory_by_its_trailing_slash() {
        let listing = names(&[]);
        let world = ctx(&listing, "/dst");
        let f = plan(
            Op::Create {
                parent: PathBuf::from("/dst"),
                name: "notes.md".to_string(),
            },
            &world,
        )
        .expect("plan should resolve");
        assert_eq!(f.kind, Kind::Create);
        assert_eq!(f.steps[0].kind, NodeKind::File);
        assert_eq!(dests(&f), vec!["/dst/notes.md"]);
        // A create has no source, and says so with an empty path.
        assert_eq!(f.steps[0].src, PathBuf::new());
        assert_eq!(f.items(), 1);
        assert_eq!(f.bytes(), 0);

        let d = plan(
            Op::Create {
                parent: PathBuf::from("/dst"),
                name: "sub/".to_string(),
            },
            &world,
        )
        .expect("plan should resolve");
        assert_eq!(d.steps[0].kind, NodeKind::Dir);
        assert_eq!(dests(&d), vec!["/dst/sub"]);
    }

    #[test]
    fn create_onto_an_existing_name_is_refused() {
        let listing = names(&["notes.md", "sub"]);
        let world = ctx(&listing, "/dst");
        assert_eq!(
            plan(
                Op::Create {
                    parent: PathBuf::from("/dst"),
                    name: "notes.md".to_string(),
                },
                &world,
            )
            .expect_err("create never suffixes"),
            Refusal::NameTaken {
                name: "notes.md".to_string()
            }
        );
        assert_eq!(
            plan(
                Op::Create {
                    parent: PathBuf::from("/dst"),
                    name: "sub/".to_string(),
                },
                &world,
            )
            .expect_err("create never suffixes"),
            Refusal::NameTaken {
                name: "sub".to_string()
            }
        );
    }

    #[test]
    fn trash_plans_one_step_per_source_with_no_destination() {
        let listing = names(&[]);
        let p = plan(
            Op::Trash {
                sources: vec![
                    file("/dst/a.txt", 10),
                    dir("/dst/tree", &[("leaf", NodeKind::File, 5)]),
                ],
            },
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(p.kind, Kind::Trash);
        assert_eq!(p.dest, PathBuf::new());
        assert_eq!(p.steps.len(), 2);
        assert!(p.steps.iter().all(|s| s.dest == PathBuf::new()));
        assert_eq!(p.items(), 3);
        assert_eq!(p.bytes(), 15);
    }

    #[test]
    fn totals_sum_items_and_bytes_across_steps() {
        let listing = names(&[]);
        let p = copy(
            vec![
                file("/src/a.bin", 1_000),
                dir(
                    "/src/tree",
                    &[
                        ("one", NodeKind::File, 200),
                        ("sub", NodeKind::Dir, 0),
                        ("sub/two", NodeKind::File, 24),
                    ],
                ),
            ],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        // One file, plus a tree root and its three nodes.
        assert_eq!(p.items(), 1 + 4);
        assert_eq!(p.bytes(), 1_224);
        // The tree's nodes ride along untouched for the executor to replay.
        assert_eq!(p.steps[1].nodes.len(), 3);
        assert_eq!(p.steps[1].nodes[2].rel, PathBuf::from("sub/two"));
    }

    #[test]
    fn summary_reads_as_one_sentence() {
        let listing = names(&["a.txt"]);
        let p = copy(
            vec![file("/one/a.txt", 3 * 1024), file("/two/b.txt", 1024)],
            "/dst",
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        assert_eq!(p.summary(), "copy 2 items, 4.0K, 1 renamed");

        let single = plan(
            Op::Create {
                parent: PathBuf::from("/dst"),
                name: "new.md".to_string(),
            },
            &ctx(&listing, "/dst"),
        )
        .expect("plan should resolve");
        // No bytes and no collisions, so no clauses that would say nothing.
        assert_eq!(single.summary(), "create 1 item");
    }

    #[test]
    fn vanished_marks_ride_along_into_the_plan() {
        // ADR 0017 D3: pruned at plan time and reported, never silently dropped.
        let listing = names(&[]);
        let gone = vec![PathBuf::from("/src/gone.txt")];
        let world = PlanCtx {
            dest_listing: &listing,
            cwd: Path::new("/dst"),
            missing: &gone,
            policy: Conflict::Rename,
        };
        let p = copy(vec![file("/src/here.txt", 1)], "/dst", &world).expect("plan should resolve");
        assert_eq!(p.missing, gone);
    }

    #[test]
    fn every_refusal_renders_one_sentence() {
        let cases = [
            Refusal::NothingSelected,
            Refusal::IntoOwnDescendant {
                src: PathBuf::from("/a"),
                dest: PathBuf::from("/a/b"),
            },
            Refusal::SameLocation {
                path: PathBuf::from("/a"),
            },
            Refusal::AlreadyThere {
                path: PathBuf::from("/a/b.txt"),
            },
            Refusal::SameName {
                name: "b.txt".to_string(),
            },
            Refusal::BadName {
                name: "..".to_string(),
            },
            Refusal::NameTaken {
                name: "b.txt".to_string(),
            },
            Refusal::FilesystemRoot,
            Refusal::AncestorOfCwd {
                path: PathBuf::from("/home"),
            },
            Refusal::TooLarge { limit: 50_000 },
            Refusal::TooDeep {
                path: PathBuf::from("/a/deep"),
                limit: 64,
            },
            Refusal::Io {
                path: PathBuf::from("/a/locked"),
                msg: "permission denied".to_string(),
            },
        ];
        for case in cases {
            let text = case.to_string();
            assert!(!text.is_empty(), "{case:?} renders nothing");
            assert!(!text.contains('\n'), "{case:?} is more than one line");
        }
        assert_eq!(
            Refusal::TooLarge { limit: 50_000 }.to_string(),
            "refusing: this spans more than 50000 items"
        );
        assert_eq!(
            Refusal::Io {
                path: PathBuf::from("/a/locked"),
                msg: "permission denied".to_string(),
            }
            .to_string(),
            "/a/locked: permission denied"
        );
    }
}
