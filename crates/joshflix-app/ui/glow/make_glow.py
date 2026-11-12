#!/usr/bin/env python3
"""Generate the pre-blurred focus-glow images (card.png, pill.png).

Slint's femtovg renderer re-renders and Gaussian-blurs a new GPU texture
whenever a drop shadow's blur, size or color changes, so animating a focus
glow through `drop-shadow-blur` re-blurs on every frame. These images carry
the blur already; the UI shows them as nine-slice images and only animates
their opacity, which costs a few textured quads per frame.

Each image is cut from a large blurred shape: the corner (or cap) slices,
plus the one-pixel row and column right next to them as the stretchable
centre, so the slices join without a seam. Pixels are white; the UI colors
them with `colorize: Theme.glow`. Matches Slint's `drop-shadow-blur`
(sigma = blur / 2). Standard library only.

Run from anywhere: python3 make_glow.py
"""

import math
import struct
import zlib
from pathlib import Path

HERE = Path(__file__).resolve().parent


def rounded_rect_mask(width, height, x0, y0, w, h, r, samples=4):
    """Coverage (0..1) of a rounded rectangle, supersampled for smooth edges."""
    mask = [[0.0] * width for _ in range(height)]
    step = 1.0 / samples
    for y in range(height):
        for x in range(width):
            hits = 0
            for sy in range(samples):
                py = y + (sy + 0.5) * step
                for sx in range(samples):
                    px = x + (sx + 0.5) * step
                    if not (x0 <= px <= x0 + w and y0 <= py <= y0 + h):
                        continue
                    # Distance into the corner region, if any.
                    cx = min(max(px, x0 + r), x0 + w - r)
                    cy = min(max(py, y0 + r), y0 + h - r)
                    if (px - cx) ** 2 + (py - cy) ** 2 <= r * r:
                        hits += 1
            mask[y][x] = hits / (samples * samples)
    return mask


def blur(mask, sigma):
    """Separable Gaussian blur (exact for a 2D Gaussian)."""
    radius = math.ceil(sigma * 3)
    kernel = [math.exp(-(i * i) / (2 * sigma * sigma)) for i in range(-radius, radius + 1)]
    total = sum(kernel)
    kernel = [k / total for k in kernel]
    height, width = len(mask), len(mask[0])

    def pass_1d(src, horizontal):
        out = [[0.0] * width for _ in range(height)]
        for y in range(height):
            for x in range(width):
                acc = 0.0
                for i, k in enumerate(kernel):
                    d = i - radius
                    if horizontal:
                        xx = x + d
                        if 0 <= xx < width:
                            acc += src[y][xx] * k
                    else:
                        yy = y + d
                        if 0 <= yy < height:
                            acc += src[yy][x] * k
                out[y][x] = acc
        return out

    return pass_1d(pass_1d(mask, True), False)


def write_png(path, alpha):
    height, width = len(alpha), len(alpha[0])
    raw = bytearray()
    for row in alpha:
        raw.append(0)  # filter: none
        for a in row:
            raw += bytes((255, 255, 255, max(0, min(255, round(a * 255)))))

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


def cut(full, border_x, border_y):
    """Keep `border` pixels at each side plus the one-pixel strip beside them."""
    height, width = len(full), len(full[0])
    xs = list(range(border_x + 1)) + list(range(width - border_x, width))
    ys = list(range(border_y + 1)) + list(range(height - border_y, height))
    if border_y == 0:
        ys = list(range(height))
    return [[full[y][x] for x in xs] for y in ys]


def make(name, *, blur_px, radius, shape_h, border_x, border_y):
    sigma = blur_px / 2
    pad = math.ceil(sigma * 3)
    inner = 160  # long straight sides, so the edge profile is the true one
    shape_w = inner
    h = shape_h if shape_h else inner
    width, height = shape_w + 2 * pad, h + 2 * pad
    full = blur(rounded_rect_mask(width, height, pad, pad, shape_w, h, radius), sigma)
    image = cut(full, border_x, border_y)
    write_png(HERE / name, image)
    print(f"{name}: {len(image[0])}x{len(image)}, pad {pad}px, nine-slice borders "
          f"top/bottom {border_y}, left/right {border_x}")


if __name__ == "__main__":
    # Cards: Theme.radius 10px, 18px blur. Borders = pad + radius + 2.
    make("card.png", blur_px=18, radius=10, shape_h=None, border_x=27 + 10 + 2, border_y=27 + 10 + 2)
    # Pills and round buttons: drawn for a 36px-tall pill, 14px blur. No top/bottom
    # border: the whole height stretches, so 30-40px buttons all fit.
    make("pill.png", blur_px=14, radius=18, shape_h=36, border_x=21 + 18 + 2, border_y=0)
