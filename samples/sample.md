# Sucher

Markdown with **real** typography in the terminal — now an interactive reader.

## Why

Classic renderers cannot make headers *bigger* and are not interactive. Sucher
uses the kitty text-sizing protocol in its pipe output, and a full-screen TUI
for navigation. This paragraph is intentionally long so that word wrapping and
line-by-line scrolling have something real to operate on when you resize the
window or page through the document with the keyboard.

## Navigation

Everything is driven from the keyboard. The status bar at the bottom always
shows the current scroll percentage and the key hints.

### Scrolling

- `j` / `k` or arrow keys move one line
- `d` / `u` jump half a page
- `g` / `G` go to the very top or bottom

### Table of contents

Press `t` to open the contents sidebar. Move with `j` / `k`, press Enter to
jump straight to that heading. This is the fastest way around a long document.

### Search

Press `/`, type a query, hit Enter. Then `n` and `N` cycle forward and back
through every match. Matched lines are highlighted in the body.

## Links

Press `l` for the link picker, then Enter to open one in your browser:

- [kitty text-sizing protocol](https://sw.kovidgoyal.net/kitty/text-sizing-protocol/)
- [ghostty tracking issue #10333](https://github.com/ghostty-org/ghostty/issues/10333)
- [ratatui](https://ratatui.rs)

## Code

```rust
fn main() {
    let doc = Rendered::build(&src);
    tui::run(title, doc);
}
```

---

## Formatting

> Blockquotes get a gutter. Nested lists indent:
>
> - level one
>   - level two
>     - level three

| Feature | Status | Notes              |
|---------|--------|--------------------|
| Scroll  | done   | j/k, d/u, g/G      |
| Search  | done   | / then n/N         |
| Tables  | done   | box-drawing grid   |
| Excel   | next   | calamine grid view |

## Roadmap

Markdown view is the foundation. Next: Excel grids via `calamine`, then PDF
pages rasterized through the kitty graphics protocol. Editing comes after the
viewers are solid.

Done.
