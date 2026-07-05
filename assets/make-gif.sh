#!/usr/bin/env bash
# Convert a screen recording (.mov/.mp4) into an optimized GIF for the README.
#
# Why native capture (not vhs/asciinema): sucher draws real pixels via the
# kitty / iTerm2 / sixel graphics protocols. Browser-based recorders (vhs uses
# xterm.js in headless Chrome) can't render those, so they'd only capture the
# half-block fallback. Record in a graphics-capable terminal instead.
#
# Capture on macOS:  Cmd+Shift+5 → Record Selected Portion → run sucher → Stop.
# Then:              ./assets/make-gif.sh ~/Desktop/screen.mov assets/demo.gif
#
# Requires ffmpeg (already a sucher runtime dep).

set -euo pipefail

# With no input arg, use the newest macOS screen recording on the Desktop
# (default name "Screen Recording … .mov", which has spaces).
src=${1:-}
if [[ -z "$src" ]]; then
  src=$(ls -t "$HOME/Desktop/"*.mov 2>/dev/null | head -1 || true)
  [[ -n "$src" ]] || { echo "no .mov on Desktop — record with Cmd+Shift+5 first"; exit 1; }
  echo "using newest recording: $src"
fi
[[ -f "$src" ]] || { echo "not found: $src"; exit 1; }
out=${2:-assets/demo.gif}
fps=${3:-12}
width=${4:-1200}

palette=$(mktemp -t sucher-palette-XXXX).png
trap 'rm -f "$palette"' EXIT

# Two-pass: generate an optimized palette, then map the video to it.
ffmpeg -y -i "$src" -vf "fps=${fps},scale=${width}:-1:flags=lanczos,palettegen=stats_mode=diff" "$palette"
ffmpeg -y -i "$src" -i "$palette" \
  -lavfi "fps=${fps},scale=${width}:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3" \
  "$out"

echo "wrote $out ($(du -h "$out" | cut -f1))"
