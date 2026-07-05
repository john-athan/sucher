# assets

Images referenced by the top-level `README.md`.

- `demo.gif` — short screen recording of the browser + viewers.
- `browser.png`, `markdown.png`, `video.png` — still screenshots (these were
  pulled straight from the recording with
  `ffmpeg -ss <t> -i <rec.mov> -frames:v 1 out.png`).

## Capture in a graphics-capable terminal

sucher draws **real pixels** via the kitty / iTerm2 / sixel graphics protocols.
Record in a terminal that supports one — **kitty, ghostty, WezTerm, iTerm2** —
so images, PDF pages, and video posters show as actual pixels.

> Don't use vhs/asciinema here: they render the terminal with xterm.js in a
> headless browser, which can't do the graphics protocols, so they'd only
> capture the half-block fallback.

### Stills

macOS: `Cmd+Shift+4`, drag over the terminal. Keep them ~1200px wide.

```sh
s .            # browser  -> browser.png
s samples/sample.md    # markdown -> markdown.png
s samples/sample.xlsx  # sheet    -> sheet.png
```

### GIF

1. `Cmd+Shift+5` → **Record Selected Portion** → run sucher → **Stop**
   (recording lands on the Desktop).
2. Convert it to an optimized GIF (uses ffmpeg):

```sh
./assets/make-gif.sh                       # uses newest Desktop recording
# or: ./assets/make-gif.sh <in.mov> <out.gif> <fps> <width>
```

## Social preview

`make-social.py` composes `social-preview.png` (1280x640) for the GitHub
social card. Upload it under **Settings → General → Social preview** (UI only;
no API). Regenerate: `python3 assets/make-social.py`.
