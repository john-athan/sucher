# Third-party material in sucher

sucher is MIT licensed and every line of its source was written for this project.
This file records the material in the repository that originated elsewhere, so
the attribution obligations that come with it are visible rather than implied.

Dependency licenses are a separate list: the release binary links its Rust
dependencies statically, so their notices live in
[`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md), regenerated on every
release by `cargo about`. The permitted license set is enforced in CI by
[`deny.toml`](deny.toml).

## Color palettes

`src/theme.rs` ships four palettes whose color values are taken from published
themes. The role mapping (which color means "directory", which means "PDF") is
ours; the hex values are theirs.

| Palette | Upstream | License | Copyright |
| --- | --- | --- | --- |
| `catppuccin-mocha` | [catppuccin/catppuccin](https://github.com/catppuccin/catppuccin) | MIT | Copyright (c) 2021 Catppuccin |
| `gruvbox-dark` | [morhetz/gruvbox](https://github.com/morhetz/gruvbox) | MIT/X11 | Copyright (c) morhetz |
| `tokyo-night` | [enkia/tokyo-night-vscode-theme](https://github.com/enkia/tokyo-night-vscode-theme) | MIT | Copyright (c) 2018-present Enkia |
| `sucher-light` | [tailwindlabs/tailwindcss](https://github.com/tailwindlabs/tailwindcss) default palette, 600 scale | MIT | Copyright (c) Tailwind Labs, Inc. |

The theme names are used to say which theme the colors come from, which is what
the names are for. No endorsement by the upstream projects is implied.

## Icon glyphs

`src/icons.rs` maps file extensions to Nerd Font code points in the Unicode
Private Use Area, the shared `nf-*` set from
[ryanoasis/nerd-fonts](https://github.com/ryanoasis/nerd-fonts) (MIT, Copyright
(c) 2014 Ryan L McIntyre). Only code points are referenced. No font file is
bundled or redistributed; rendering requires a patched font the user already has
installed.

Per-language accent colors in the same module are the brand colors of the
languages and tools they identify. They are used to identify the file type,
which is nominative use, and imply no affiliation.

## Spinner

The braille progress sequence in `src/dir.rs` (`SPINNER`) is the widely
reproduced `dots` frame set, first published in
[sindresorhus/cli-spinners](https://github.com/sindresorhus/cli-spinners) (MIT,
Copyright (c) Sindre Sorhus).

## Sample files

`samples/` contains small fixtures generated for this repository to exercise the
viewers. They contain no third-party content.

## Reviewed and cleared

Findings from [`scripts/provenance-check.py`](scripts/provenance-check.py) that
were investigated and closed. Kept so the reasoning survives longer than the
memory of it.

### `base64_encode` in `src/util.rs` (reviewed 2026-08-10)

A code search for the line `let sextet = |shift: u32| ALPHABET[...]` matches
`src/base64.rs` in [Madadog/Egg-Game](https://github.com/Madadog/Egg-Game),
which is GPL-3.0. The two functions share the closure name `sextet`, the
`div_ceil` capacity calculation, the push order and the shape of the doc
comment. The upstream file predates ours (2026-07-04 against 2026-08-07).

Assessed as convergent output, not copying:

- The function implements RFC 4648, whose alphabet and 24-bit grouping are
  specified by the standard. The set of reasonable Rust spellings is small.
- The differences are in exactly the places where an author has a free choice
  (`& 63` against `& 0x3f`, inline packing against a temporary array), which is
  not the pattern a copy leaves.
- Both files carry the register of an assistant-written explanation, which is
  the more economical explanation for the resemblance than either author having
  read the other's repository.

No dependency, no attribution obligation, and nothing was changed. Recorded
because the next reader deserves the answer without redoing the work.
