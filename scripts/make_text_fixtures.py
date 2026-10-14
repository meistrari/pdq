#!/usr/bin/env python3
"""Generate the tests/fixtures/text-*.pdf fixtures for `pdq text` tests.

All fixtures are tiny, uncompressed, hand-checkable PDFs:

  text-simple.pdf    one page, Helvetica: "Invoice" at 18pt (72,720 baseline)
                     and "Hello" / "World" at 12pt separated by a TJ gap.
  text-rotate90.pdf  same MediaBox with /Rotate 90, "Rotated" at 24pt
                     (100,200 baseline in unrotated PDF space).
  text-image-only.pdf  one page whose only content is a DCT image (no text).
  text-degraded.pdf  a Type3 font with no ToUnicode and a glyph name that
                     maps to nothing: the glyph renders but has no Unicode
                     mapping, so extraction must flag the page as degraded.
  text-type3-numeric-names.pdf  a Type3 font as some producers write it:
                     no ToUnicode, glyphs named after their own
                     hex code (/C41 = "A"), /FontMatrix [1 0 0 1 0 0] and
                     0.2 Tf, so the em is 0.2 pt while glyphs draw 6.6 pt.
  text-glyph-names.pdf  no ToUnicode either, but the /Differences names are
                     resolvable through the AGL's algorithmic rules, so
                     extraction must recover them rather than degrade.
  text-annotations.pdf  one page word plus three /Widget annotations whose
                     /AP /N appearance streams each draw a distinct word:
                     a normal one (/F 4), a Hidden one (/F 2) and a NoView
                     one (/F 32).
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
    # No ToUnicode, and a glyph name that is neither in the AGL nor spells
    # its own code, so the glyph is visible but unmappable.
    glyph = stream(b"", b"750 0 d0 50 50 650 650 re f")
    font = (b"<</Type/Font/Subtype/Type3/FontBBox[0 0 750 750]"
            b"/FontMatrix[0.001 0 0 0.001 0 0]"
            b"/CharProcs<</shape1 6 0 R>>"
            b"/Encoding<</Type/Encoding/Differences[65/shape1]>>"
            b"/FirstChar 65/LastChar 65/Widths[750]>>")
    content = b"BT /F1 12 Tf 72 720 Td (AAA) Tj ET"
    (FIXTURES / "text-degraded.pdf").write_bytes(
        page_pdf(content, font=font, extra_objs=[glyph]))


def make_type3_numeric_names():
    # As some producers write it: a Type3 font in a 1:1 glyph space, scaled down
    # by a tiny Tf, whose /Differences names each glyph after its own code in
    # hex. A reader that only consults ToUnicode and the AGL sees U+FFFD, and
    # one that takes the 0.2 pt em for the font size sees every glyph tens of
    # ems apart and emits one run per glyph.
    def glyph(proc: bytes) -> bytes:
        return stream(b"", b"25 0 0 -7 25 26 d1 " + proc)

    font = (b"<</Type/Font/Subtype/Type3/FontBBox[0 -7 25 26]"
            b"/FontMatrix[1 0 0 1 0 0]"
            b"/CharProcs<</C20 6 0 R/C41 7 0 R/C42 8 0 R>>"
            b"/Encoding<</Type/Encoding/Differences[32/C20 65/C41/C42]>>"
            b"/FirstChar 32/LastChar 66/Widths[25" + b" 0" * 32 + b" 25 25]>>")
    content = b"BT /F1 0.2 Tf 72 720 Td (AB BA) Tj ET"
    (FIXTURES / "text-type3-numeric-names.pdf").write_bytes(page_pdf(
        content, font=font, extra_objs=[
            stream(b"", b"25 0 d0"),
            glyph(b"2 0 20 26 re f"),
            glyph(b"2 0 20 26 re f 6 6 8 14 re f"),
        ]))


def make_type3_line_spacing():
    # Both fonts draw small glyphs on baselines twelve points apart. Neither
    # excess FontBBox padding nor horizontal shear should merge the lines.
    cases = [
        ("text-type3-padded-bbox.pdf", b".01 0 0 .01 0 0",
         b"-1000 -1000 1000 1000", 50, 70, b"10"),
        ("text-type3-shear.pdf", b"1 0 20 1 0 0",
         b"0 -7 25 26", 25, 26, b"0.2"),
    ]
    for name, matrix, bbox, width, height, size in cases:
        font = (b"<</Type/Font/Subtype/Type3/FontBBox[" + bbox + b"]"
                b"/FontMatrix[" + matrix + b"]"
                b"/CharProcs<</A 6 0 R/B 7 0 R>>"
                b"/Encoding<</Type/Encoding/Differences[65/A/B]>>"
                b"/FirstChar 65/LastChar 66/Widths[%d %d]>>" % (width, width))
        glyph = stream(b"", b"%d 0 d0 2 0 %d %d re f" %
                       (width, width - 4, height))
        content = (b"BT /F1 " + size +
                   b" Tf 72 720 Td (AB) Tj 0 -12 Td (BA) Tj ET")
        for suffix, scale in [("", b"1"), ("-small", b"0.25"), ("-large", b"4")]:
            scaled = b"q " + scale + b" 0 0 " + scale + b" 0 0 cm " + content + b" Q"
            filename = name.removesuffix(".pdf") + suffix + ".pdf"
            (FIXTURES / filename).write_bytes(
                page_pdf(scaled, font=font, extra_objs=[glyph, glyph]))


def make_glyph_names():
    # None of these names are in the AGL as written: the specification
    # defines them algorithmically, by stripping the variant suffix after
    # the first period and splitting ligatures on underscores. A reader that
    # only does a flat table lookup sees three unmappable glyphs and degrades
    # the page; resolving them yields "fi", "-" and "ffl".
    font = (b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica"
            b"/Encoding<</Type/Encoding/Differences"
            b"[65/f_i 66/hyphen.case 67/f_f_l]>>>>")
    content = b"BT /F1 12 Tf 72 720 Td (ABC) Tj ET"
    (FIXTURES / "text-glyph-names.pdf").write_bytes(
        page_pdf(content, font=font))


def make_annotations():
    # Widget appearance streams are form XObjects placed at the annotation's
    # /Rect, so each word lands inside its own widget rectangle. The flags
    # are the three cases text extraction has to distinguish: 4 = Print,
    # 2 = Hidden (never extracted), 32 = NoView (poppler and mupdf suppress
    # it, hayro does not).
    def widget(rect: bytes, flags: int, ap: int) -> bytes:
        return (b"<</Type/Annot/Subtype/Widget/FT/Tx/T(field%d)"
                b"/Rect[%s]/F %d/AP<</N %d 0 R>>>>" % (ap, rect, flags, ap))

    def appearance(word: bytes) -> bytes:
        return stream(
            b"/Type/XObject/Subtype/Form/BBox[0 0 100 20]"
            b"/Resources<</Font<</F1 4 0 R>>>>",
            b"BT /F1 12 Tf 2 4 Td (%s) Tj ET" % word)

    content = b"BT /F1 18 Tf 72 720 Td (Content) Tj ET"
    (FIXTURES / "text-annotations.pdf").write_bytes(page_pdf(
        content,
        extra_page=b"/Annots[6 0 R 8 0 R 10 0 R]",
        extra_objs=[
            widget(b"200 600 300 620", 4, 7), appearance(b"Filled"),
            widget(b"200 560 300 580", 2, 9), appearance(b"Hidden"),
            widget(b"200 520 300 540", 32, 11), appearance(b"Noview"),
        ]))


if __name__ == "__main__":
    make_simple()
    make_rotate90()
    make_image_only()
    make_degraded()
    make_glyph_names()
    make_type3_numeric_names()
    make_type3_line_spacing()
    make_annotations()
    for f in sorted(FIXTURES.glob("text-*.pdf")):
        print(f.name, f.stat().st_size, "bytes")
