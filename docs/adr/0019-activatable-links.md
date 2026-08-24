# ADR 0019: Activatable links in the markdown viewer

Status: **Accepted, 2026-08-24**

## Context

The markdown viewer (`src/tui.rs`) already collected every link into
`Rendered::links` and offered a picker (`l`) whose Enter called `open_url`.
But `open_url` is gated by `util::is_safe_url` (ADR 0009 / S5), which accepts
only `http`, `https`, and `mailto`. Every other link target silently did
nothing: a relative file link (`./notes.md`, `../src/main.rs`), an absolute
path, and an in-document anchor (`#install`) all looked clickable in the
picker and all no-opped on Enter. Nothing was clickable with the mouse at
all; `dir.rs` (the directory browser) had carried mouse support since ADR
0005, but `tui.rs` never gained the matching `MouseGuard`/`Event::Mouse` wiring.

Two things needed solving together: the document layout had no way to say
"screen column N on display line M is part of link K" (link identity was
lost the moment `Rendered::layout` word-wrapped text into `Line`s), and the
existing `is_safe_url` gate, correct for what it protects, is the wrong gate
for a local path.

## Decision

**D1: link identity survives layout, via a struct return, not a wider
tuple.** `Tok::Word(String, Style)` became a struct variant carrying
`link: Option<usize>`, the index into `Builder::links` (and later
`Rendered::links`) the word belongs to. Set once per `tokenize` call from
`self.link_url.is_some().then_some(self.links.len())`: the `LinkRef` for an
open link is not pushed until `Event::End(TagEnd::Link)`, so at tokenize time
`self.links.len()` is exactly the index that entry WILL get once it lands.
Inline code inside a link (`` [click `here`](url) ``) gets the same
treatment at its own `Event::Code` site. `Rendered::layout` used to return a
3-tuple (`display`, `plain`, `log2disp`); a fourth parallel vector for link
positions would have made every call site an unreadable
`let (a, b, c, d) = ...`, so it returns a named `Layout` struct instead, and
`hits: Vec<Vec<LinkHit>>` (one entry per display line, since a link can wrap
across a line break) sits beside the other three fields. `LinkHit { col,
width, link }` uses CHARACTER columns matching `Layout::plain`, not byte
offsets, so it composes with the rest of the module's char-counted wrapping.
Both the wrapping path (`wrap`) and the unwrapped path (`render_unwrapped`,
used for fenced code blocks and table rows, where a link can never appear)
now thread hits through; `wrap` merges adjacent same-link words, and the
gap between them, into one hit rather than one per word, by tracking a
single open run (`start`, `end`) and only closing it when the link changes,
disappears, or the display line itself is flushed by a wrap boundary. A link
that wraps mid-run therefore yields exactly one hit per display line it
touches, which is what a mouse click needs: it only ever tests one line.

**D2: the security boundary is which OPENER a target reaches, not whether it
is "safe" in the abstract.** `activate_link` (the one function both a mouse
click and the link picker's Enter call, so the two paths cannot diverge)
resolves a link target in a fixed order, factored into a pure decision
function `decide_link(url, base_dir) -> LinkDecision` so the logic is
testable without a terminal or the filesystem:

1. `util::is_safe_url` (`http`/`https`/`mailto`) still goes to the OS
   browser/mail opener exactly as before. Unchanged.
2. A `#fragment` is an in-document jump: never touches an opener at all.
3. Anything else carrying an explicit scheme (`^[A-Za-z][A-Za-z0-9+.-]*:`,
   matched by a small hand-written `has_scheme`, no regex dependency) is
   refused outright. This is what keeps `file://`, `javascript:`, and any
   custom scheme away from both openers.
4. Only a target with NO scheme prefix at all is treated as a path: percent-
   decoded, resolved against the SOURCE DOCUMENT's directory (`self.open`'s
   parent, or the process cwd for a document rendered from stdin) unless
   already absolute.

The point of step 3 existing at all is what makes step 4 safe: `file:///etc/passwd`
never reaches path resolution, because it has a scheme and is refused before
`decide_link` ever calls `Path::join`. A bare `./notes.md` has no scheme, so
it reaches step 4, resolves, and if it exists on disk is handed back as
`Some(path)` for the CALLER to open, not the pure function. `activate_link`
opens that path IN SUCHER's OWN VIEWER (`main::open_interactive`, the same
entry point `dir.rs` uses for Enter on a listing row), never through
`open`/`xdg-open`. That is the deliberate asymmetry with ADR 0014's
`open_in_native_app`: sucher only ever VIEWS a file it opens on itself, it
never executes it, so a relative link from an untrusted document is safe to
hand to sucher's own viewer in a way it would not be safe to hand to the OS's
default-application opener (which might be a program that macros, executes,
or otherwise acts on the file). `is_safe_url`'s allow-list stays exactly what
it was for the browser/mail case; this ADR does not loosen it by one scheme.
It adds a second, narrower door, open only to schemeless local paths, that
leads to sucher itself rather than to the OS.

*Not restricted: where the resolved path may point.* A link may carry `../`
segments or an absolute path, so a document can point at any file the user can
already read. That is deliberate. The target is only ever rendered on the
user's own screen, it takes a click or an Enter to reach, the full target is
visible in the link picker before activation, and sucher has no outbound
channel to leak what it shows. Sandboxing the viewer to the document's own
subtree would break the ordinary and useful case, a repo README linking
`../CONTRIBUTING.md`, while buying nothing against an attacker who by
construction cannot see the result.

**D3: percent-decoding is a dozen lines, not a dependency.** `percent_decode`
handles `%XX` escapes (reassembled as UTF-8, lossily on a malformed
sequence) and leaves anything else untouched. A markdown link target only
ever needs this common case; pulling in a crate for it would be a poor
trade for something this small and this easy to get exactly right for the
inputs that matter.

**D4: the anchor jump reuses the TOC, matched by a GitHub-shaped slug.**
`markdown::slug(title)` lowercases, turns whitespace into `-`, drops
anything that is neither alphanumeric nor `-`, and collapses repeated `-`.
`activate_link`'s anchor case slugs the link fragment and the title of every
`TocEntry`, and jumps to the first match via the same `log2disp` lookup the
TOC overlay's own Enter uses. No match is a silent no-op, matching how the
TOC overlay already behaves when nothing is selected.

**D5: mouse capture mirrors `dir::MouseGuard` exactly (ADR 0005 D2), because
the invariant it protects is the same invariant.** A second, independent
`MouseGuard` in `tui.rs` enables capture right after `ratatui::init()`,
gated on `config::mouse_enabled()`, and disables it on `Drop`, so a panic,
an early return, or the "open a resolved link in sucher" round trip can
never leave the terminal stuck in capture mode. `run` became a
`loop { ratatui::init() ... }` returning an `Action` (`Quit` / `Open(PathBuf)`),
the same shape `dir::run` already uses for exactly the same reason: opening a
file has to tear down this screen, hand off, and re-enter cleanly on return.
A left-click is mapped to a display line/column via `content_area`, a
`Rect` recorded each render (border and TOC-sidebar offsets already folded
in), looked up against that line's `hits`, and run through the same
`activate_link` the picker uses. The wheel scrolls three lines, matching
`dir.rs`'s selection-move granularity in spirit. Mouse input is ignored
outright while any overlay (search/TOC/links/help/gallery) is active, so a
click can never reach a link hidden underneath a popup.

**D6: a link that resolves to nothing says so.** `App::flash: Option<String>`
is a short-lived status-bar message, cleared at the top of every keypress
(so a stale message never lingers past the very next key) and set by
`activate_link` to `no such file: <url>` when a path does not exist on disk,
or `refused: unsupported link scheme` when step 3 above refuses a target.
Silently doing nothing here would have been the wrong failure mode: the
user just clicked something that looked actionable, and a no-op with no
feedback reads as sucher being broken rather than as the link being bad.

## Consequences

- A relative or absolute file link opens in sucher's own viewer; an anchor
  jumps in place; `http`/`https`/`mailto` still go to the OS opener exactly
  as before; every other scheme is refused, with the status bar saying so.
- `Rendered::layout`'s signature change (tuple to `Layout` struct) has one
  other caller in the tree, `dir.rs`'s markdown preview, which only ever
  needed the `display` field and was updated to destructure the new struct.
- The mouse-click path and the link-picker's Enter share one decision
  function (`decide_link`) and one effect function (`activate_link`), so
  they cannot drift apart the way two independently-maintained "open a
  link" implementations eventually would.
- `decide_link` is pure (no filesystem access, no terminal, no viewport
  mutation) specifically so the scheme/path/anchor/browser routing is unit
  tested directly, without driving a TTY; the impure existence check and the
  actual open/jump live only in `activate_link`, one layer up.
- The two "open something" boundaries in sucher now read as one coherent
  ladder: `is_safe_url` gates the OS browser/mail opener (ADR 0009 / S5,
  unchanged), `open_in_native_app` gates the OS default-application opener
  for a file the user themselves selected (ADR 0014, unchanged), and this
  ADR's path-resolution step gates sucher's OWN viewer for a file an
  untrusted document merely POINTS AT. Three doors, three distinct threat
  models, none of them reusing another's gate.
