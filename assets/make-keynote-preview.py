#!/usr/bin/env python3
"""Regenerate the QuickLook title-slide preview embedded in samples/slides.key.

The .key package carries a single `preview.jpg` (1600x1200) that the Keynote
viewer displays. This redraws it, then rewrites it into the package. Run from
the repo root:  python3 assets/make-keynote-preview.py
"""
import io
import zipfile
import shutil
from PIL import Image, ImageDraw, ImageFont

W, H = 1600, 1200


def font(paths, size):
    for p in paths:
        try:
            return ImageFont.truetype(p, size)
        except OSError:
            continue
    return ImageFont.load_default()


SANS = ["/System/Library/Fonts/SFNS.ttf", "/System/Library/Fonts/Helvetica.ttc"]

img = Image.new("RGB", (W, H))
px = img.load()
# Diagonal gradient: navy (top-left) -> blue (bottom-right).
a, b = (13, 20, 45), (40, 70, 150)
for y in range(H):
    for x in range(W):
        t = (x / W + y / H) / 2
        px[x, y] = tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))

d = ImageDraw.Draw(img)
d.text((120, 340), "Sucher", font=font(SANS, 150), fill=(245, 247, 250))
d.rectangle([124, 486, 300, 500], fill=(120, 200, 250))
d.text((124, 528), "A fast terminal file viewer", font=font(SANS, 52), fill=(160, 190, 230))
d.text((124, 762), "Keynote preview · slide 1 of 6", font=font(SANS, 34), fill=(130, 150, 190))

# Bar chart, bottom-right; 4th bar highlighted green.
bars = [(1060, 300), (1150, 420), (1240, 360), (1330, 470), (1420, 400)]
bw = 74
base = 1120
for i, (bx, bh) in enumerate(bars):
    col = (80, 210, 130) if i == 3 else (130, 200, 245)
    d.rectangle([bx, base - bh, bx + bw, base], fill=col)
d.text((1055, 1130), "throughput by format", font=font(SANS, 30), fill=(140, 165, 205))

buf = io.BytesIO()
img.save(buf, "JPEG", quality=88)
new = buf.getvalue()

src = "samples/slides.key"
tmp = src + ".new"
with zipfile.ZipFile(src) as zin, zipfile.ZipFile(tmp, "w", zipfile.ZIP_DEFLATED) as zout:
    for item in zin.infolist():
        data = new if item.filename == "preview.jpg" else zin.read(item.filename)
        zout.writestr(item, data)
shutil.move(tmp, src)
print("rewrote preview.jpg in", src, img.size)
