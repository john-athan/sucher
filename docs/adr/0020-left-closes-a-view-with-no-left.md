# ADR 0020: Left closes a full view that has no left

Status: **Accepted, 2026-08-24**

## Context

Every full view is reached the same way: the browser hands a file to a viewer,
the viewer takes the screen, and `Esc` or `q` gives the screen back. That return
gesture is the most-used key in the app, and it is the one the hand reaches for
without looking.

The arrow keys do not agree with each other about what Left means. Across the
viewers it currently means five different things:

| Viewer | Left today |
|---|---|
| `dir` (browser) | go to the parent directory |
| `pdf` | previous page |
| `video` | seek back 5 s |
| `sheet` | previous column |
| `archive` | up one level in the archive tree |
| `text` | pan the content 4 columns left |
| `hex`, `svg`, `imgview`, `tui` (markdown) | nothing at all |

The last row is the problem. In those views Left is a dead key, and in `text` it
is dead too whenever the file has no line long enough to pan. The user presses
Left expecting to back out, the way Left backs out of the browser and of most
column and drill-down interfaces, and nothing happens. The fix cannot simply be
"Left always closes", because in the rows above it Left already carries a real
motion that people rely on.

## Decision

**D1: Left closes the view exactly where Left is not already a motion.** The
rule is one sentence and it is content-aware rather than viewer-aware: if there
is somewhere to go left, go there; if there is not, leave. Where a viewer has no
horizontal axis at all (`hex` is fixed-width, `svg` and `tui` scroll only
vertically, `imgview` never pans), Left joins `Esc` and `q` unconditionally.
In `text` the answer depends on the file in front of you: Left pans while the
longest line overflows the viewport, and closes once it does not. The same
keypress reads as "back" precisely when it can mean nothing else.

*Rejected, bind Left to close everywhere:* it would cost a page turn in `pdf`, a
seek in `video`, a column in `sheet`, and a level in `archive`, all of them
gestures with an established meaning and a real cost when they misfire.

*Rejected, leave the dead keys dead:* a key that does nothing is not neutral.
The reflex it fails to answer is the one the app most wants to be cheap.

**D2: the letter keys keep their vim reading, only the arrow closes.** `h` stays
pure motion wherever it exists, so a vim user panning with `h` never exits by
holding it, and ADR 0002's "explicit bindings win" posture is untouched. The
arrow is the navigational gesture, the letter is the motion gesture, and that is
the line between them.

**D3: the status bar says so.** The affected viewers now read `[←/q] back`
rather than `[q] quit`, which is both the truth and the discovery path. A
binding nobody can see is a binding nobody uses.

## Consequences

- Backing out of a Dockerfile, a hex dump, an SVG, an image, or a markdown
  document is now the same gesture as backing out of a directory.
- `text` is the only viewer where one key has two readings, and the reading is
  decided by the content rather than by a mode. A file with one very long line
  keeps Left as pan for as long as that line is on screen.
- The viewers with real Left motions are unchanged, so no muscle memory is spent.
