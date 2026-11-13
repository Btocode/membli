#!/usr/bin/env python3
"""Render the Membli logo: an abstract orbital bloom: three glowing orbits around a core.

Writes assets/icon/membli-<size>.png for the usual icon sizes and
assets/icon/membli-256.rgba (raw RGBA) for the window icon.
Needs Pillow:  python3 -I scripts/make_icon.py
"""
import math
import os

from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 1024  # working size; scaled down at the end
OUT = os.path.join(os.path.dirname(__file__), "..", "assets", "icon")


def rounded_tile():
    tile = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, S - 1, S - 1], radius=int(S * 0.22), fill=255)
    # deep space gradient: lighter teal-navy in the middle, near black at the edges
    bg = Image.new("RGBA", (S, S), (4, 7, 13, 255))
    glow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    gd = ImageDraw.Draw(glow)
    for i in range(60, 0, -1):
        r = S * 0.62 * i / 60
        a = int(70 * (1 - i / 60) ** 1.4)
        gd.ellipse([S / 2 - r, S / 2 - r, S / 2 + r, S / 2 + r], fill=(10, 52, 72, a))
    bg = Image.alpha_composite(bg, glow)
    tile.paste(bg, (0, 0), mask)
    return tile, mask


def ring(color, tilt_deg, rx, ry, width):
    """A tilted orbit ellipse, brighter on one side so the rings read as passing over each other."""
    layer = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    cx, cy = S / 2, S / 2
    d.ellipse([cx - rx, cy - ry, cx + rx, cy + ry], outline=color + (255,), width=width)
    layer = layer.rotate(tilt_deg, resample=Image.BICUBIC, center=(cx, cy))
    fade = Image.new("L", (S, S), 0)
    fd = ImageDraw.Draw(fade)
    a0 = math.radians(tilt_deg)
    for i in range(256):
        t = i / 255
        x = cx + math.cos(a0) * (t - 0.5) * S * 1.4
        y = cy - math.sin(a0) * (t - 0.5) * S * 1.4
        fd.ellipse([x - S * 0.5, y - S * 0.5, x + S * 0.5, y + S * 0.5], fill=int(90 + 165 * t))
    fade = fade.filter(ImageFilter.GaussianBlur(S * 0.08))
    r, g, b, al = layer.split()
    al = ImageChops.multiply(al, fade)
    return Image.merge("RGBA", (r, g, b, al))


def add_glow(base, layer, blur, strength):
    g = layer.filter(ImageFilter.GaussianBlur(blur))
    if strength != 1:
        r, gg, b, a = g.split()
        a = a.point(lambda v: min(255, int(v * strength)))
        g = Image.merge("RGBA", (r, gg, b, a))
    base = Image.alpha_composite(base, g)
    return Image.alpha_composite(base, layer)


def main():
    os.makedirs(OUT, exist_ok=True)
    img, mask = rounded_tile()
    rings = [
        ((0, 229, 255), 28, S * 0.36, S * 0.15, int(S * 0.034)),
        ((77, 140, 255), 88, S * 0.36, S * 0.15, int(S * 0.034)),
        ((255, 43, 214), 148, S * 0.36, S * 0.15, int(S * 0.034)),
    ]
    for color, tilt, rx, ry, w in rings:
        img = add_glow(img, ring(color, tilt, rx, ry, w), S * 0.025, 1.6)
    # core
    core = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    cd = ImageDraw.Draw(core)
    for i in range(40, 0, -1):
        r = S * 0.13 * i / 40
        a = int(255 * (1 - i / 40) ** 0.7)
        cd.ellipse([S / 2 - r, S / 2 - r, S / 2 + r, S / 2 + r], fill=(180, 250, 255, a))
    r = S * 0.045
    cd.ellipse([S / 2 - r, S / 2 - r, S / 2 + r, S / 2 + r], fill=(255, 255, 255, 255))
    img = add_glow(img, core, S * 0.03, 1.0)
    # thin rim
    rim = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle([3, 3, S - 4, S - 4], radius=int(S * 0.22), outline=(0, 229, 255, 70), width=6)
    img = Image.alpha_composite(img, rim)
    # clip to the tile
    out = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    out.paste(img, (0, 0), mask)

    for size in (16, 24, 32, 48, 64, 128, 256, 512):
        out.resize((size, size), Image.LANCZOS).save(os.path.join(OUT, f"membli-{size}.png"))
    with open(os.path.join(OUT, "membli-256.rgba"), "wb") as f:
        f.write(out.resize((256, 256), Image.LANCZOS).tobytes())
    print("icons written to", os.path.normpath(OUT))


if __name__ == "__main__":
    main()
