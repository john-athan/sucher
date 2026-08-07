//! The last of the three stages: replay a decided [`Plan`] on a background
//! thread, record what actually happened, and hand back a journal undo can trust
//! (ADR 0017 D6, D7, D8).
//!
//! `plan` already settled every question worth arguing about: which source
//! becomes which destination name, what is refused, what is suffixed. Nothing
//! here re-decides any of it. What is left is the part that can only be done
//! against a real filesystem, and the two obligations that come with touching
//! one.
//!
//! ## Shape of the module
//! [`start`] spawns exactly one owning thread and returns a [`Run`] immediately,
//! so a multi-gigabyte copy never blocks the UI. The thread streams
//! [`Msg::Progress`] over an `mpsc` and finishes with exactly one
//! [`Msg::Done`]; the browser drains the channel once per loop iteration
//! ([`Run::drain`]) on the same 60 ms tier as `pump_search`. Dropping the handle
//! trips a shared cancel flag, exactly as `search::Search` does, so a run cannot
//! outlive the screen that asked for it.
//!
//! [`start_undo`] is the same door for the other direction, and running undo
//! through it rather than calling it inline is a correctness decision, not
//! symmetry for its own sake. Reversing a cross-device move copies the whole
//! tree home, so an undo invoked from the key handler would freeze the UI for
//! exactly as long as the original move took. "The common case is fast" is the
//! argument ADR 0009 rejected when it bounded the decoders instead of trusting
//! typical inputs, and it is rejected here for the same reason. Both directions
//! therefore stream the same messages and end in the same [`Report`], so the
//! browser has one pipeline for every mutation rather than a second, differently
//! shaped one for `U`.
//!
//! ## Obligation one: nothing is destroyed
//! ADR 0017 D7 says delete means trash, and that rule is total. It reaches two
//! places beyond the `D` binding the ADR text describes:
//!
//!   * An **overwrite** does not destroy the entry it displaces. The existing
//!     destination is sent to the trash first and the new entry is written
//!     second. If the trash refuses, the step fails and nothing is written.
//!   * **Undoing a copy or a create** sends what sucher made to the trash rather
//!     than unlinking it. A copied directory may have gained files since it
//!     landed, and a recursive unlink of a tree the user has touched is exactly
//!     the surprise this project refuses to ship.
//!
//! Where there is no trash, the step fails honestly and changes nothing further.
//! It never falls back to `unlink`. A silent downgrade from recoverable to
//! unrecoverable is the same class of surprise as a `.db` that quietly parses as
//! something other than SQLite (ADR 0016), and the answer is the same: an honest
//! error (ADR 0009).
//!
//! ## Obligation two: the journal can never claim more than happened
//! [`Journal`] entries are appended **as each thing occurs**, not planned up
//! front, which is what makes undo correct after a partial failure or a
//! cancellation (ADR 0017 D8). A directory root is journalled the moment it
//! exists on disk rather than once its tree is finished, so a copy that was
//! stopped halfway is still something [`undo`] can take away again.
//!
//! Because the plan is a snapshot, every step re-checks its destination against
//! the real filesystem before writing. A destination that appeared since
//! planning fails that one step and the run continues with the rest: the stale
//! snapshot is never grounds to clobber.
//!
//! Undo carries the same weight as the forward run, which is why ADR 0017 D6's
//! cross-device fallback is implemented in both directions. On the FUSE mounts
//! D6 was written for every move crosses a device, so a reverse `fs::rename`
//! fails for exactly the reason the forward one did. Undo transplants back
//! instead, through the same tree copy the forward move used, so the round trip
//! is lossless rather than undo being unavailable on the main road.
//!
//! ## Obligation three: an undo is a run, and says so
//! An undo reports through [`Report`] like everything else, with two readings
//! that only apply to it. Its `journal` is always empty, because an undo
//! produces nothing to undo again. And the paths it could not restore because
//! only the system trash holds them are [`Report::notes`], not [`Failure`]s:
//! nothing went wrong there, and a user scanning a red list for what broke must
//! not have to read past advice to find it.

use super::collect;
use super::plan::{Kind, Node, NodeKind, Plan, Step};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often progress is allowed onto the channel. The UI polls at 60 ms, so a
/// message per file would flood the channel on a large tree and change nothing a
/// human could see. Cumulative counters plus one guaranteed final send before
/// [`Msg::Done`] mean the status line still ends on the true totals.
const PROGRESS_EVERY: Duration = Duration::from_millis(50);

/// The errno for "the rename would cross a filesystem boundary": `EXDEV` is 18
/// on Linux and macOS, `ERROR_NOT_SAME_DEVICE` is 17 on Windows.
#[cfg(unix)]
const CROSS_DEVICE: i32 = 18;
#[cfg(windows)]
const CROSS_DEVICE: i32 = 17;

/// A message from the background run to the UI thread.
pub enum Msg {
    /// Cumulative totals plus the path being worked on, throttled to
    /// [`PROGRESS_EVERY`] so the status line can say where the run is without
    /// the channel carrying one message per file.
    Progress {
        items: usize,
        bytes: u64,
        current: PathBuf,
    },
    /// Sent exactly once, whether the run finished, failed partway, or was
    /// cancelled.
    Done(Report),
}

/// What one run did, forward or backward. Sent as the final message and kept by
/// the browser so `U` has something to undo.
#[derive(Debug)]
pub struct Report {
    /// Which operation this report is about. For an undo it is the kind of the
    /// operation being reversed, which is why [`Report::direction`] exists
    /// beside it rather than instead of it.
    pub kind: Kind,
    pub direction: Direction,
    /// Filesystem entries touched, except on an undo, where it is the number of
    /// journal steps actually reversed. That is what the user is owed after
    /// pressing `U`: a cross-device restore carries a whole tree home, and
    /// reporting ten thousand items would describe the labour rather than the
    /// outcome, which was to put one move back.
    pub items: usize,
    /// Payload bytes moved. An undo counts only what it copied with its own
    /// hands, because the journal records paths and not sizes (ADR 0017 D8), so
    /// a restore performed by a rename honestly has nothing to add here.
    pub bytes: u64,
    /// Steps that failed, each with an honest one-line reason. A non-empty
    /// failure list is reported to the user, never swallowed (ADR 0009).
    pub failures: Vec<Failure>,
    /// Things worth knowing that are not failures, shown beside the failure list
    /// and never inside it. The separation is the point: a user reading a red
    /// list of what went wrong must not have to sort advice out of it. Forward
    /// operations leave this empty, so nothing about them changes.
    pub notes: Vec<String>,
    /// What this run did, for `U` to reverse. **Always empty on an undo**, since
    /// an undo produces nothing to undo again; `dir.rs` skips pushing an empty
    /// journal onto the bounded stack, and that is what stops `U` from stacking
    /// undos of undos (ADR 0017 D8).
    pub journal: Journal,
}

/// Which way a run travelled.
///
/// [`Kind`] answers "which operation", and it lives in `plan` because it names
/// what a [`Plan`] describes; an undo has no plan, so `Kind` cannot be taught to
/// say "undo of a move" without every match on it growing an arm that means
/// something of a different order. The direction therefore travels beside the
/// kind and the browser composes the sentence from both: `Undo` plus
/// [`Kind::Move`] reads "undid the move of 3 items", where `kind` alone would
/// have the status line claim a move had just happened when one had just been
/// taken back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// A planned operation, replayed by [`execute`].
    #[default]
    Forward,
    /// A journal replayed in reverse by [`execute_undo`].
    Undo,
}

/// One thing that did not happen, and the sentence explaining why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub path: PathBuf,
    pub msg: String,
}

/// What actually happened, recorded as it happened, so undo can never claim more
/// than was done (ADR 0017 D8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    pub kind: Kind,
    pub steps: Vec<Undoable>,
}

/// One journalled fact, and its inverse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Undoable {
    /// A rename or a move: undo puts it back.
    Moved { from: PathBuf, to: PathBuf },
    /// Something sucher itself brought into being (a copied tree, a new file or
    /// directory). Undo sends it to the trash rather than unlinking it, because
    /// the tree may have gained files since it landed.
    Created { path: PathBuf },
    /// Sent to the OS trash. Not undoable in process; recorded so undo can say
    /// so instead of pretending (ADR 0017 D8).
    Trashed { path: PathBuf },
}

/// A live operation. Owns the receiver plus the cancel handle; dropping it (or
/// calling [`Run::cancel`]) stops the worker, exactly as `search::Search` does.
/// The browser holds at most one of these at a time (ADR 0017 D2).
pub struct Run {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl Run {
    /// Drain every message available right now, non-blocking. The UI calls this
    /// once per loop iteration to move the progress bar and notice completion.
    pub fn drain(&self) -> Vec<Msg> {
        self.rx.try_iter().collect()
    }

    /// Signal the worker to stop. Idempotent; also invoked on drop.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for Run {
    /// A dropped `Run` must not leave its worker mutating the filesystem behind
    /// a screen that has moved on.
    fn drop(&mut self) {
        self.cancel();
    }
}

/// The one door to the OS trash, kept as a seam for two reasons. It is the only
/// call in this module with a side effect the developer's own machine would
/// keep, so the tests substitute a recorder rather than filling anyone's Finder
/// trash with fixtures. And naming it makes ADR 0017 D7 auditable: every path
/// that leaves sucher's control goes through exactly this trait.
trait Trash {
    fn send(&self, path: &Path) -> Result<(), String>;
}

/// The real thing, used everywhere outside tests.
struct OsTrash;

impl Trash for OsTrash {
    /// A platform or filesystem without a trash yields a message that says
    /// plainly why nothing happened, so the user is never left guessing whether
    /// the file was quietly destroyed instead (ADR 0017 D7).
    fn send(&self, path: &Path) -> Result<(), String> {
        trash_context().delete(path).map_err(|e| {
            format!(
                "cannot send {} to the trash ({e}), and sucher never permanently deletes, so nothing was removed",
                path.display()
            )
        })
    }
}

/// The configured trash handle.
///
/// On macOS this is not a formality. The `trash` crate's default delete method
/// there drives **Finder over AppleScript**, which needs the "control Finder"
/// Automation permission. In a terminal that has never been granted it, the call
/// does not fail: it blocks, waiting on a consent prompt that a non-GUI process
/// never receives. A smoke test caught exactly that, with `D` sitting at `0/2`
/// forever, and the consequences compound: the worker is blocked inside the
/// syscall, so the cancel flag it only checks between steps never gets read, and
/// `q` refuses to quit while an operation is in flight. A hung trash would have
/// trapped the user in the browser.
///
/// `NsFileManager` is the direct `-[NSFileManager trashItemAtURL:...]` call: no
/// Finder, no Automation permission, no prompt, and about 40 ms in practice. It
/// is what "move to trash" means at the API level, so it is also the more honest
/// of the two.
///
/// A context is built per call rather than cached: it is a tiny value, deleting
/// is already a syscall, and a shared handle would need synchronising across the
/// worker threads for nothing.
fn trash_context() -> trash::TrashContext {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx
}

/// The rename seam, the second of this module's two substitutable edges.
///
/// It exists for the same reason the [`Trash`] seam does: the behaviour behind
/// it cannot be reached from a test otherwise. Whether two paths sit on the same
/// device is a property of the machine, and every path a test can create lives
/// inside one temp directory, so the cross-device half of ADR 0017 D6 would go
/// permanently untested on exactly the mounts (S3, GCS, rclone) where it is the
/// normal case. Substituting a rename that reports `CrossesDevices` exercises
/// the real fallback instead of a mock of it.
type Rename = fn(&Path, &Path) -> io::Result<()>;

/// The real rename, used everywhere outside tests.
fn os_rename(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

/// Start replaying `plan` on a background thread, returning immediately.
pub fn start(plan: Plan) -> Run {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    std::thread::spawn(move || {
        execute(plan, &OsTrash, os_rename, &tx, &flag);
    });
    Run { rx, cancel }
}

/// Start replaying `journal`'s inverses on a background thread, returning
/// immediately. The mirror of [`start`], down to the [`Run`] it hands back.
///
/// The journal is taken by value because the worker outlives the caller's frame,
/// and because a journal that has been undone has no second use: the browser
/// pops it off the stack to get here (ADR 0017 D8).
pub fn start_undo(journal: Journal) -> Run {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    std::thread::spawn(move || {
        execute_undo(journal, &OsTrash, os_rename, &tx, &flag);
    });
    Run { rx, cancel }
}

/// The body of the worker, with the two seams and the channel passed in so the
/// tests can drive it synchronously against a recorder.
///
/// Steps run in plan order. A failing step is recorded and the run carries on
/// with the rest: one destination that changed under us is not a reason to
/// abandon the other three files the user asked for.
fn execute(plan: Plan, trash: &dyn Trash, rename: Rename, tx: &Sender<Msg>, cancel: &AtomicBool) {
    let Plan { kind, steps, .. } = plan;
    let mut ex = Exec::new(kind, Direction::Forward, trash, rename, tx, cancel);
    for step in steps {
        // Checked between steps, so a cancelled run leaves a journal of exactly
        // what completed rather than a half-written entry.
        if ex.cancelled() {
            break;
        }
        let outcome = match kind {
            Kind::Copy => ex.copy_step(&step),
            Kind::Move => ex.move_step(&step),
            Kind::Rename => ex.rename_step(&step),
            Kind::Create => ex.create_step(&step),
            Kind::Trash => ex.trash_step(&step),
        };
        if let Err(msg) = outcome {
            // A trash step has no destination path to name, so it is reported
            // against its source.
            let at = if step.dest.as_os_str().is_empty() {
                &step.src
            } else {
                &step.dest
            };
            ex.fail(at, msg);
        }
    }
    ex.finish();
}

/// The undo counterpart of [`execute`], with the same two seams and the same
/// channel, so an undo is driven by the tests exactly as a forward run is.
///
/// Steps run newest first, and the reverse order is what makes an overwrite undo
/// correctly: the copy that displaced something is taken away before the
/// displaced entry is reported, so the user is told about the trash after the
/// thing sitting on top of it is gone.
///
/// Each arm reports its own failures rather than returning them, because unlike
/// a forward step the path to blame differs by case: a refused restore is the
/// destination's fault, an occupied original is the source's.
fn execute_undo(
    journal: Journal,
    trash: &dyn Trash,
    rename: Rename,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
) {
    let Journal { kind, steps } = journal;
    let mut ex = Exec::new(kind, Direction::Undo, trash, rename, tx, cancel);
    // Gathered here rather than on `Exec` because it is folded into exactly one
    // note at the end and a forward run would otherwise carry a field that is
    // always empty.
    let mut trash_only: Vec<PathBuf> = Vec::new();
    for step in steps.iter().rev() {
        // Checked between steps exactly as the forward run does, so cancelling
        // an undo stops it where it stands and still reports what it managed.
        if ex.cancelled() {
            break;
        }
        match step {
            Undoable::Moved { from, to } => ex.restore(from, to),
            // Trashed, never unlinked: a copied directory may have gained files
            // since it landed, and a recursive unlink of a tree the user has
            // touched is the surprise this project refuses to ship.
            Undoable::Created { path } => ex.remove(path),
            // Not restorable in process, and not attempted: `trash`'s restore
            // API is not available on every platform, and a half-supported undo
            // is worse than an honest pointer to Finder (ADR 0017 D8).
            Undoable::Trashed { path } => trash_only.push(path.clone()),
        }
    }
    if !trash_only.is_empty() {
        ex.note(trash_only_note(&trash_only));
    }
    // `ex.journal` is deliberately never appended to on this path. An undo
    // produces nothing to undo again, and `dir.rs` skips pushing an empty
    // journal, so this omission is what stops `U` from stacking undos of undos
    // (ADR 0017 D8). It is stated here rather than left to fall out of the fact
    // that no arm above happens to record anything.
    ex.finish();
}

/// The one note an undo can produce: the paths it deliberately did not restore.
///
/// It names them rather than counting them, because "2 paths are in the trash"
/// leaves the user to work out which two, and it says where they come back from,
/// because the honest pointer to Finder is the whole substitute for the in-process
/// restore ADR 0017 D8 declines to attempt.
fn trash_only_note(paths: &[PathBuf]) -> String {
    let named: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    format!(
        "{} went to the system trash, and that is where {} come back from: sucher does not restore from it",
        named.join(", "),
        if paths.len() == 1 { "it does" } else { "they do" }
    )
}

/// The mutable state of one run in either direction: the counters the progress
/// messages carry, the failures and notes collected so far, and the journal being
/// written as things happen.
///
/// One accumulator serves both directions on purpose. An undo that assembled its
/// own shape and translated it into a [`Report`] at the end would be the second
/// pipeline this module exists to not have.
struct Exec<'a> {
    trash: &'a dyn Trash,
    rename: Rename,
    tx: &'a Sender<Msg>,
    cancel: &'a AtomicBool,
    direction: Direction,
    items: usize,
    bytes: u64,
    current: PathBuf,
    last: Instant,
    failures: Vec<Failure>,
    notes: Vec<String>,
    journal: Journal,
}

impl<'a> Exec<'a> {
    fn new(
        kind: Kind,
        direction: Direction,
        trash: &'a dyn Trash,
        rename: Rename,
        tx: &'a Sender<Msg>,
        cancel: &'a AtomicBool,
    ) -> Self {
        Exec {
            trash,
            rename,
            tx,
            cancel,
            direction,
            items: 0,
            bytes: 0,
            current: PathBuf::new(),
            last: Instant::now(),
            failures: Vec::new(),
            notes: Vec::new(),
            journal: Journal {
                kind,
                steps: Vec::new(),
            },
        }
    }

    fn fail(&mut self, path: &Path, msg: String) {
        self.failures.push(Failure {
            path: path.to_path_buf(),
            msg,
        });
    }

    /// Record something the user should know that is not something that went
    /// wrong. Kept apart from [`Exec::fail`] all the way to the overlay.
    fn note(&mut self, text: String) {
        self.notes.push(text);
    }

    /// Account for one entry and let the throttle decide whether to say so.
    fn tick(&mut self, current: &Path, items: usize, bytes: u64) {
        self.items += items;
        self.bytes += bytes;
        self.current = current.to_path_buf();
        if self.last.elapsed() >= PROGRESS_EVERY {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.last = Instant::now();
        let _ = self.tx.send(Msg::Progress {
            items: self.items,
            bytes: self.bytes,
            current: self.current.clone(),
        });
    }

    /// One last unthrottled progress message, then exactly one [`Msg::Done`], so
    /// the status line ends on the real totals rather than on whatever the
    /// throttle happened to let through last.
    fn finish(mut self) {
        self.flush();
        let kind = self.journal.kind;
        let _ = self.tx.send(Msg::Done(Report {
            kind,
            direction: self.direction,
            items: self.items,
            bytes: self.bytes,
            failures: self.failures,
            notes: self.notes,
            journal: self.journal,
        }));
    }

    /// Make the destination safe to write, against the filesystem as it is now
    /// rather than as the plan's snapshot remembered it.
    ///
    /// `symlink_metadata` rather than `exists`, so a broken symlink squatting on
    /// the destination still counts as something that is there. Without
    /// `step.overwrite` this is a failure and the step writes nothing: a stale
    /// snapshot is never grounds to clobber (ADR 0017 D5). With it, the
    /// displaced entry goes to the trash first and is journalled, so undo can
    /// tell the user where it went (D7).
    fn clear_dest(&mut self, step: &Step) -> Result<(), String> {
        if fs::symlink_metadata(&step.dest).is_err() {
            return Ok(());
        }
        if !step.overwrite {
            return Err(format!(
                "{} exists now although it did not when the plan was made, so nothing was written",
                step.dest.display()
            ));
        }
        self.trash.send(&step.dest)?;
        self.journal.steps.push(Undoable::Trashed {
            path: step.dest.clone(),
        });
        Ok(())
    }

    fn copy_step(&mut self, step: &Step) -> Result<(), String> {
        self.clear_dest(step)?;
        copy_tree(step.kind, &step.src, &step.dest, &step.nodes, self)?;
        // Journalled as soon as the root exists, whether or not the tree beneath
        // it finished. A half-copied directory is still something sucher made,
        // and undo has to be able to take it away again (ADR 0017 D8). Only the
        // root is recorded: undo trashes it whole, so listing every node would
        // be noise.
        self.journal.steps.push(Undoable::Created {
            path: step.dest.clone(),
        });
        Ok(())
    }

    /// A move that `fs::rename` refused because the two paths sit on different
    /// devices (ADR 0017 D6). The S3/GCS/rclone FUSE mounts the README
    /// advertises are always a different device from the local disk, so this is
    /// the common path rather than an exotic one.
    ///
    /// The source is removed by **trashing** it, never by unlinking it, and a
    /// trash that refuses fails the step with the copy left in place and says
    /// so. An incomplete copy stops before the source is touched at all: losing
    /// the original to a partial duplicate is the one outcome a move must never
    /// produce.
    fn transplant(&mut self, step: &Step) -> Result<(), String> {
        if !copy_tree(step.kind, &step.src, &step.dest, &step.nodes, self)? {
            return Err(format!(
                "{} did not copy whole, so the original was left in place",
                step.dest.display()
            ));
        }
        self.trash.send(&step.src).map_err(|e| {
            format!(
                "{} was copied to {} but the original could not be removed: {e}",
                step.src.display(),
                step.dest.display()
            )
        })
    }

    fn move_step(&mut self, step: &Step) -> Result<(), String> {
        self.clear_dest(step)?;
        match (self.rename)(&step.src, &step.dest) {
            // A rename moves the whole subtree in one syscall, so the step's
            // counts land in one go.
            Ok(()) => self.tick(&step.dest, step.items, step.bytes),
            Err(e) if is_cross_device(&e) => self.transplant(step)?,
            Err(e) => {
                return Err(format!(
                    "cannot move {} to {}: {e}",
                    step.src.display(),
                    step.dest.display()
                ))
            }
        }
        self.journal.steps.push(Undoable::Moved {
            from: step.src.clone(),
            to: step.dest.clone(),
        });
        Ok(())
    }

    /// A rename stays inside one directory, so ADR 0017 D6's cross-device
    /// fallback cannot arise and any error is a real one.
    fn rename_step(&mut self, step: &Step) -> Result<(), String> {
        self.clear_dest(step)?;
        (self.rename)(&step.src, &step.dest).map_err(|e| {
            format!(
                "cannot rename {} to {}: {e}",
                step.src.display(),
                step.dest.display()
            )
        })?;
        self.tick(&step.dest, step.items, step.bytes);
        self.journal.steps.push(Undoable::Moved {
            from: step.src.clone(),
            to: step.dest.clone(),
        });
        Ok(())
    }

    fn create_step(&mut self, step: &Step) -> Result<(), String> {
        self.clear_dest(step)?;
        match step.kind {
            NodeKind::Dir => fs::create_dir(&step.dest)
                .map_err(|e| format!("cannot create {}: {e}", step.dest.display()))?,
            // `create_new` fails rather than truncating, which is the same
            // refusal the planner already made for a name that is taken. Two
            // guards for one rule is deliberate here: the planner's snapshot can
            // be stale and this one cannot.
            _ => {
                File::create_new(&step.dest)
                    .map_err(|e| format!("cannot create {}: {e}", step.dest.display()))?;
            }
        }
        self.journal.steps.push(Undoable::Created {
            path: step.dest.clone(),
        });
        self.tick(&step.dest, 1, 0);
        Ok(())
    }

    /// The whole of delete (ADR 0017 D7). There is no unlink anywhere in this
    /// module, and the journal records only that the path went to the trash,
    /// because the system trash is the restore surface and not this process.
    fn trash_step(&mut self, step: &Step) -> Result<(), String> {
        self.trash.send(&step.src)?;
        self.journal.steps.push(Undoable::Trashed {
            path: step.src.clone(),
        });
        self.tick(&step.src, step.items, step.bytes);
        Ok(())
    }

    /// The inverse of [`Undoable::Moved`]: put `to` back at `from`.
    fn restore(&mut self, from: &Path, to: &Path) {
        // Something took the original name back while the operation was on
        // screen. Say so rather than clobbering it, which would make undo itself
        // the destructive act.
        if fs::symlink_metadata(from).is_ok() {
            let msg = format!(
                "{} exists again, so {} was left where it is",
                from.display(),
                to.display()
            );
            self.fail(from, msg);
            return;
        }
        match (self.rename)(to, from) {
            // One journal step reversed, and no bytes to claim: a rename moves a
            // whole subtree without reading one.
            Ok(()) => self.tick(from, 1, 0),
            // The forward move already fell back to a transplant for this exact
            // reason, so the way back has to as well.
            Err(e) if is_cross_device(&e) => self.carry_back(to, from),
            Err(e) => {
                let msg = format!(
                    "cannot put {} back to {}: {e}",
                    to.display(),
                    from.display()
                );
                self.fail(to, msg);
            }
        }
    }

    /// The inverse of [`Undoable::Created`]: take away what sucher itself brought
    /// into being, by trashing it rather than unlinking it (ADR 0017 D7).
    fn remove(&mut self, path: &Path) {
        let sent = self.trash.send(path);
        match sent {
            Ok(()) => self.tick(path, 1, 0),
            Err(msg) => self.fail(path, msg),
        }
    }

    /// Undo a cross-device move by running ADR 0017 D6's transplant backwards:
    /// enumerate what is at `to`, copy it to `from`, then trash `to`.
    ///
    /// Without this, undo would be unavailable on exactly the mounts D6 was
    /// written for. On an S3, GCS or rclone FUSE mount every move is a
    /// cross-device move, so the reverse `fs::rename` fails for the very reason
    /// the forward one did, and the user would be left with the file at `to` and
    /// the original in the trash. Undo is the safety net for the operation most
    /// likely to have been a mistake, so it cannot be missing from the main road.
    ///
    /// `collect` does the enumeration, the same stage the forward move used, so
    /// undo inherits its bounds and its refusal to follow symlinks rather than
    /// growing a second walker that could drift out of agreement with the first.
    fn carry_back(&mut self, to: &Path, from: &Path) {
        let source = match collect(std::slice::from_ref(&to.to_path_buf())) {
            Ok(found) => match found.sources.into_iter().next() {
                Some(source) => source,
                // `collect` reports a vanished path as missing rather than as an
                // error, so an empty source list means there is nothing left
                // there.
                None => {
                    let msg = format!(
                        "{} is no longer there, so there is nothing to put back",
                        to.display()
                    );
                    self.fail(to, msg);
                    return;
                }
            },
            Err(refusal) => {
                let msg = format!("cannot read {} to put it back: {refusal}", to.display());
                self.fail(to, msg);
                return;
            }
        };

        let carried = copy_tree(
            source.kind,
            to,
            from,
            &source.nodes,
            &mut CarryBack(&mut *self),
        );
        match carried {
            Ok(true) => {}
            // The same rule as the forward move, in the other direction: losing
            // the surviving copy to a partial duplicate is the one outcome undo
            // must never produce, so `to` is left exactly where it is. The
            // entries that did not land have already reported themselves through
            // the sink; this line says what their sum means.
            Ok(false) => {
                let msg = format!(
                    "{} did not copy back whole, so {} was left alone",
                    from.display(),
                    to.display()
                );
                self.fail(from, msg);
                return;
            }
            Err(msg) => {
                self.fail(from, msg);
                return;
            }
        }

        // The restore succeeded the moment the tree landed back at `from`. What
        // happens to the leftover copy cannot retract that, so it is counted here
        // and any trouble with the copy is reported separately below.
        self.tick(from, 1, 0);
        let removed = self.trash.send(to);
        if let Err(msg) = removed {
            let msg = format!(
                "{} was restored, but the copy left at {} could not be removed: {msg}",
                from.display(),
                to.display()
            );
            self.fail(to, msg);
        }
        // Deliberately not a note. The notes list means "only the system trash
        // can bring this back", which is a thing to tell the user about. The copy
        // trashed here is the duplicate the undo exists to remove, and pointing
        // the user at Finder to restore it would invite them to undo their own
        // undo.
    }
}

/// Where a tree copy reports what it did.
///
/// The forward copy has progress counters, a cancel flag and a failure list on a
/// running worker; undo's reverse transplant has none of that and only wants to
/// know what did not land. Both implement this, so [`copy_tree`] stays one piece
/// of code used in both directions. That matters beyond tidiness: a round trip
/// over a FUSE mount is only lossless if the way back recreates symlinks without
/// dereferencing them (ADR 0017 D5) exactly as the way out did, and sharing the
/// code is the only way to guarantee that stays true.
trait TreeSink {
    fn landed(&mut self, path: &Path, bytes: u64);
    fn failed(&mut self, path: &Path, msg: String);
    /// Only a live run has anything to cancel, so undo takes the default.
    fn cancelled(&self) -> bool {
        false
    }
}

impl TreeSink for Exec<'_> {
    fn landed(&mut self, path: &Path, bytes: u64) {
        self.tick(path, 1, bytes);
    }

    fn failed(&mut self, path: &Path, msg: String) {
        self.fail(path, msg);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// The sink undo's reverse transplant reports through.
///
/// It borrows the run rather than accumulating on its own, which is what puts a
/// carry back on the same footing as a forward copy: the progress line moves
/// while a large tree is coming home, and the cancel flag is honoured *inside*
/// the tree rather than only between journal steps. Both matter for the same
/// reason the transplant exists at all, since on a FUSE mount the tree being
/// carried back may be the whole operation and `Esc` has to be able to reach it.
///
/// `landed` deliberately does not count items. An undo's [`Report::items`] is
/// the number of journal steps it reversed, and folding one transplant's ten
/// thousand entries into it would report the labour instead of the outcome.
struct CarryBack<'a, 'e>(&'e mut Exec<'a>);

impl TreeSink for CarryBack<'_, '_> {
    fn landed(&mut self, path: &Path, bytes: u64) {
        self.0.tick(path, 0, bytes);
    }

    fn failed(&mut self, path: &Path, msg: String) {
        self.0.fail(path, msg);
    }

    fn cancelled(&self) -> bool {
        self.0.cancelled()
    }
}

/// Reproduce `src` at `dest`, root first and then every enumerated node.
///
/// The node order is load-bearing: `collect` produced parents before children,
/// so every `create_dir` below has somewhere to go. That is why the executor
/// replays a decided list instead of rediscovering the tree while mutating it.
///
/// `Err` means the root itself could not be made, which is the only failure that
/// stops the whole thing. `Ok(false)` means the root landed but something under
/// it did not: one unreadable file fails its own node through the sink and the
/// rest of the tree still arrives, which is a better answer than abandoning a
/// copy that is already half done. The caller decides what a partial tree is
/// worth, and for a move it is worth refusing to touch the original.
fn copy_tree(
    kind: NodeKind,
    src: &Path,
    dest: &Path,
    nodes: &[Node],
    sink: &mut dyn TreeSink,
) -> Result<bool, String> {
    let bytes = place(kind, src, dest)?;
    sink.landed(dest, bytes);
    if kind != NodeKind::Dir {
        return Ok(true);
    }
    let mut whole = true;
    for (done, node) in nodes.iter().enumerate() {
        // Checked between nodes as well as between steps, so cancelling during
        // one large directory takes effect within a file rather than at the end
        // of the tree. A partial tree left behind is named rather than left
        // silent (ADR 0009).
        if sink.cancelled() {
            let msg = format!(
                "cancelled after {done} of {} entries, so {} is incomplete",
                nodes.len(),
                dest.display()
            );
            sink.failed(dest, msg);
            return Ok(false);
        }
        let from = src.join(&node.rel);
        let to = dest.join(&node.rel);
        match place(node.kind, &from, &to) {
            Ok(bytes) => sink.landed(&to, bytes),
            Err(msg) => {
                whole = false;
                sink.failed(&to, msg);
            }
        }
    }
    Ok(whole)
}

/// Reproduce one entry at `dest`, returning the payload bytes written. The three
/// arms are the whole of what a copy does, and a step root and a nested node go
/// through the same code so a selected file and a file three levels down can
/// never be treated differently.
fn place(kind: NodeKind, src: &Path, dest: &Path) -> Result<u64, String> {
    match kind {
        NodeKind::File => copy_file(src, dest),
        NodeKind::Symlink => copy_symlink(src, dest).map(|()| 0),
        NodeKind::Dir => make_dir(src, dest).map(|()| 0),
    }
}

/// `fs::copy` carries the source's permission bits on unix, which is what keeps
/// a 0600 key file private after a paste.
fn copy_file(src: &Path, dest: &Path) -> Result<u64, String> {
    fs::copy(src, dest)
        .map_err(|e| format!("cannot copy {} to {}: {e}", src.display(), dest.display()))
}

/// A symlink is recreated as a symlink pointing at the same target, never
/// followed and never dereferenced into a copy of what it points at (ADR 0017
/// D5). Following one would let a copy escape the source tree, reopen the
/// symlink-loop problem `collect` closed, and turn a twenty byte link into a
/// duplicate of a gigabyte.
fn copy_symlink(src: &Path, dest: &Path) -> Result<(), String> {
    let target =
        fs::read_link(src).map_err(|e| format!("cannot read the link {}: {e}", src.display()))?;
    symlink_to(&target, src, dest)
}

#[cfg(unix)]
fn symlink_to(target: &Path, _src: &Path, dest: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, dest)
        .map_err(|e| format!("cannot create the link {}: {e}", dest.display()))
}

/// Windows has to know at creation time whether the link points at a directory,
/// and the only way to find out is to resolve the *source* link. A link whose
/// target has gone becomes a file link, which is what the shell does too.
#[cfg(windows)]
fn symlink_to(target: &Path, src: &Path, dest: &Path) -> Result<(), String> {
    let to_dir = fs::metadata(src).map(|m| m.is_dir()).unwrap_or(false);
    let made = if to_dir {
        std::os::windows::fs::symlink_dir(target, dest)
    } else {
        std::os::windows::fs::symlink_file(target, dest)
    };
    made.map_err(|e| format!("cannot create the link {}: {e}", dest.display()))
}

#[cfg(not(any(unix, windows)))]
fn symlink_to(_target: &Path, _src: &Path, dest: &Path) -> Result<(), String> {
    Err(format!(
        "this platform cannot recreate the symlink {}",
        dest.display()
    ))
}

/// `create_dir`, never `create_dir_all`: `collect` enumerated parents before
/// children, so every parent is already there by the time this runs, and a
/// missing one means the tree changed under us. That is a real error rather than
/// something to paper over.
///
/// The mode is carried across on a best-effort basis and its failure is
/// deliberately not a [`Failure`]. A directory that was 0700 at the source must
/// not arrive world-readable, but the FUSE mounts ADR 0017 D6 names often have
/// no modes to set at all, and the payload arrived intact either way.
fn make_dir(src: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    if let Ok(meta) = fs::metadata(src) {
        let _ = fs::set_permissions(dest, meta.permissions());
    }
    Ok(())
}

/// Whether a failed `fs::rename` failed *only* because the two paths live on
/// different devices (ADR 0017 D6).
///
/// The precision is the point. Treating every rename error as cross-device would
/// turn a permission refusal into a silent copy plus a trashed original, which
/// is exactly the kind of downgrade this module exists to refuse.
///
/// [`io::ErrorKind::CrossesDevices`] is stable on this toolchain, so the named
/// kind is the primary test. The raw comparison behind it is a second line of
/// defence for a platform whose errno the standard library has not mapped.
fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices || raw_cross_device(e)
}

#[cfg(any(unix, windows))]
fn raw_cross_device(e: &io::Error) -> bool {
    e.raw_os_error() == Some(CROSS_DEVICE)
}

#[cfg(not(any(unix, windows)))]
fn raw_cross_device(_e: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fileop::plan::{plan, Conflict, Op, PlanCtx, Source};
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// The trash seam, substituted. Records every path handed to it, together
    /// with the content that path still had at the moment of the call, which is
    /// what lets the overwrite test prove the displaced entry went to the trash
    /// *before* the new bytes were written.
    struct Recorder {
        seen: Mutex<Vec<(PathBuf, Option<String>)>>,
        /// Simulates a platform or filesystem with no trash (ADR 0017 D7).
        fail: bool,
        /// Tripped on the first call, so cancellation lands mid-run at a known
        /// point instead of racing a real thread.
        trip: Option<Arc<AtomicBool>>,
    }

    impl Recorder {
        fn new() -> Recorder {
            Recorder {
                seen: Mutex::new(Vec::new()),
                fail: false,
                trip: None,
            }
        }

        fn failing() -> Recorder {
            Recorder {
                fail: true,
                ..Recorder::new()
            }
        }

        fn tripping(flag: Arc<AtomicBool>) -> Recorder {
            Recorder {
                trip: Some(flag),
                ..Recorder::new()
            }
        }

        fn paths(&self) -> Vec<PathBuf> {
            self.seen
                .lock()
                .expect("recorder lock")
                .iter()
                .map(|(p, _)| p.clone())
                .collect()
        }

        fn contents(&self) -> Vec<Option<String>> {
            self.seen
                .lock()
                .expect("recorder lock")
                .iter()
                .map(|(_, c)| c.clone())
                .collect()
        }
    }

    impl Trash for Recorder {
        fn send(&self, path: &Path) -> Result<(), String> {
            let content = fs::read_to_string(path).ok();
            self.seen
                .lock()
                .expect("recorder lock")
                .push((path.to_path_buf(), content));
            if let Some(flag) = &self.trip {
                flag.store(true, Ordering::Relaxed);
            }
            if self.fail {
                return Err(format!("no trash for {}", path.display()));
            }
            Ok(())
        }
    }

    struct Driven {
        report: Report,
        progress: Vec<(usize, u64, PathBuf)>,
    }

    /// A rename that always reports the one error ADR 0017 D6 falls back on.
    /// Every path a test can build sits inside one temp directory, so the real
    /// `fs::rename` can never produce this and the fallback would otherwise go
    /// untested on the mounts where it is the normal path.
    fn always_cross_device(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::CrossesDevices))
    }

    /// A rename that fails for a reason that is emphatically not cross-device,
    /// to prove the fallback is not a catch-all.
    fn always_denied(_from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::from(io::ErrorKind::PermissionDenied))
    }

    /// Drain a finished worker's channel exactly as the UI loop would. Shared by
    /// both directions, because both directions send the same messages.
    fn drained(rx: Receiver<Msg>) -> Driven {
        let mut progress = Vec::new();
        let mut report = None;
        for msg in rx.try_iter() {
            match msg {
                Msg::Progress {
                    items,
                    bytes,
                    current,
                } => progress.push((items, bytes, current)),
                Msg::Done(r) => report = Some(r),
            }
        }
        Driven {
            report: report.expect("exactly one Done"),
            progress,
        }
    }

    /// Run a plan synchronously against a substituted trash, then drain the
    /// channel exactly as the UI loop would.
    fn drive_with(plan: Plan, trash: &dyn Trash, rename: Rename, cancel: &AtomicBool) -> Driven {
        let (tx, rx) = mpsc::channel();
        execute(plan, trash, rename, &tx, cancel);
        drop(tx);
        drained(rx)
    }

    fn drive(plan: Plan, trash: &dyn Trash, cancel: &AtomicBool) -> Driven {
        drive_with(plan, trash, os_rename, cancel)
    }

    fn run(plan: Plan, trash: &dyn Trash) -> Driven {
        drive(plan, trash, &AtomicBool::new(false))
    }

    /// The undo mirror of [`drive_with`], and the only way the tests reach an
    /// undo now that it runs on the very same worker as a forward operation.
    fn drive_undo(
        journal: Journal,
        trash: &dyn Trash,
        rename: Rename,
        cancel: &AtomicBool,
    ) -> Driven {
        let (tx, rx) = mpsc::channel();
        execute_undo(journal, trash, rename, &tx, cancel);
        drop(tx);
        drained(rx)
    }

    /// Undo with a chosen rename and nothing cancelling it.
    fn undone_with(journal: Journal, trash: &dyn Trash, rename: Rename) -> Driven {
        drive_undo(journal, trash, rename, &AtomicBool::new(false))
    }

    /// Undo against a substituted trash and the real rename, which is what every
    /// same-device case wants. Narrowed to the report, because these cases are
    /// about the outcome rather than about the stream that carried it.
    fn undone(journal: Journal, trash: &dyn Trash) -> Report {
        undone_with(journal, trash, os_rename).report
    }

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, body).expect("write");
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).expect("read back")
    }

    /// The destination directory's real listing, which is what the browser would
    /// hand the planner.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn sources(paths: &[PathBuf]) -> Vec<Source> {
        collect(paths).expect("collect").sources
    }

    fn resolve(op: Op, cwd: &Path, listing: &[String], policy: Conflict) -> Plan {
        plan(
            op,
            &PlanCtx {
                dest_listing: listing,
                cwd,
                missing: &[],
                policy,
            },
        )
        .expect("plan should resolve")
    }

    fn copy_plan(srcs: &[PathBuf], dest: &Path, listing: &[String], policy: Conflict) -> Plan {
        resolve(
            Op::Copy {
                sources: sources(srcs),
                dest: dest.to_path_buf(),
            },
            Path::new("/nowhere"),
            listing,
            policy,
        )
    }

    #[test]
    fn a_file_copy_lands_with_the_right_content() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src/a.txt");
        let dst = tmp.path().join("dst");
        write(&src, "hello");
        fs::create_dir_all(&dst).expect("dst");

        let rec = Recorder::new();
        let out = run(copy_plan(&[src], &dst, &[], Conflict::Rename), &rec);

        assert_eq!(read(&dst.join("a.txt")), "hello");
        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        // A forward run leaves `notes` empty, so nothing about it changed when
        // undo joined the same pipeline.
        assert!(out.report.notes.is_empty());
        assert_eq!(out.report.direction, Direction::Forward);
        assert_eq!(out.report.items, 1);
        assert_eq!(out.report.bytes, 5);
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Created {
                path: dst.join("a.txt")
            }]
        );
        // A final Progress always precedes Done, so the status line ends on the
        // real totals rather than whatever the throttle last let through.
        assert_eq!(out.progress.last().map(|p| (p.0, p.1)), Some((1, 5)));
        assert!(rec.paths().is_empty(), "a plain copy trashes nothing");
    }

    #[test]
    fn a_directory_copy_replays_the_whole_tree() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("src/tree");
        let dst = tmp.path().join("dst");
        write(&root.join("a.txt"), "aa");
        write(&root.join("sub/b.txt"), "bbb");
        write(&root.join("sub/deep/c.txt"), "cccc");
        fs::create_dir_all(&dst).expect("dst");

        let rec = Recorder::new();
        let out = run(copy_plan(&[root], &dst, &[], Conflict::Rename), &rec);

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert_eq!(read(&dst.join("tree/a.txt")), "aa");
        assert_eq!(read(&dst.join("tree/sub/b.txt")), "bbb");
        assert_eq!(read(&dst.join("tree/sub/deep/c.txt")), "cccc");
        // Root, a.txt, sub, sub/b.txt, sub/deep, sub/deep/c.txt.
        assert_eq!(out.report.items, 6);
        assert_eq!(out.report.bytes, 9);
        // Only the root is journalled: undo trashes the whole tree at once.
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Created {
                path: dst.join("tree")
            }]
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_symlink_is_recreated_as_a_link_and_not_dereferenced() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().expect("tempdir");
        let target = tmp.path().join("target.txt");
        let link = tmp.path().join("src/link");
        let dst = tmp.path().join("dst");
        write(&target, "secret");
        fs::create_dir_all(link.parent().expect("parent")).expect("src");
        fs::create_dir_all(&dst).expect("dst");
        symlink(&target, &link).expect("symlink");

        let rec = Recorder::new();
        let out = run(copy_plan(&[link], &dst, &[], Conflict::Rename), &rec);

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        let landed = dst.join("link");
        let meta = fs::symlink_metadata(&landed).expect("the link exists");
        assert!(meta.file_type().is_symlink(), "copied as a real file");
        assert_eq!(fs::read_link(&landed).expect("read_link"), target);
    }

    #[test]
    fn a_same_device_move_renames_in_place() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src/a.txt");
        let dst = tmp.path().join("dst");
        write(&src, "moved");
        fs::create_dir_all(&dst).expect("dst");

        let p = resolve(
            Op::Move {
                sources: sources(std::slice::from_ref(&src)),
                dest: dst.clone(),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        );
        let rec = Recorder::new();
        let out = run(p, &rec);

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert!(!src.exists(), "the source is gone");
        assert_eq!(read(&dst.join("a.txt")), "moved");
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Moved {
                from: src,
                to: dst.join("a.txt")
            }]
        );
        // A same-device move never touches the trash.
        assert!(rec.paths().is_empty());
    }

    #[test]
    fn a_destination_that_appeared_after_planning_fails_instead_of_clobbering() {
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("src/a.txt");
        let b = tmp.path().join("src/b.txt");
        let dst = tmp.path().join("dst");
        write(&a, "new a");
        write(&b, "new b");
        fs::create_dir_all(&dst).expect("dst");

        // Planned against an empty destination, then the world changed.
        let p = copy_plan(&[a, b], &dst, &[], Conflict::Rename);
        write(&dst.join("a.txt"), "someone else's a");

        let rec = Recorder::new();
        let out = run(p, &rec);

        assert_eq!(out.report.failures.len(), 1, "{:?}", out.report.failures);
        assert_eq!(out.report.failures[0].path, dst.join("a.txt"));
        assert_eq!(read(&dst.join("a.txt")), "someone else's a");
        // The run carried on with the step that was still safe.
        assert_eq!(read(&dst.join("b.txt")), "new b");
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Created {
                path: dst.join("b.txt")
            }]
        );
        assert!(rec.paths().is_empty(), "a stale snapshot never trashes");
    }

    #[test]
    fn an_overwrite_trashes_the_displaced_entry_before_writing() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src/a.txt");
        let dst = tmp.path().join("dst");
        write(&src, "fresh");
        write(&dst.join("a.txt"), "displaced");

        let p = copy_plan(&[src], &dst, &listing(&dst), Conflict::Overwrite);
        assert!(p.steps[0].overwrite, "the plan asked for an overwrite");

        let rec = Recorder::new();
        let out = run(p, &rec);

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert_eq!(rec.paths(), vec![dst.join("a.txt")]);
        // The displaced bytes were still on disk when the trash seam ran, which
        // is what "trashed first, written second" means.
        assert_eq!(rec.contents(), vec![Some("displaced".to_string())]);
        assert_eq!(read(&dst.join("a.txt")), "fresh");
        assert_eq!(
            out.report.journal.steps,
            vec![
                Undoable::Trashed {
                    path: dst.join("a.txt")
                },
                Undoable::Created {
                    path: dst.join("a.txt")
                },
            ]
        );
    }

    #[test]
    fn an_unavailable_trash_refuses_the_overwrite_rather_than_deleting() {
        let tmp = TempDir::new().expect("tempdir");
        let src = tmp.path().join("src/a.txt");
        let dst = tmp.path().join("dst");
        write(&src, "fresh");
        write(&dst.join("a.txt"), "displaced");

        let p = copy_plan(&[src], &dst, &listing(&dst), Conflict::Overwrite);
        let rec = Recorder::failing();
        let out = run(p, &rec);

        assert_eq!(out.report.failures.len(), 1, "{:?}", out.report.failures);
        // ADR 0017 D7: no fallback to unlink, and nothing written on top.
        assert_eq!(read(&dst.join("a.txt")), "displaced");
        assert!(out.report.journal.steps.is_empty());
    }

    #[test]
    fn create_makes_a_file_and_a_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let dst = tmp.path().to_path_buf();

        let rec = Recorder::new();
        let file = resolve(
            Op::Create {
                parent: dst.clone(),
                name: "notes.md".to_string(),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        );
        let out = run(file, &rec);
        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert!(dst.join("notes.md").is_file());
        assert_eq!(read(&dst.join("notes.md")), "");
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Created {
                path: dst.join("notes.md")
            }]
        );

        let dir = resolve(
            Op::Create {
                parent: dst.clone(),
                name: "sub/".to_string(),
            },
            Path::new("/nowhere"),
            &listing(&dst),
            Conflict::Rename,
        );
        let out = run(dir, &rec);
        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert!(dst.join("sub").is_dir());
    }

    #[test]
    fn a_trash_step_hands_every_source_to_the_seam() {
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        write(&a, "a");
        write(&b, "b");

        let p = resolve(
            Op::Trash {
                sources: sources(&[a.clone(), b.clone()]),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        );
        let rec = Recorder::new();
        let out = run(p, &rec);

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert_eq!(rec.paths(), vec![a.clone(), b.clone()]);
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Trashed { path: a }, Undoable::Trashed { path: b },]
        );
    }

    #[test]
    fn an_unavailable_trash_says_so_rather_than_deleting() {
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("a.txt");
        write(&a, "a");

        let p = resolve(
            Op::Trash {
                sources: sources(std::slice::from_ref(&a)),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        );
        let out = run(p, &Recorder::failing());

        assert_eq!(out.report.failures.len(), 1);
        assert!(a.exists(), "sucher never permanently deletes");
        assert!(out.report.journal.steps.is_empty());
    }

    #[test]
    fn cancellation_stops_partway_and_the_journal_holds_only_what_happened() {
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        let c = tmp.path().join("c.txt");
        write(&a, "a");
        write(&b, "b");
        write(&c, "c");

        let p = resolve(
            Op::Trash {
                sources: sources(&[a.clone(), b.clone(), c.clone()]),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let rec = Recorder::tripping(Arc::clone(&cancel));
        let out = drive(p, &rec, &cancel);

        assert_eq!(rec.paths(), vec![a.clone()], "stopped after the first step");
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Trashed { path: a }]
        );
        assert_eq!(out.report.items, 1);
    }

    #[test]
    fn undo_of_a_move_puts_it_back() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        write(&to, "moved");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::new();
        let report = undone(journal, &rec);

        assert_eq!(report.items, 1);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(read(&from), "moved");
        assert!(!to.exists());
    }

    #[test]
    fn undo_of_a_move_refuses_when_the_original_path_is_occupied_again() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        write(&to, "moved");
        write(&from, "someone else got here first");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let report = undone(journal, &Recorder::new());

        assert_eq!(report.items, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, from);
        assert_eq!(read(&from), "someone else got here first");
        assert_eq!(read(&to), "moved", "the moved file stays put");
    }

    #[test]
    fn undo_of_a_create_sends_it_to_the_trash() {
        let tmp = TempDir::new().expect("tempdir");
        let made = tmp.path().join("copied");
        write(&made.join("inside.txt"), "a file the user added later");

        let journal = Journal {
            kind: Kind::Copy,
            steps: vec![Undoable::Created { path: made.clone() }],
        };
        let rec = Recorder::new();
        let report = undone(journal, &rec);

        assert_eq!(report.items, 1);
        // Trashed, never recursively unlinked: the tree may have gained files.
        assert_eq!(rec.paths(), vec![made]);
        assert!(report.notes.is_empty(), "nothing here is trash-only");
    }

    #[test]
    fn undo_of_a_trash_reports_it_as_trash_only_rather_than_pretending() {
        let tmp = TempDir::new().expect("tempdir");
        let gone = tmp.path().join("gone.txt");

        let journal = Journal {
            kind: Kind::Trash,
            steps: vec![Undoable::Trashed { path: gone.clone() }],
        };
        let rec = Recorder::new();
        let report = undone(journal, &rec);

        // ADR 0017 D8: the system trash is the restore surface, and undo says so
        // instead of claiming an undo it cannot perform.
        assert_eq!(report.items, 0);
        // A pointer to Finder is advice, not something that went wrong, so it
        // travels as a note and the failure list stays clean.
        assert!(report.failures.is_empty());
        assert_eq!(report.notes.len(), 1, "{:?}", report.notes);
        assert!(
            report.notes[0].contains(&gone.display().to_string())
                && report.notes[0].contains("system trash"),
            "the note neither names the path nor says where it comes back from: {}",
            report.notes[0]
        );
        assert!(rec.paths().is_empty());
    }

    #[test]
    fn undo_replays_the_inverses_in_reverse_order() {
        let tmp = TempDir::new().expect("tempdir");
        let displaced = tmp.path().join("dst/a.txt");
        let created = tmp.path().join("dst/a.txt");
        write(&created, "fresh");

        let journal = Journal {
            kind: Kind::Copy,
            steps: vec![
                Undoable::Trashed {
                    path: displaced.clone(),
                },
                Undoable::Created {
                    path: created.clone(),
                },
            ],
        };
        let rec = Recorder::new();
        let report = undone(journal, &rec);

        // The copy is trashed first, and only then is the displaced entry
        // reported as trash-only. Reverse order is what makes that true.
        assert_eq!(rec.paths(), vec![created]);
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].contains(&displaced.display().to_string()));
        assert_eq!(report.items, 1);
    }

    #[test]
    #[cfg(unix)]
    fn cross_device_is_detected_precisely_and_nothing_else_is() {
        // EXDEV is 18 on Linux and macOS alike.
        assert!(is_cross_device(&io::Error::from_raw_os_error(18)));
        assert!(is_cross_device(&io::Error::from(
            io::ErrorKind::CrossesDevices
        )));
        // A permission error must surface as a failure, never quietly become a
        // copy: EACCES is 13, EPERM is 1.
        assert!(!is_cross_device(&io::Error::from_raw_os_error(13)));
        assert!(!is_cross_device(&io::Error::from_raw_os_error(1)));
        assert!(!is_cross_device(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    /// Build a small tree at `root` and return the plan that moves it into
    /// `dest_dir`, which is what both cross-device move tests need.
    fn move_tree_plan(root: &Path, dest_dir: &Path) -> Plan {
        write(&root.join("a.txt"), "aa");
        write(&root.join("sub/b.txt"), "bbb");
        fs::create_dir_all(dest_dir).expect("dst");
        resolve(
            Op::Move {
                sources: sources(std::slice::from_ref(&root.to_path_buf())),
                dest: dest_dir.to_path_buf(),
            },
            Path::new("/nowhere"),
            &[],
            Conflict::Rename,
        )
    }

    #[test]
    fn a_cross_device_move_copies_the_tree_then_trashes_the_source() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("src/tree");
        let dest_dir = tmp.path().join("dst");
        let p = move_tree_plan(&root, &dest_dir);
        let dest = dest_dir.join("tree");

        let rec = Recorder::new();
        let out = drive_with(p, &rec, always_cross_device, &AtomicBool::new(false));

        assert!(out.report.failures.is_empty(), "{:?}", out.report.failures);
        assert_eq!(read(&dest.join("a.txt")), "aa");
        assert_eq!(read(&dest.join("sub/b.txt")), "bbb");
        // ADR 0017 D7 reaches the move fallback too: the source is trashed, not
        // unlinked.
        assert_eq!(rec.paths(), vec![root.clone()]);
        assert_eq!(
            out.report.journal.steps,
            vec![Undoable::Moved {
                from: root,
                to: dest
            }]
        );
    }

    /// Make `dir` read-only and report whether that actually took effect. A test
    /// runner with root ignores the mode, and faking a pass there would be worse
    /// than skipping.
    #[cfg(unix)]
    fn seal(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).expect("chmod");
        fs::File::create(dir.join(".probe")).is_err()
    }

    #[cfg(unix)]
    fn unseal(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    }

    #[test]
    #[cfg(unix)]
    fn an_incomplete_cross_device_move_leaves_the_original_alone() {
        let tmp = TempDir::new().expect("tempdir");
        let root = tmp.path().join("src/tree");
        let dest_dir = tmp.path().join("dst");
        let p = move_tree_plan(&root, &dest_dir);

        // A read-only source directory is reproduced read-only at the
        // destination, so its root lands and its children cannot.
        if !seal(&root) {
            unseal(&root);
            return;
        }
        let rec = Recorder::new();
        let out = drive_with(p, &rec, always_cross_device, &AtomicBool::new(false));
        unseal(&root);

        // Losing the original to a partial duplicate is the one outcome a move
        // must never produce, so the source was never handed to the trash.
        assert!(rec.paths().is_empty(), "the original was trashed anyway");
        assert!(root.join("a.txt").exists(), "the original is still there");
        assert!(!out.report.failures.is_empty());
        assert!(out.report.journal.steps.is_empty());
    }

    #[test]
    fn undo_of_a_cross_device_move_carries_the_tree_back_and_trashes_the_copy() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/tree");
        let to = tmp.path().join("dst/tree");
        write(&to.join("a.txt"), "aa");
        write(&to.join("sub/b.txt"), "bbb");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");
        #[cfg(unix)]
        std::os::unix::fs::symlink(Path::new("a.txt"), to.join("link")).expect("symlink");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::new();
        let out = undone_with(journal, &rec, always_cross_device);
        let report = &out.report;

        // ADR 0017 D6 runs backwards: the reverse rename cannot work for exactly
        // the reason the forward one could not, so undo transplants instead.
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        // One journal step reversed, not one per entry carried home: the tree is
        // the labour, the move is the outcome.
        assert_eq!(report.items, 1);
        // The entries came home through the progress stream on the way, so a
        // long carry back is watchable rather than a frozen frame.
        assert_eq!(report.bytes, 5);
        assert_eq!(read(&from.join("a.txt")), "aa");
        assert_eq!(read(&from.join("sub/b.txt")), "bbb");
        // The round trip is lossless, links included (ADR 0017 D5).
        #[cfg(unix)]
        {
            let back = from.join("link");
            assert!(
                fs::symlink_metadata(&back)
                    .expect("the link came back")
                    .file_type()
                    .is_symlink(),
                "the link was dereferenced on the way back"
            );
            assert_eq!(fs::read_link(&back).expect("read_link"), Path::new("a.txt"));
        }
        // The copy left behind goes to the trash, never to unlink.
        assert_eq!(rec.paths(), vec![to]);
        // Nothing here is trash-only: the user has their tree back at `from`,
        // and pointing them at Finder for the duplicate would mislead.
        assert!(report.notes.is_empty());
    }

    #[test]
    fn undo_of_a_cross_device_move_refuses_when_the_original_path_is_occupied_again() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/tree");
        let to = tmp.path().join("dst/tree");
        write(&to.join("a.txt"), "aa");
        write(&from.join("a.txt"), "someone else got here first");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::new();
        let report = undone_with(journal, &rec, always_cross_device).report;

        // The occupancy guard runs before the rename is even attempted, so the
        // fallback cannot be a way around it.
        assert_eq!(report.items, 0);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, from);
        assert_eq!(read(&from.join("a.txt")), "someone else got here first");
        assert_eq!(read(&to.join("a.txt")), "aa");
        assert!(rec.paths().is_empty(), "nothing was trashed");
    }

    #[test]
    #[cfg(unix)]
    fn an_incomplete_carry_back_leaves_the_copy_alone_and_says_so() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/tree");
        let to = tmp.path().join("dst/tree");
        write(&to.join("a.txt"), "aa");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        // Same trick as the forward case: a read-only source is reproduced
        // read-only, so the root lands and its contents cannot.
        if !seal(&to) {
            unseal(&to);
            return;
        }
        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::new();
        let report = undone_with(journal, &rec, always_cross_device).report;
        unseal(&to);
        unseal(&from);

        assert_eq!(report.items, 0, "an incomplete restore is not a restore");
        assert!(
            report
                .failures
                .iter()
                .any(|f| f.msg.contains("did not copy back whole")),
            "the incompleteness is not named: {:?}",
            report.failures
        );
        // The surviving copy is untouched: undo must never trade it for a
        // partial duplicate.
        assert!(rec.paths().is_empty(), "the copy was trashed anyway");
        assert_eq!(read(&to.join("a.txt")), "aa");
    }

    #[test]
    fn a_carry_back_whose_trash_refuses_still_counts_as_restored() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        write(&to, "moved");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::failing();
        let report = undone_with(journal, &rec, always_cross_device).report;

        // The user's file is back where it belongs, so the restore succeeded.
        // The leftover duplicate is reported rather than counted against it.
        assert_eq!(report.items, 1);
        assert_eq!(read(&from), "moved");
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, to);
        assert!(report.failures[0].msg.contains("restored"));
    }

    #[test]
    fn a_rename_error_that_is_not_cross_device_stays_a_plain_failure() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        write(&to, "moved");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let rec = Recorder::new();
        let report = undone_with(journal, &rec, always_denied).report;

        // The fallback is not a catch-all: a permission refusal must surface as
        // itself, not turn into a copy plus a trashed original.
        assert_eq!(report.items, 0);
        assert_eq!(report.failures.len(), 1);
        assert!(!from.exists(), "nothing was copied back");
        assert_eq!(read(&to), "moved");
        assert!(rec.paths().is_empty());
    }

    /// Poll a live [`Run`] the way the browser's pump does, until its one
    /// [`Msg::Done`] arrives. Bounded, so a worker that never finishes fails the
    /// test rather than hanging the suite.
    fn awaited(run: Run) -> Report {
        for _ in 0..2_000 {
            for msg in run.drain() {
                if let Msg::Done(report) = msg {
                    return report;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the worker never sent Done");
    }

    #[test]
    fn start_undo_streams_a_done_that_names_what_it_reversed() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        write(&to, "moved");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        // A `Moved` step only: this is the one test that goes through the real
        // [`OsTrash`], and no test may put a fixture in the developer's Finder
        // trash, so nothing here is allowed to reach the trash seam.
        let journal = Journal {
            kind: Kind::Move,
            steps: vec![Undoable::Moved {
                from: from.clone(),
                to: to.clone(),
            }],
        };
        let report = awaited(start_undo(journal));

        // One pipeline for every mutation: the same channel, the same single
        // `Done`, the same `Report` the browser already folds after a paste.
        assert_eq!(report.direction, Direction::Undo);
        // The kind is the operation that was reversed, which is what lets the
        // status line say "undid the move" rather than "moved".
        assert_eq!(report.kind, Kind::Move);
        assert_eq!(report.items, 1, "one journal step was put back");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(report.notes.is_empty());
        assert_eq!(read(&from), "moved");
        assert!(!to.exists());
    }

    #[test]
    fn an_undo_reports_an_empty_journal_so_u_cannot_stack_undos() {
        let tmp = TempDir::new().expect("tempdir");
        let from = tmp.path().join("src/a.txt");
        let to = tmp.path().join("dst/a.txt");
        let made = tmp.path().join("dst/made.txt");
        let gone = tmp.path().join("dst/gone.txt");
        write(&to, "moved");
        write(&made, "created");
        fs::create_dir_all(from.parent().expect("parent")).expect("src");

        let journal = Journal {
            kind: Kind::Move,
            steps: vec![
                Undoable::Moved {
                    from: from.clone(),
                    to: to.clone(),
                },
                Undoable::Created { path: made },
                Undoable::Trashed { path: gone },
            ],
        };
        let rec = Recorder::new();
        let report = undone(journal, &rec);

        // Every arm of the undo did something, and not one of them recorded a
        // step. ADR 0017 D8: an undo produces nothing to undo again, and the
        // empty journal is what `dir.rs` declines to push onto the stack.
        assert_eq!(report.items, 2);
        assert!(
            report.journal.steps.is_empty(),
            "an undo left something for `U` to undo: {:?}",
            report.journal.steps
        );
        // The journal still carries the kind, because an empty journal is still
        // a journal of a move.
        assert_eq!(report.journal.kind, Kind::Move);
    }

    #[test]
    fn a_trash_only_path_arrives_as_a_note_and_never_joins_the_failures() {
        let tmp = TempDir::new().expect("tempdir");
        let made = tmp.path().join("copied.txt");
        let gone = tmp.path().join("gone.txt");
        write(&made, "created");

        let journal = Journal {
            kind: Kind::Copy,
            steps: vec![
                Undoable::Trashed { path: gone.clone() },
                Undoable::Created { path: made.clone() },
            ],
        };
        // The trash refuses, so this undo has a genuine failure to report as
        // well as a note. Keeping both in one run is the point: the note must
        // not blend into the red list beside it.
        let report = undone(journal, &Recorder::failing());

        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].path, made);
        assert_eq!(report.notes.len(), 1, "{:?}", report.notes);
        assert!(report.notes[0].contains(&gone.display().to_string()));
        assert!(
            !report
                .failures
                .iter()
                .any(|f| f.msg.contains(&gone.display().to_string())),
            "the trash-only path leaked into the failures: {:?}",
            report.failures
        );
        // Nothing was reversed: the one reversible step could not be taken.
        assert_eq!(report.items, 0);
    }

    #[test]
    fn cancelling_an_undo_stops_it_and_reports_what_it_managed() {
        let tmp = TempDir::new().expect("tempdir");
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        let c = tmp.path().join("c.txt");
        write(&a, "a");
        write(&b, "b");
        write(&c, "c");

        let journal = Journal {
            kind: Kind::Copy,
            steps: vec![
                Undoable::Created { path: a.clone() },
                Undoable::Created { path: b.clone() },
                Undoable::Created { path: c.clone() },
            ],
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let rec = Recorder::tripping(Arc::clone(&cancel));
        let out = drive_undo(journal, &rec, os_rename, &cancel);

        // Newest first, so `c` is the one that got away before `Esc` landed.
        assert_eq!(rec.paths(), vec![c], "stopped after the first step");
        assert_eq!(out.report.items, 1, "an undo reports what it managed");
        assert!(a.exists() && b.exists(), "the rest was left alone");
        assert!(out.report.journal.steps.is_empty());
        // A final unthrottled Progress still precedes Done, so a cancelled undo
        // leaves the status line on its real total rather than mid-count.
        assert_eq!(out.progress.last().map(|p| p.0), Some(1));
    }
}
