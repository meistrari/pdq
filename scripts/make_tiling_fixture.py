#!/usr/bin/env python3
"""Synthesize tests/fixtures/tiling-step-overflow.pdf (FDN-992 regression).

Replicates the pathological structure of the iText "paint the tile once,
never repeat" idiom found in Brazilian signature-certificate pages — a
page-sized PatternType 1 tile with /XStep 99999 /YStep 99999 — without
copying any byte of a real document. Rendering this used to make hayro
size the tile pixmap from the scaled step: the f32→u16 cast saturated at
65535 and allocated 65535×65535×4 ≈ 16 GiB.

Four A4 pages, all filling the page with a variant of the same pattern.
The cell content is deliberately NON-uniform so the tests can detect both
failure modes of naive fixes (early repetition from clamping the period,
blur from downscaling the cell):

  red rect     top-left quadrant     (0, 421)..(297.5, 842)
  green rect   bottom-right quadrant (297.5, 0)..(595, 421)
  blue rect    inside the otherwise-empty top-right quadrant
  black 1.5pt hairline at x = 100 (full height) — sharpness probe

  page 1  XStep/YStep 99999, identity matrix   (the incident shape)
  page 2  same steps, /Matrix translate(-99999, -99999): the visible
          window sits on lattice instance (1, 1) — must render exactly
          like page 1
  page 3  same steps, /Matrix translate(-50000, 0): the visible window
          falls in the gap between instances — must render blank
  page 4  XStep/YStep -99999, identity matrix — the lattice is the same
          set as page 1's, must render exactly like page 1

All streams are stored uncompressed so the fixture is reviewable as text.
"""

from pathlib import Path

OUT = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "tiling-step-overflow.pdf"

CELL = b"""1 0 0 rg
0 421 297.5 421 re f
0 0.6 0 rg
297.5 0 297.5 421 re f
0 0 1 rg
400 600 50 50 re f
0 0 0 rg
100 0 1.5 842 re f
"""

PAGE_CONTENT = b"""q
/Pattern cs /P1 scn
0 0 595 842 re f
Q
"""


def pattern(cell_ref: int, x_step: str, y_step: str, matrix: str) -> bytes:
    # /Resources of the pattern itself is empty: the cell only paints
    # device-color rects.
    return (
        b"<< /Type /Pattern /PatternType 1 /PaintType 1 /TilingType 2"
        b" /BBox [0 0 595 842]"
        b" /XStep " + x_step.encode() + b" /YStep " + y_step.encode() +
        b" /Matrix [" + matrix.encode() + b"]"
        b" /Resources << /ProcSet [/PDF] >>"
        b" /Length " + str(len(CELL)).encode() + b" >>\nstream\n" + CELL + b"endstream"
    )


def stream_obj(body: bytes) -> bytes:
    return (
        b"<< /Length " + str(len(body)).encode() + b" >>\nstream\n" + body + b"endstream"
    )


def page(parent: int, contents: int, pat: int) -> bytes:
    return (
        b"<< /Type /Page /Parent " + str(parent).encode() + b" 0 R"
        b" /MediaBox [0 0 595 842]"
        b" /Resources << /Pattern << /P1 " + str(pat).encode() + b" 0 R >> >>"
        b" /Contents " + str(contents).encode() + b" 0 R >>"
    )


def build() -> bytes:
    objs: dict[int, bytes] = {}
    objs[1] = b"<< /Type /Catalog /Pages 2 0 R >>"
    objs[2] = b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 >>"
    objs[7] = stream_obj(PAGE_CONTENT)
    objs[8] = pattern(7, "99999", "99999", "1 0 0 1 0 0")
    objs[9] = pattern(7, "99999", "99999", "1 0 0 1 -99999 -99999")
    objs[10] = pattern(7, "99999", "99999", "1 0 0 1 -50000 0")
    objs[11] = pattern(7, "-99999", "-99999", "1 0 0 1 0 0")
    objs[3] = page(2, 7, 8)
    objs[4] = page(2, 7, 9)
    objs[5] = page(2, 7, 10)
    objs[6] = page(2, 7, 11)

    out = bytearray(b"%PDF-1.7\n")
    offsets: dict[int, int] = {}
    for num in sorted(objs):
        offsets[num] = len(out)
        out += str(num).encode() + b" 0 obj\n" + objs[num] + b"\nendobj\n"

    xref_at = len(out)
    count = len(objs) + 1
    out += b"xref\n0 " + str(count).encode() + b"\n"
    out += b"0000000000 65535 f \n"
    for num in sorted(objs):
        out += f"{offsets[num]:010d} 00000 n \n".encode()
    out += (
        b"trailer\n<< /Size " + str(count).encode() + b" /Root 1 0 R >>\n"
        b"startxref\n" + str(xref_at).encode() + b"\n%%EOF\n"
    )
    return bytes(out)


if __name__ == "__main__":
    OUT.write_bytes(build())
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")
