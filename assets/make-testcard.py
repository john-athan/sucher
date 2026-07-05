#!/usr/bin/env python3
"""Generate the sample graphics test card (1600x1000).

Written to samples/picture.jpg and samples/testcard.png — a colour/contrast
reference used to show off the image viewer's real-pixel rendering. Run from
the repo root:  python3 assets/make-testcard.py
"""
import math
from PIL import Image, ImageDraw, ImageFont

W, H = 1600, 1000
img = Image.new("RGB", (W, H), (10, 11, 15))
d = ImageDraw.Draw(img)


def font(paths, size):
    for p in paths:
        try:
            return ImageFont.truetype(p, size)
        except OSError:
            continue
    return ImageFont.load_default()


MONO = ["/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf"]
SANS = ["/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/Helvetica.ttc"]

# --- Top: SMPTE-style colour bars -------------------------------------------
BARS = [(192, 192, 192), (192, 192, 0), (0, 192, 192), (0, 192, 0),
        (192, 0, 192), (192, 0, 0), (0, 0, 192)]
bw = W / len(BARS)
for i, c in enumerate(BARS):
    d.rectangle([i * bw, 0, (i + 1) * bw, 150], fill=c)

# --- Rainbow gradient strip --------------------------------------------------
for x in range(W):
    h = x / W
    r = int(255 * max(0, min(1, abs(h * 6 - 3) - 1)))
    g = int(255 * max(0, min(1, 2 - abs(h * 6 - 2))))
    b = int(255 * max(0, min(1, 2 - abs(h * 6 - 4))))
    d.line([(x, 150), (x, 200)], fill=(r, g, b))

# --- Grayscale ramp ----------------------------------------------------------
steps = 20
sw = W / steps
for i in range(steps):
    v = int(255 * i / (steps - 1))
    d.rectangle([i * sw, 700, (i + 1) * sw, 780], fill=(v, v, v))

# --- Colour patch grid (top-right) ------------------------------------------
gx, gy, cell = 1180, 230, 34
for r in range(8):
    for cix in range(12):
        hue = cix / 12
        val = 1 - r / 8
        i = int(hue * 6)
        f = hue * 6 - i
        p, q, t = 0, val * (1 - f), val * f
        rgb = [(val, t, p), (q, val, p), (p, val, t),
               (p, q, val), (t, p, val), (val, p, q)][i % 6]
        col = tuple(int(255 * c) for c in rgb)
        d.rectangle([gx + cix * cell, gy + r * cell,
                     gx + cix * cell + cell - 2, gy + r * cell + cell - 2], fill=col)

# --- Concentric circles / resolution target (bottom-right) ------------------
cx, cy = 1330, 560
for rad in range(10, 130, 12):
    d.ellipse([cx - rad, cy - rad, cx + rad, cy + rad], outline=(90, 170, 210), width=2)
for a in range(0, 360, 15):
    x2 = cx + 130 * math.cos(math.radians(a))
    y2 = cy + 130 * math.sin(math.radians(a))
    d.line([(cx, cy), (x2, y2)], fill=(60, 120, 150), width=1)

# --- Grid box (mid-left) -----------------------------------------------------
bx, by, bs = 70, 230, 220
d.rectangle([bx, by, bx + bs, by + bs], outline=(70, 120, 150), width=2)
for i in range(1, 11):
    d.line([(bx + i * bs / 10, by), (bx + i * bs / 10, by + bs)], fill=(40, 60, 75))
    d.line([(bx, by + i * bs / 10), (bx + bs, by + i * bs / 10)], fill=(40, 60, 75))

# --- Text block --------------------------------------------------------------
d.text((70, 500), "SUCHER", font=font(MONO, 64), fill=(235, 238, 245))
d.text((72, 585), "terminal graphics test card", font=font(MONO, 30), fill=(150, 200, 230))
d.text((72, 630), "1600 × 1000   ·   sharpness · colour depth · banding · contrast",
       font=font(MONO, 20), fill=(120, 122, 135))
samples = "18px AaBbCc 0123   ·   24px AaBbCc 0123   ·   32px AaBbCc 0123"
d.text((72, 830), samples, font=font(SANS, 26), fill=(210, 214, 224))

# --- Corner registration marks ----------------------------------------------
for (mx, my) in [(8, 8), (W - 28, 8), (8, H - 28), (W - 28, H - 28)]:
    d.rectangle([mx, my, mx + 20, my + 20], outline=(220, 60, 60), width=3)

img.save("samples/picture.jpg", quality=90)
img.save("samples/testcard.png")
print("wrote samples/picture.jpg and samples/testcard.png", img.size)
