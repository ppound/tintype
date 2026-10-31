"""Draws the Tintype app icon: a dark lacquered plate (a tintype is a photo on
a metal plate) with a sepia oval vignette, a hill silhouette and a brass rim.

    python assets/icon/make_icon.py

Writes icon.png (1024 px), the PNG sizes the installers use, icon.ico and
icon.icns next to this script. Needs Pillow.
"""

import math
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

OUT = Path(__file__).parent
S = 1024


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def plate() -> Image.Image:
    """Rounded square, dark warm metal with a soft diagonal sheen."""
    img = Image.new("RGBA", (S, S))
    top, bottom = (54, 45, 39), (24, 20, 18)
    px = img.load()
    for y in range(S):
        for x in range(S):
            t = (x * 0.35 + y) / (S * 1.35)
            px[x, y] = (*lerp(top, bottom, t), 255)
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([40, 40, S - 40, S - 40], radius=200, fill=255)
    img.putalpha(mask)
    return img


def vignette(box) -> Image.Image:
    """The sepia oval: bright centre fading to dark edges, with hills."""
    x0, y0, x1, y1 = box
    w, h = x1 - x0, y1 - y0
    oval = Image.new("RGBA", (w, h))
    px = oval.load()
    light, dark = (236, 222, 194), (122, 98, 70)
    for y in range(h):
        for x in range(w):
            dx, dy = (x - w / 2) / (w / 2), (y - h * 0.42) / (h / 2)
            t = min(1.0, math.hypot(dx, dy) ** 1.6)
            px[x, y] = (*lerp(light, dark, t), 255)
    d = ImageDraw.Draw(oval)
    # Two hills, far and near.
    far = [(0, h * 0.70)] + [(x, h * 0.66 - math.sin(x / w * math.pi * 1.3 + 0.4) * h * 0.08) for x in range(0, w + 1, 8)] + [(w, h)] + [(0, h)]
    d.polygon(far, fill=(150, 124, 92, 255))
    near = [(0, h * 0.80)] + [(x, h * 0.78 - math.sin(x / w * math.pi + 0.9) * h * 0.10) for x in range(0, w + 1, 8)] + [(w, h)] + [(0, h)]
    d.polygon(near, fill=(70, 55, 42, 255))
    # A low sun.
    r = w * 0.10
    d.ellipse([w * 0.62 - r, h * 0.36 - r, w * 0.62 + r, h * 0.36 + r], fill=(250, 240, 220, 255))
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).ellipse([0, 0, w - 1, h - 1], fill=255)
    mask = mask.filter(ImageFilter.GaussianBlur(2))
    oval.putalpha(mask)
    return oval


def main():
    img = plate()
    box = (232, 176, S - 232, S - 176)
    # Brass rim, slightly larger than the oval.
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).ellipse([box[0] - 22, box[1] - 22, box[2] + 22, box[3] + 22], outline=(196, 158, 96, 255), width=18)
    img.alpha_composite(rim)
    img.alpha_composite(vignette(box), (box[0], box[1]))
    # Keep everything inside the rounded plate.
    alpha = img.getchannel("A")
    plate_mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(plate_mask).rounded_rectangle([40, 40, S - 40, S - 40], radius=200, fill=255)
    img.putalpha(ImageChops.multiply(alpha, plate_mask))

    img.save(OUT / "icon.png")
    for size in (32, 64, 128, 256, 512):
        img.resize((size, size), Image.LANCZOS).save(OUT / f"{size}x{size}.png")
    img.save(OUT / "icon.ico", sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    img.save(OUT / "icon.icns")
    print("wrote", sorted(p.name for p in OUT.iterdir() if p.suffix in (".png", ".ico", ".icns")))


if __name__ == "__main__":
    main()
