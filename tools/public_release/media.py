"""Small strict raster handlers; unknown binary formats fail closed.

PNG/ICO metadata checks are deterministic; visible text still requires OCR and
visual review of the FINAL hash. This is not a steganography detector.
"""
from __future__ import annotations

import struct
import zlib


def clean_png(data: bytes) -> bool:
    if not data.startswith(b"\x89PNG\r\n\x1a\n"):
        return False
    offset, seen = 8, []
    while offset + 12 <= len(data):
        size = struct.unpack(">I", data[offset:offset+4])[0]
        kind = data[offset+4:offset+8]
        end = offset + 12 + size
        if end > len(data) or kind not in {b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS"}:
            return False
        expected = struct.unpack(">I", data[end-4:end])[0]
        if zlib.crc32(data[offset+4:end-4]) & 0xffffffff != expected:
            return False
        seen.append(kind)
        offset = end
        if kind == b"IEND":
            return offset == len(data) and seen[0] == b"IHDR" and b"IDAT" in seen and size == 0
    return False


def clean_raster(name: str, data: bytes) -> bool:
    if name.lower().endswith(".png"):
        return clean_png(data)
    if not name.lower().endswith(".ico") or data[:4] != b"\0\0\x01\0" or len(data) < 6:
        return False
    count = struct.unpack("<H", data[4:6])[0]
    if not 1 <= count <= 32 or len(data) < 6 + 16*count:
        return False
    end = 6 + 16*count
    for index in range(count):
        entry = data[6+16*index:22+16*index]
        size, start = struct.unpack("<II", entry[8:16])
        if start != end or not clean_png(data[start:start+size]):
            return False
        end = start + size
    return end == len(data)
