#!/usr/bin/env python3
"""Compose assets/social-preview.png (1280x640) for the GitHub social card.

Wordmark + tagline on the left, a UI screenshot on the right, in sucher's
palette. Run from the repo root:  python3 assets/make-social.py
"""
from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 640
BG = (12, 14, 19)
PANEL = (18, 21, 28)
SKY = (125, 211, 252)
AMBER = (252, 211, 77)
GREEN = (74, 222, 128)
GRAY = (150, 152, 164)

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

img = Image.new("RGB", (W, H), BG)
d = ImageDraw.Draw(img)

# Right: screenshot in a rounded panel, fixed width, vertically centered.
shot = Image.open("assets/browser.png").convert("RGB")
sw = 540
sh = int(shot.height * sw / shot.width)
shot = shot.resize((sw, sh), Image.LANCZOS)
px = W - sw - 52
py = (H - sh) // 2
panel = Image.new("RGB", (sw + 24, sh + 24), PANEL)
pd = ImageDraw.Draw(panel)
pd.rounded_rectangle([0, 0, sw + 23, sh + 23], radius=14, outline=(40, 44, 54), width=2)
img.paste(panel, (px - 12, py - 12))
img.paste(shot, (px, py))

# Left text column — kept clear of the panel (panel left edge ≈ px-12).
x = 64
d.text((x, 168), "sucher", font=font(MONO, 104), fill=SKY)
d.text((x + 4, 300), "Fast terminal viewer + directory browser",
       font=font(SANS, 26), fill=(225, 227, 235))
d.text((x + 4, 342), "markdown · sheets · pdf · images · video · docx",
       font=font(SANS, 22), fill=GRAY)
chip = font(MONO, 20)
d.text((x + 4, 398), "real pixels", font=chip, fill=AMBER)
d.text((x + 152, 398), "·  kitty / iterm2 / sixel", font=chip, fill=GRAY)
d.text((x + 4, 432), "s <file|dir>", font=chip, fill=GREEN)

img.save("assets/social-preview.png")
print("wrote assets/social-preview.png", img.size)
