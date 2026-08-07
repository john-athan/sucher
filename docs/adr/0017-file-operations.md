# ADR 0017: File operations in the browser (multi-select, copy/move/rename/create/trash)

Status: proposed (decided; implementation is follow-up work)
Date: 2026-08-07

## Context

Sucher is a *viewfinder*: it frames files, it does not change them. Every ADR so
far has held that line. Archives are listed but never extracted (ADR 0001), data
files are attached `READ_ONLY` (ADR 0016), and the one escape hatch to the
outside world hands a path to the OS opener and waits for nothing (ADR 0014).

But the directory browser is now the primary way people reach files: a Miller
navigator with git gutter, sort modes, a smart filter, a recursive streaming
search, and a live preview of everything under the cursor. Once a user has
*found* the four PDFs they were looking for, the browser's answer to "now put
them somewhere" is to quit and retype the paths in a shell. The find half is
excellent and the act half is missing, so the tool gets abandoned mid-task.

The question this ADR answers is not "should sucher become a file manager" but
**how much mutation can be added without the viewfinder identity dissolving**,
and where the boundary is drawn so that it holds under later pressure.

## Decision

### D1: Operations act on *paths*, never on file *contents*

The boundary is the file as an opaque object. Sucher will mark, copy, move,
rename, create, and trash **whole paths**. It will never edit bytes, extract an
archive member, write a spreadsheet cell, or run a SQL `INSERT`.

This is what keeps the change cheap and reviewable:

- No decoder, viewer, or `Book` backend is touched. All twenty-odd rendering
  paths stay byte-for-byte read-only.
- The archive read-only rule (ADR 0001) and the data-file `READ_ONLY` attach
  (ADR 0016) are unaffected: they are *content* rules, and this ADR is a *path*
  feature.
- The blast radius of a bug is "a file is in the wrong folder", never "a file's
  bytes are corrupted".

The feature is **browser-only, browse-mode-only, TTY-only**. Pipe mode
(`dir::dump`) gains nothing, so a non-interactive sucher still cannot mutate
anything and stays safe in a script.

### D2: Two new modules; `dir.rs` only wires them

Following the flat-module convention:

- **`marks.rs`**: the multi-select set. An ordered set of absolute `PathBuf`
  with `toggle` / `invert` / `all` / `clear` / `contains` / `len` / `total`.
  Pure, no filesystem, fully unit-testable.
- **`fileop.rs`**: the operation engine, split along the project's usual
  pure/impure seam.
  - `plan(op, dest_listing) -> Result<Plan, Refusal>` is **pure**. It takes the
    intended operation and a listing of the destination, and returns either a
    fully resolved `Plan` (every source mapped to its final destination name,
    plus item and byte counts) or a `Refusal` naming exactly why. No filesystem,
    no clock, so the whole conflict/refusal matrix is unit-tested without a temp
    directory.
  - `execute(plan) -> Handle` runs on a background thread and streams
    `Msg::{Progress, Done, Failed}` over an `mpsc`, drained by a `pump_fileop()`
    sitting beside `pump_search` and `pump_raster` in `main_loop`, on the same
    60 ms poll tier. A multi-gigabyte copy therefore never blocks the UI, and
    progress is visible rather than a frozen frame.
  - Exactly **one operation in flight**, mirroring the single live raster worker
    (`raster_pending`). A second request is refused with a "busy" status, not
    queued: queued mutation is impossible to reason about while the user is
    simultaneously navigating.

`dir.rs` gains a `Mode::Input(Prompt)` arm (rename / create, using the same text
buffer idiom as Filter and Search), a `Mode::Confirm(Plan)` arm, an overlay
renderer built on the existing `centered_rect` + `Clear` popup geometry, a
one-column mark gutter, and status text. Nothing more.

*Note:* this pushes `dir.rs` past ~4200 lines and makes ADR 0012 (decompose the
browser `App`) materially more urgent. It is **not** a prerequisite, since the
new state is already externalised into `Marks` and `FileOp`, which is the
direction 0012 points, but 0012 should be scheduled straight after.

### D3: Marks are global and survive navigation

The mark set is keyed by absolute path and is **not** cleared on a directory
change. Marking three files here, two in a sibling folder, and pasting once is
the entire reason multi-select beats operating on the cursor; a set that resets
on every `h`/`l` would be a worse cursor.

Consequences, accepted deliberately:

- The mark gutter is drawn **only when the set is non-empty**, so a browser with
  no marks renders byte-for-byte as before. This is the same "invisible until
  used" rule the git gutter follows.
- The status line carries `N marked · <size>` so a set held across folders is
  never invisible state.
- Paths that vanished between marking and acting are **pruned at plan time and
  reported**, never silently dropped.

Ops act on the mark set when it is non-empty, otherwise on the entry under the
cursor. Rename is the exception: it requires exactly one target and says so
plainly when several are marked, rather than guessing a bulk-rename intent.

### D4: vim/yazi lowercase bindings, through `browse_char`

Every binding is registered in `browse_char`, the single source of truth (ADR
0002 D2), so typeahead's "is this char bound?" test stays automatically correct.

| key | operation |
| --- | --- |
| `Space` | toggle mark, advance |
| `V` / `Ctrl-a` | invert marks / mark all in view |
| `Esc` | clear marks if any, else quit |
| `y` | copy selection to the clipboard |
| `X` | cut selection to the clipboard |
| `p` | paste the clipboard into the current directory |
| `r` | rename (inline prompt, stem preselected) |
| `a` | create; a trailing `/` makes a directory |
| `D` | delete to trash (confirm overlay) |
| `U` | undo the last operation |
| `Y` | yank absolute path(s) to the system clipboard |

The cost is explicit: `y`, `p`, `r`, and `a` leave the type-to-select alphabet.
That is consistent rather than new, since `d u g l h o t x` were already spent on
motions, and `/` remains the strictly better way to find a name. The alternative
schemes (uppercase-only, or a `m` leader menu) preserve more of the alphabet but
trade away the muscle memory that makes these keys guessable for anyone arriving
from yazi, ranger, or vim.

`Esc` gains one guard: it clears marks when marks exist and otherwise quits as
before. A destructive-feeling key that silently quits with a live selection would
be worse than the small conditional.

`Y` uses **OSC 52** rather than a clipboard crate: no dependency, and it works
through ssh. That is the same reasoning that put the kitty graphics and
text-sizing protocols in the tool.

### D5: Nothing is overwritten silently, and the plan is shown before it runs

Name collisions are resolved by the planner into ` (2)`-style suffixes, and the
resulting `Plan` is rendered in the confirm overlay before a single byte moves:
what will be created, what was renamed to avoid a clash, how many items, how
large. Overwrite is reachable only by an explicit `o` toggle in that overlay,
which re-plans the whole batch and shows the result in the danger colour.

This is ADR 0009's principle ("an honest error, never a silent truncation")
applied to mutation: the user sees the outcome before authorising it.

The planner refuses outright, with a named reason, when asked to:

- move or copy a directory into its own descendant;
- rename to an empty name, `.`, `..`, or anything containing a path separator;
- operate on an ancestor of the current directory, or on `/`;
- recurse past a bounded depth or item count (an explicit refusal, never a
  silent partial copy).

Symlinks are copied **as links**, never followed, which closes symlink loops and
the "copy escapes the source tree" surprise in one rule.

### D6: Move falls back to copy+delete across devices

`fs::rename` is attempted first; on a cross-device error the operation degrades
to copy-then-remove-source. This is not optional polish. The S3/GCS/rclone FUSE
mounts the README advertises are *always* a different device from the local
disk, so a move between a mount and `~` is the common case, not the exotic one.

### D7: Delete means trash; sucher never permanently deletes

`D` moves to the OS trash (macOS Finder trash, XDG trash, Windows recycle bin)
via the `trash` crate. There is no permanent-delete binding at all.

Where trash is unavailable, on a remote mount or an unsupported filesystem, the
operation **fails honestly and does nothing** rather than falling back to
`unlink`. This is the same posture as "a `.db` that isn't actually SQLite errors
on open rather than falling back" (ADR 0016): a silent downgrade from
recoverable to unrecoverable is exactly the class of surprise that costs someone
their data. Users who genuinely want destruction have a shell.

Because delete is recoverable, the confirm overlay for `D` is informational
rather than a scare prompt: it lists what is going to the trash and needs one
key.

### D8: Undo is journal-backed, so it can never lie

`execute` records a `Journal` of the steps that **actually succeeded**. Undo
replays their inverses: a rename un-renames, a move moves back, a copy or a
create removes precisely the paths sucher itself created. A partially failed
operation therefore undoes exactly the part that happened.

The stack is bounded (depth 16). Trash is not undone in-process. The system
trash is the restore surface and the status line says so, because `trash`'s
restore API is not available on every platform and a half-supported undo is
worse than an honest pointer to Finder.

### D9: Not in search mode

Recursive search is a text-input surface where every printable key belongs to
the query (ADR 0007 D1). Ops are browse-mode only; the path from a search hit to
an operation is `Enter` into its folder, then operate. Splitting the key space
inside search would produce exactly the filter/search hybrid that D1 of that ADR
rejected.

## Consequences

- The viewfinder identity survives with a sharper edge than before: sucher
  changes *where files are*, never *what they contain*. That sentence is
  testable and belongs in the README.
- The conflict, refusal, and collision-naming matrix is pure and unit-tested with
  no filesystem. Only the thin executor needs a temp directory, which introduces
  `tempfile` as the project's first dev-dependency.
- One new runtime dependency (`trash`), subject to the `deny.toml` licence and
  advisory gates. It performs no network I/O, so the offline guarantee holds.
- `dir.rs` grows by roughly 300 lines of wiring and overlay rendering, and ADR
  0012 becomes the next architectural task rather than the lowest-urgency one.
- Deliberately out of scope, and to be recorded in Limitations: bulk rename via
  `$EDITOR`, permission changes, archive extraction, symlink creation on paste,
  and operations from within search mode.

## Alternatives considered

**Marks reset on directory change.** Simpler state and no stale-path handling,
but it reduces multi-select to a slower cursor and removes the one workflow,
gather from several folders and act once, that motivates the feature.

**Uppercase-only bindings**, to preserve the typeahead alphabet. Keeps `y p r a`
available for type-to-select, but sacrifices the guessability that makes these
keys learnable without the help overlay, and capitals still collide with real
names (`README`, `Cargo.toml`), so the win is partial.

**A `m` leader key opening a which-key ops menu.** Costs exactly one letter and
is maximally discoverable, but makes the two most frequent operations (`y`, `p`)
two keystrokes, which is precisely backwards.

**Permanent delete behind a second confirmation.** More capable, and it works on
filesystems without a trash. Rejected because it hands sucher, a tool people
point at directories they are only browsing, one irreversible action, and the
confirm dialog is a thin defence against muscle memory.

**Operations synchronous on the UI thread.** Simpler than a worker plus a message
pump, and correct for small files. Rejected because a directory copy is unbounded
work, and ADR 0005's split already established that unbounded work goes
off-thread behind visible progress while cheap work stays inline.
