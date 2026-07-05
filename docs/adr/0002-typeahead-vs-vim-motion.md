# ADR 0002 — Type-to-select vs. vim motion in the directory browser

Status: **Accepted — 2026-07-04**

Inherited from this project's predecessor (its ADR 0003), adapted to the ratatui
directory browser here.

## Context

The browser's most reflexive navigation would be **type-to-select**: type a few letters
and the cursor jumps to the first entry whose name starts with them (`Rea` → `README`).
The browser has none — the only name filter is the modal `/` fuzzy filter.

The conflict: the browser binds **single letters to vim motion** — `j`/`k` move,
`d`/`u` half-page, `g`/`G` top/bottom, `h`/`l` parent/open — plus `.`, `/`, `q`. Naïve
typeahead would route every printable key into a name buffer and clobber those motions.
The two behaviours must coexist on the same keys without either swallowing the other.

The browser also has a modal `/` filter mode; typeahead applies only in Browse
mode, never while the filter (a text-input surface) is focused — the same gating
used for its text surfaces.

## Decision

**D1 — A timed typeahead *session*, with explicit bindings winning only while idle.**
Typeahead keeps a buffer string plus the `Instant` of the last keystroke. A session is
*active* only while `now - last < TIMEOUT` (900 ms). On a printable key (a single
non-control char, no Ctrl/Alt) in Browse mode the precedence is:

- **Session active** → append the char and re-match. Once mid-type, *every* printable key
  — including a motion key like `j` or `g` — extends the prefix. The session ends on its
  own when the timeout lapses (or on Escape).
- **No active session** → if the key is bound to a browse action (`j`/`k`/`g`/…), let that
  action run (**vim motion preserved**); if the key is bound to nothing, **start** a
  session with it.

The rule is three pure, time-injected helpers — `active(now, last, timeout)`,
`action(active, key_is_bound) -> {Append, StartNew, PassThrough}`, and
`match_prefix(names, buffer) -> Option<usize>` (first case-insensitive `starts_with`,
order-preserving, `None` on empty buffer). They own no ratatui types and no clock, so the
precedence and timeout reset are unit-tested without a terminal or real waiting.

*Why explicit bindings win when idle but sessions capture-all when active:* the two
readings of a keypress — "a vim motion" vs. "a letter I'm spelling" — are disambiguated by
**intent over time**, not by a modifier. At rest, a lone `j` almost always means *move
down*, so the binding wins and muscle memory is intact. The instant you've typed a letter
you're spelling a name, so follow-on keys must all be letters. Anchoring "am I spelling?"
to a short recency window resolves this with no third gesture to learn.

*Rejected — drop vim nav (full typeahead):* vim motion is a deliberate strength of the
browser; trading it for typeahead is a wash at best. *Rejected — typeahead behind a
modifier / a dedicated key:* a third thing to learn and reach for, defeating the point of a
reflexive jump.

**D2 — One source of truth for "is this char bound".** `is_bound` must not be a second
hand-maintained list that can drift from the real key handler (drift is the very bug class
this codebase is being cleaned of — see ADR 0001). The browser's char-driven actions are
factored into one pure `browse_char(c) -> Option<CharAction>`; the real key handler
executes its result, and typeahead's `key_is_bound` is exactly `browse_char(c).is_some()`.
Add a browse binding in one place and typeahead stays correct automatically.

**D3 — Session-aware editing keys; silent no-op on miss.** While a session is active,
`Esc` cancels the session (clears the buffer) instead of quitting the browser, and
`Backspace` edits the buffer (pop last char, re-match) instead of going to the parent
directory; with no active session both keep their normal browse meaning (`Esc` quits,
`Backspace` goes up). On **no match** the buffer is kept and the cursor stays put (a typo
doesn't fling the cursor to an unrelated entry; the next char can still complete a valid
prefix) — a subtle no-op, consistent with the app's restraint. Typeahead state is transient
input state, not persisted.

## Consequences

- Vim motion, arrows, `Enter`, and the `/` filter are untouched; typeahead only ever
  consumes a printable key that is *either* mid-session *or* unbound.
- **The one cost:** from *idle*, a file whose name starts with a motion key's letter can't
  be reached by that first letter (e.g. `g` runs "top", not a jump to `git/`). It stays
  reachable by typing a different leading prefix, by continuing to type once any session is
  open, or via the `/` filter — the deliberate trade of keeping vim nav primary.
- The precedence rule and matcher are pure and unit-tested; the wiring is exercised by
  compilation, as with the rest of the browser.
