"""Render Cropmark PNG/ICO assets from the brand geometry."""

from __future__ import annotations

import struct
import zlib
from pathlib import Path


def png_chunk(tag: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


def write_png(path: Path, size: int, rgba: bytes) -> None:
    raw = b"".join(b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size))
    payload = b"".join(
        [
            b"\x89PNG\r\n\x1a\n",
            png_chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)),
            png_chunk(b"IDAT", zlib.compress(raw, 9)),
            png_chunk(b"IEND", b""),
        ]
    )
    path.write_bytes(payload)


def lerp(a: int, b: int, t: float) -> int:
    return int(a + (b - a) * t)


def rounded_rect_sdf(x: float, y: float, size: int, radius: float, inset: float) -> float:
    half = size / 2
    qx = abs(x - half) - (half - inset - radius)
    qy = abs(y - half) - (half - inset - radius)
    return (max(qx, 0.0) ** 2 + max(qy, 0.0) ** 2) ** 0.5 + min(max(qx, qy), 0.0) - radius


def crop_mark_alpha(x: float, y: float, size: int) -> float:
    inset = size * (0.18 if size <= 32 else 0.20)
    length = size * (0.34 if size <= 32 else 0.18)
    thickness = max(size * (0.12 if size <= 32 else 0.055), 1.5)
    corners = [
        (inset, inset, 1, 1),
        (size - inset, inset, -1, 1),
        (inset, size - inset, 1, -1),
        (size - inset, size - inset, -1, -1),
    ]
    alpha = 0.0
    for cx, cy, dx, dy in corners:
        hx = min(max((x - cx) * dx, 0.0), length)
        hy = abs(y - cy)
        horiz = max(0.0, 1.0 - max(abs((x - cx) * dx - hx), hy) / (thickness / 2))
        vx = abs(x - cx)
        vy = min(max((y - cy) * dy, 0.0), length)
        vert = max(0.0, 1.0 - max(vx, abs((y - cy) * dy - vy)) / (thickness / 2))
        alpha = max(alpha, horiz, vert)
    if size < 64:
        return alpha
    inner = size * 0.31
    box = max(abs(x - size / 2), abs(y - size / 2))
    ring = max(0.0, 1.0 - abs(box - inner) / max(size * 0.018, 1.0))
    return max(alpha, ring * 0.85)


def render(size: int) -> bytes:
    bg = (19, 78, 74)
    fg = (94, 234, 212)
    radius = size * 0.22
    pixels = bytearray(size * size * 4)
    for y in range(size):
        for x in range(size):
            sdf = rounded_rect_sdf(x + 0.5, y + 0.5, size, radius, size * 0.02)
            cover = max(0.0, min(1.0, 0.5 - sdf))
            mark = crop_mark_alpha(x + 0.5, y + 0.5, size) * cover
            r = lerp(bg[0], fg[0], mark)
            g = lerp(bg[1], fg[1], mark)
            b = lerp(bg[2], fg[2], mark)
            i = (y * size + x) * 4
            pixels[i : i + 4] = bytes((r, g, b, int(255 * cover)))
    return bytes(pixels)


def encode_png(size: int, rgba: bytes) -> bytes:
    raw = b"".join(b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size))
    return b"".join(
        [
            b"\x89PNG\r\n\x1a\n",
            png_chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)),
            png_chunk(b"IDAT", zlib.compress(raw, 9)),
            png_chunk(b"IEND", b""),
        ]
    )


def write_icns(path: Path, images: list[tuple[bytes, int, bytes]]) -> None:
    body = bytearray()
    for tag, size, rgba in images:
        png = encode_png(size, rgba)
        body += tag + struct.pack(">I", len(png) + 8) + png
    path.write_bytes(b"icns" + struct.pack(">I", len(body) + 8) + body)


def write_ico(path: Path, images: list[tuple[int, bytes]]) -> None:
    chunks: list[bytes] = []
    directory = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for size, rgba in images:
        raw = b"".join(b"\x00" + rgba[y * size * 4 : (y + 1) * size * 4] for y in range(size))
        png = b"".join(
            [
                b"\x89PNG\r\n\x1a\n",
                png_chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)),
                png_chunk(b"IDAT", zlib.compress(raw, 9)),
                png_chunk(b"IEND", b""),
            ]
        )
        directory += struct.pack(
            "<BBBBHHII",
            0 if size >= 256 else size,
            0 if size >= 256 else size,
            0,
            0,
            1,
            32,
            len(png),
            offset,
        )
        chunks.append(png)
        offset += len(png)
    path.write_bytes(bytes(directory) + b"".join(chunks))


def main() -> None:
    root = Path(__file__).resolve().parents[2]
    brand = root / "assets" / "brand"
    icons = root / "src-tauri" / "icons"
    brand.mkdir(parents=True, exist_ok=True)
    icons.mkdir(parents=True, exist_ok=True)

    master = render(1024)
    write_png(brand / "cropmark-icon.png", 1024, master)
    sizes = {
        32: render(32),
        128: render(128),
        256: render(256),
        512: render(512),
        1024: master,
    }
    write_png(icons / "32x32.png", 32, sizes[32])
    write_png(icons / "128x128.png", 128, sizes[128])
    write_png(icons / "128x128@2x.png", 256, sizes[256])
    write_png(icons / "icon.png", 512, sizes[512])
    write_ico(
        icons / "icon.ico",
        [(16, render(16)), (32, sizes[32]), (48, render(48)), (256, sizes[256])],
    )
    write_icns(
        icons / "icon.icns",
        [
            (b"icp5", 32, sizes[32]),
            (b"ic07", 128, sizes[128]),
            (b"ic08", 256, sizes[256]),
            (b"ic09", 512, sizes[512]),
            (b"ic10", 1024, master),
        ],
    )


if __name__ == "__main__":
    main()
