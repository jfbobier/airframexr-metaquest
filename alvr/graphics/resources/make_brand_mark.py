#!/usr/bin/env python3
"""Bakes the AirFrameXR mark into airframexr_mark.rgba (raw straight-alpha RGBA8, SIZE x SIZE) for the Quest
welcome panel (include_bytes! in ../src/lobby.rs). Source: branding/logos/airframexr-mark-color.svg of the
AirFrameXR repository. Needs rsvg-convert (librsvg); the PNG is decoded with the standard library only.

    python3 make_brand_mark.py /path/to/AirFrameXR/branding/logos/airframexr-mark-color.svg
"""
import struct, subprocess, sys, zlib

SIZE = 192

def decode_png(data):
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    pos, idat, w, h = 8, b"", 0, 0
    while pos < len(data):
        n, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + n]
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
            assert depth == 8 and ctype == 6 and interlace == 0, "expected 8-bit RGBA, not interlaced"
        elif kind == b"IDAT":
            idat += body
        pos += 12 + n
    raw, stride, out = zlib.decompress(idat), w * 4, bytearray()
    prev = bytearray(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            a = line[i - 4] if i >= 4 else 0
            b = prev[i]
            c = prev[i - 4] if i >= 4 else 0
            if f == 1: line[i] = (line[i] + a) & 255
            elif f == 2: line[i] = (line[i] + b) & 255
            elif f == 3: line[i] = (line[i] + ((a + b) >> 1)) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out += line
        prev = line
    return w, h, bytes(out)

png = subprocess.run(["rsvg-convert", "-w", str(SIZE), "-h", str(SIZE), sys.argv[1]], check=True, capture_output=True).stdout
w, h, rgba = decode_png(png)
assert (w, h) == (SIZE, SIZE)
open("airframexr_mark.rgba", "wb").write(rgba)
print(f"airframexr_mark.rgba {w}x{h} {len(rgba)} bytes")
