#!/usr/bin/env python3
"""Generate the tests/fixtures/text-*.pdf fixtures for `pdq text` tests.

All fixtures are tiny, uncompressed, hand-checkable PDFs:

  text-simple.pdf    one page, Helvetica: "Invoice" at 18pt (72,720 baseline)
                     and "Hello" / "World" at 12pt separated by a TJ gap.
  text-rotate90.pdf  same MediaBox with /Rotate 90, "Rotated" at 24pt
                     (100,200 baseline in unrotated PDF space).
  text-image-only.pdf  one page whose only content is a DCT image (no text).
  text-degraded.pdf  a Type3 font with no ToUnicode: the glyph renders but
                     has no Unicode mapping, so extraction must flag the
                     page as degraded.
"""
from pathlib import Path

HERE = Path(__file__).parent
FIXTURES = HERE.parent / "tests" / "fixtures"


def build(objs: list[bytes], root: int) -> bytes:
    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for n, body in enumerate(objs, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % n
        out += body
        out += b"\nendobj\n"
    xref_at = len(out)
    out += b"xref\n0 %d\n" % (len(objs) + 1)
    out += b"0000000000 65535 f \n"
    for off in offsets[1:]:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<</Size %d/Root %d 0 R>>\nstartxref\n%d\n%%%%EOF\n" % (
        len(objs) + 1, root, xref_at)
    return bytes(out)


def stream(dict_inner: bytes, data: bytes) -> bytes:
    return b"<<%s/Length %d>>stream\n%s\nendstream" % (dict_inner, len(data), data)


def page_pdf(content: bytes, extra_page: bytes = b"",
             resources: bytes = b"/Font<</F1 4 0 R>>",
             font: bytes = b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
             extra_objs=None) -> bytes:
    objs = [
        b"<</Type/Catalog/Pages 2 0 R>>",
        b"<</Type/Pages/Kids[3 0 R]/Count 1>>",
        b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 612 792]"
        b"/Resources<<%s>>/Contents 5 0 R%s>>" % (resources, extra_page),
        font,
        stream(b"", content),
    ]
    if extra_objs:
        objs.extend(extra_objs)
    return build(objs, root=1)


def make_simple():
    content = (b"BT /F1 18 Tf 72 720 Td (Invoice) Tj ET\n"
               b"BT /F1 12 Tf 72 700 Td [(Hello) -2000 (World)] TJ ET")
    (FIXTURES / "text-simple.pdf").write_bytes(page_pdf(content))


def make_rotate90():
    content = b"BT /F1 24 Tf 100 200 Td (Rotated) Tj ET"
    (FIXTURES / "text-rotate90.pdf").write_bytes(
        page_pdf(content, extra_page=b"/Rotate 90"))


def make_image_only():
    jpg = (FIXTURES / "tiny-gray.jpg").read_bytes()
    img = stream(
        b"/Type/XObject/Subtype/Image/Width 8/Height 8"
        b"/ColorSpace/DeviceGray/BitsPerComponent 8/Filter/DCTDecode", jpg)
    content = b"q 200 0 0 200 100 400 cm /Im1 Do Q"
    (FIXTURES / "text-image-only.pdf").write_bytes(page_pdf(
        content,
        resources=b"/XObject<</Im1 6 0 R>>",
        font=b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
        extra_objs=[img]))


def make_degraded():
    # Type3 fonts can only provide Unicode through a ToUnicode CMap; this one
    # has none, so its glyphs are visible but unmappable.
    glyph = stream(b"", b"750 0 d0 50 50 650 650 re f")
    font = (b"<</Type/Font/Subtype/Type3/FontBBox[0 0 750 750]"
            b"/FontMatrix[0.001 0 0 0.001 0 0]"
            b"/CharProcs<</square 6 0 R>>"
            b"/Encoding<</Type/Encoding/Differences[65/square]>>"
            b"/FirstChar 65/LastChar 65/Widths[750]>>")
    content = b"BT /F1 12 Tf 72 720 Td (AAA) Tj ET"
    (FIXTURES / "text-degraded.pdf").write_bytes(
        page_pdf(content, font=font, extra_objs=[glyph]))


if __name__ == "__main__":
    make_simple()
    make_rotate90()
    make_image_only()
    make_degraded()
    for f in sorted(FIXTURES.glob("text-*.pdf")):
        print(f.name, f.stat().st_size, "bytes")
