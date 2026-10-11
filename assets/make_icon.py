#!/usr/bin/env python3
"""Draw the ActiveGit icon and write assets/activegit.{png,ico}.

No dependencies: the shapes are tested per sample point and averaged for
anti-aliasing, then written as PNG / ICO by hand. The same geometry is in
activegit.svg, which is the version to open in an editor.

    python assets/make_icon.py

The icon is a small commit graph on a dark rounded tile: a blue trunk with
two commits, and a green branch ending in a highlighted commit, the one
that is "active".
"""
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))

# Everything below is in a 256 x 256 design space.
TILE_RADIUS = 56
TOP = (0x2B, 0x2D, 0x42)        # tile gradient, top
BOTTOM = (0x16, 0x16, 0x22)     # tile gradient, bottom
BLUE = (0x89, 0xB4, 0xFA)
GREEN = (0xA6, 0xE3, 0xA1)
HOLE = (0x1E, 0x1E, 0x2E)

STROKE = 18.0
TRUNK = ((92.0, 60.0), (92.0, 196.0))
NODE_R = 25.0
HOLE_R = 9.5
ACTIVE = (174.0, 92.0)
ACTIVE_R = 27.0
GLOW_R = 41.0


def branch_points():
    """The branch: straight down from the active commit, then a curve into
    the trunk."""
    pts = [ACTIVE, (ACTIVE[0], 112.0)]
    p0, p1, p2 = (ACTIVE[0], 112.0), (ACTIVE[0], 160.0), (92.0, 166.0)
    steps = 18
    for i in range(1, steps + 1):
        t = i / steps
        a = (1 - t) * (1 - t)
        b = 2 * (1 - t) * t
        c = t * t
        pts.append((a * p0[0] + b * p1[0] + c * p2[0], a * p0[1] + b * p1[1] + c * p2[1]))
    return pts


BRANCH = branch_points()
BRANCH_BOX = (
    min(p[0] for p in BRANCH) - STROKE,
    min(p[1] for p in BRANCH) - STROKE,
    max(p[0] for p in BRANCH) + STROKE,
    max(p[1] for p in BRANCH) + STROKE,
)


def seg_dist2(px, py, a, b):
    ax, ay = a
    bx, by = b
    dx, dy = bx - ax, by - ay
    length2 = dx * dx + dy * dy
    t = 0.0 if length2 == 0 else max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / length2))
    cx, cy = ax + t * dx, ay + t * dy
    return (px - cx) ** 2 + (py - cy) ** 2


def in_circle(px, py, c, r):
    return (px - c[0]) ** 2 + (py - c[1]) ** 2 <= r * r


def in_tile(px, py):
    r = TILE_RADIUS
    cx = min(max(px, r), 256 - r)
    cy = min(max(py, r), 256 - r)
    return (px - cx) ** 2 + (py - cy) ** 2 <= r * r


def sample(px, py):
    """Color (r, g, b, a) of the design at one point."""
    if not in_tile(px, py):
        return (0, 0, 0, 0)
    t = py / 256.0
    color = [TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3)]

    def over(rgb, alpha=1.0):
        for i in range(3):
            color[i] = color[i] + (rgb[i] - color[i]) * alpha

    half2 = (STROKE / 2) ** 2
    # Soft halo around the active commit.
    if in_circle(px, py, ACTIVE, GLOW_R):
        over(GREEN, 0.22)
    if BRANCH_BOX[0] <= px <= BRANCH_BOX[2] and BRANCH_BOX[1] <= py <= BRANCH_BOX[3]:
        if any(seg_dist2(px, py, BRANCH[i], BRANCH[i + 1]) <= half2 for i in range(len(BRANCH) - 1)):
            over(GREEN)
    # The trunk goes over the branch, so the join is a clean T.
    if seg_dist2(px, py, *TRUNK) <= half2:
        over(BLUE)
    for node in TRUNK:
        if in_circle(px, py, node, NODE_R):
            over(BLUE)
            if in_circle(px, py, node, HOLE_R):
                over(HOLE)
    if in_circle(px, py, ACTIVE, ACTIVE_R):
        over(GREEN)
    return (color[0], color[1], color[2], 255)


def render(size):
    """`size` x `size` RGBA rows, each sample averaged over a sub-grid."""
    sub = 4 if size >= 128 else 6
    scale = 256.0 / size
    rows = []
    for y in range(size):
        row = bytearray()
        for x in range(size):
            acc = [0.0, 0.0, 0.0, 0.0]
            for sy in range(sub):
                for sx in range(sub):
                    r, g, b, a = sample((x + (sx + 0.5) / sub) * scale, (y + (sy + 0.5) / sub) * scale)
                    # Premultiplied, so the transparent corners do not darken the edge.
                    acc[0] += r * a
                    acc[1] += g * a
                    acc[2] += b * a
                    acc[3] += a
            n = sub * sub
            alpha = acc[3] / n
            if acc[3] > 0:
                row += bytes((round(acc[0] / acc[3]), round(acc[1] / acc[3]), round(acc[2] / acc[3]), round(alpha)))
            else:
                row += b"\0\0\0\0"
        rows.append(bytes(row))
    return rows


def png_bytes(size, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    raw = b"".join(b"\0" + row for row in rows)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def bmp_entry(size, rows):
    """32-bit DIB as stored inside an .ico: bottom-up BGRA plus an (unused,
    all-zero) 1-bit mask."""
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    pixels = bytearray()
    for row in reversed(rows):
        for i in range(0, len(row), 4):
            r, g, b, a = row[i : i + 4]
            pixels += bytes((b, g, r, a))
    mask_row = ((size + 31) // 32) * 4
    return header + bytes(pixels) + b"\0" * (mask_row * size)


def main():
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    images = {size: render(size) for size in sizes}
    with open(os.path.join(HERE, "activegit.png"), "wb") as f:
        f.write(png_bytes(256, images[256]))
    # Small sizes as DIBs (every resource compiler takes those), 256 as PNG.
    entries = [(s, png_bytes(s, images[s]) if s == 256 else bmp_entry(s, images[s])) for s in sizes]
    offset = 6 + 16 * len(entries)
    directory = struct.pack("<HHH", 0, 1, len(entries))
    blob = b""
    for size, data in entries:
        directory += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
        blob += data
    with open(os.path.join(HERE, "activegit.ico"), "wb") as f:
        f.write(directory + blob)
    print("wrote activegit.png and activegit.ico (%s)" % ", ".join(str(s) for s in sizes))


if __name__ == "__main__":
    main()
