#!/usr/bin/env python3
"""Synthesize anonymized-but-pathological PDF fixtures.

Replicates the structural pathologies of two Brazilian court PDFs without
copying a single byte of content from them:

  A) PJe/OpenPDF-like: 12,732 pages, 65,570 objects, bottom-up 10-ary page
     tree (1,418 internal nodes), per-page inline resources, filter zoo
     (JBIG2/DCT/JPX/CCITT/Flate + chains), 728 link annots, 176 outlines,
     named-destination tree, 112 XMP metadata streams, zero-length raw
     streams, uncompressed form XObjects.
  B) TRF4/FPDF+FPDI-like: 2,642 pages, 17,861 objects, flat page tree
     (single /Pages node with all kids), ONE shared /Resources dict listing
     every /TPLn form XObject of the whole file, MediaBox inherited from the
     page-tree root, 2,473 outline items (depth 3), mojibake UTF-8 Info
     string.

Stream counts and per-class stream length distributions are taken from
census JSONs (lengths-pje.json / lengths-trf4.json) so file sizes and
per-object cost profiles match the originals closely.
"""
import json
import struct
import sys
import zlib
from pathlib import Path

import random

HERE = Path(__file__).parent
RNG = random.Random(20260703)

B64_ALPHABET = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

LOREM = (
    "EXCELENTISSIMO SENHOR DOUTOR JUIZ DO TRABALHO DA VARA FICTICIA. "
    "PARTE AUTORA FICTICIA, pessoa fisica de existencia meramente ilustrativa, "
    "inscrita no CPF 000.000.000-00, vem, respeitosamente, por meio deste "
    "documento sintetico gerado para fins de benchmark, expor e requerer o que "
    "segue. Este arquivo nao contem dados pessoais reais; todo o conteudo "
    "textual foi substituido por texto de preenchimento equivalente em volume. "
)


def b64_junk(n: int) -> bytes:
    """Incompressible-ish printable padding (safe inside a content-stream comment)."""
    return bytes(B64_ALPHABET[b & 63] for b in RNG.randbytes(n))


def flate_random(target: int) -> bytes:
    """Valid zlib stream of approximately `target` bytes (random payload)."""
    k = max(0, target - 11)
    return zlib.compress(RNG.randbytes(k), 6)


def pad_block(n: int) -> bytes:
    """Valid, invisible, incompressible content ops: off-page Tj of random bytes.

    Strict content parsers (lopdf::Content::decode_strict) must accept the
    result, so padding lives inside a string literal, not a % comment. Bytes
    that would need escaping are remapped to printable stand-ins.
    """
    payload = RNG.randbytes(n)
    payload = (payload.replace(b"\\", b"|").replace(b"(", b"{")
               .replace(b")", b"}").replace(b"\r", b"~"))
    return b"q BT /F1 4 Tf 0 -20000 Td (" + payload + b") Tj ET Q"


def flate_ops(base: bytes, target: int) -> bytes:
    """Valid zlib stream of ~target bytes whose payload starts with PDF operators."""
    data = zlib.compress(base, 6)
    pad_len = 0
    for _ in range(5):
        deficit = target - len(data)
        if deficit <= 24:
            break
        pad_len += max(8, deficit - 45)
        data = zlib.compress(base + b"\n" + pad_block(pad_len), 6)
    return data


def plain_ops(base: bytes, target: int) -> bytes:
    """Uncompressed content-stream bytes of exactly max(len(base), target) bytes."""
    pad = target - len(base) - 46
    if pad <= 0:
        return base + b" " * max(0, target - len(base))
    return base + b"\n" + pad_block(pad)


class JpegFactory:
    """Valid JPEG payloads of arbitrary exact size: the repo's tiny noise
    JPEG fixture, grown with COM segments before the EOI marker."""

    def __init__(self):
        template_path = HERE.parent / "tests" / "fixtures" / "tiny-gray.jpg"
        self.template = template_path.read_bytes()
        assert self.template.endswith(b"\xff\xd9")

    def make(self, target: int) -> bytes:
        t = self.template
        if target <= len(t) + 4:
            return t
        need = target - len(t)
        body, coms = t[:-2], []
        while need > 0:
            seg = min(need - 4, 65531)
            if seg < 0:
                seg = 0
            coms.append(b"\xff\xfe" + struct.pack(">H", seg + 2) + RNG.randbytes(seg))
            need -= seg + 4
        return body + b"".join(coms) + b"\xff\xd9"


JPEG = JpegFactory()


def xmp_packet(target: int) -> bytes:
    core = (
        b'<?xpacket begin="\xef\xbb\xbf" id="W5M0MpCehiHzreSzNTczkc9d"?>\n'
        b'<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF '
        b'xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
        b'<rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/">'
        b'<dc:title><rdf:Alt><rdf:li xml:lang="x-default">Documento sintetico anonimizado'
        b'</rdf:li></rdf:Alt></dc:title></rdf:Description></rdf:RDF></x:xmpmeta>\n'
    )
    tail = b'<?xpacket end="w"?>'
    pad = max(0, target - len(core) - len(tail))
    return core + b" " * pad + tail


class Builder:
    def __init__(self, version: str):
        self.version = version
        self.objs: dict[int, bytes] = {}
        self._next = 1

    def reserve(self) -> int:
        n = self._next
        self._next += 1
        return n

    def put(self, n: int, body: bytes):
        assert n not in self.objs
        self.objs[n] = body

    def add(self, body: bytes) -> int:
        n = self.reserve()
        self.put(n, body)
        return n

    def stream(self, dict_inner: bytes, data: bytes) -> bytes:
        return (b"<<" + dict_inner + b"/Length %d>>stream\n" % len(data)
                + data + b"\nendstream")

    def add_stream(self, dict_inner: bytes, data: bytes) -> int:
        return self.add(self.stream(dict_inner, data))

    @property
    def count(self) -> int:
        return self._next - 1

    def write(self, path: Path, trailer_fmt: bytes):
        with open(path, "wb") as f:
            f.write(b"%PDF-" + self.version.encode() + b"\n%\xe2\xe3\xcf\xd3\n")
            offsets = [0] * (self._next)
            for n in range(1, self._next):
                offsets[n] = f.tell()
                f.write(b"%d 0 obj\n" % n)
                f.write(self.objs[n])
                f.write(b"\nendobj\n")
            xref_at = f.tell()
            f.write(b"xref\n0 %d\n" % self._next)
            f.write(b"0000000000 65535 f \n")
            for n in range(1, self._next):
                f.write(b"%010d 00000 n \n" % offsets[n])
            f.write(b"trailer\n")
            f.write(trailer_fmt.replace(b"@SIZE@", b"%d" % self._next))
            f.write(b"\nstartxref\n%d\n%%%%EOF\n" % xref_at)


def take(lst, n):
    out = lst[:n]
    del lst[:n]
    return out


def image_dict(with_type: bool, filt: bytes, w: int, h: int, cs: bytes,
               bpc: int, extra: bytes = b"") -> bytes:
    t = b"/Type/XObject" if with_type else b""
    return (t + b"/Subtype/Image/Width %d/Height %d" % (w, h)
            + cs + b"/BitsPerComponent %d" % bpc
            + (b"/Filter" + filt if filt else b"") + extra)


def flate_image(pdf: Builder, target: int, with_type: bool, ncomp: int = 3,
                cs: bytes = b"/ColorSpace/DeviceRGB") -> int:
    w = 64
    h = max(1, (target - 11) // (w * ncomp))
    data = zlib.compress(RNG.randbytes(w * h * ncomp), 6)
    return pdf.add_stream(
        image_dict(with_type, b"/FlateDecode", w, h, cs, 8), data)


def dct_image(pdf: Builder, target: int, with_type: bool) -> int:
    data = JPEG.make(target)
    return pdf.add_stream(
        image_dict(with_type, b"/DCTDecode", 8, 8,
                   b"/ColorSpace/DeviceGray", 8), data)


def flate_dct_image(pdf: Builder, target: int, with_type: bool) -> int:
    data = zlib.compress(JPEG.make(max(0, target - 11)), 6)
    return pdf.add_stream(
        image_dict(with_type, b"[/FlateDecode/DCTDecode]", 8, 8,
                   b"/ColorSpace/DeviceGray", 8), data)


def jbig2_image(pdf: Builder, target: int, with_type: bool, globals_ref: int) -> int:
    extra = b"/ImageMask true/DecodeParms<</JBIG2Globals %d 0 R>>" % globals_ref
    return pdf.add_stream(
        image_dict(with_type, b"/JBIG2Decode", 992, 1536, b"", 1, extra),
        RNG.randbytes(target))


def ccitt_image(pdf: Builder, target: int, with_type: bool) -> int:
    extra = b"/DecodeParms<</K -1/Columns 1728/Rows 2200>>"
    return pdf.add_stream(
        image_dict(with_type, b"/CCITTFaxDecode", 1728, 2200, b"", 1, extra),
        RNG.randbytes(target))


def jpx_image(pdf: Builder, target: int, with_type: bool) -> int:
    return pdf.add_stream(
        image_dict(with_type, b"/JPXDecode", 128, 128,
                   b"/ColorSpace/DeviceRGB", 8), RNG.randbytes(target))


def raw_image(pdf: Builder, target: int, with_type: bool) -> int:
    w, h = 1, max(1, target)
    data = RNG.randbytes(w * h)
    return pdf.add_stream(
        image_dict(with_type, b"", w, h, b"/ColorSpace/DeviceGray", 8), data)


STD_FONTS = [b"/Helvetica", b"/Times-Roman", b"/Times-Bold", b"/Helvetica-Bold",
             b"/Courier", b"/Times-Italic", b"/Helvetica-Oblique", b"/Symbol"]


class FontPool:
    """Creates font dicts consuming class budgets; returns lists of obj numbers."""

    def __init__(self, pdf: Builder, n_type1, n_tt, n_type0, n_cidft2, n_desc,
                 tounicode_flate: list, tounicode_hex: list,
                 fontfile_flate: list, fontfile_hex: list, n_enc: int,
                 widths_budget: int):
        self.pdf = pdf
        self.type1, self.tt, self.type0 = [], [], []
        descs = []
        for i in range(n_desc):
            ff = b""
            if fontfile_flate:
                target = fontfile_flate.pop()
                ff_obj = pdf.add_stream(
                    b"/Subtype/Type1C/Filter/FlateDecode", flate_random(target))
                ff = b"/FontFile3 %d 0 R" % ff_obj
            elif fontfile_hex:
                target = fontfile_hex.pop()
                hexdata = RNG.randbytes(max(0, (target - 1) // 2)).hex().encode() + b">"
                ff_obj = pdf.add_stream(
                    b"/Subtype/CIDFontType0C/Filter/ASCIIHexDecode", hexdata)
                ff = b"/FontFile3 %d 0 R" % ff_obj
            descs.append(pdf.add(
                b"<</Type/FontDescriptor/FontName/FicFont%d/Flags 32"
                b"/FontBBox[-100 -200 1100 900]/ItalicAngle 0/Ascent 720"
                b"/Descent -200/CapHeight 660/StemV 80%s>>" % (i, ff)))
        encs = [pdf.add(b"<</Type/Encoding/BaseEncoding/WinAnsiEncoding"
                        b"/Differences[32/space]>>") for _ in range(n_enc)]

        def tounicode_entry(i):
            if tounicode_flate:
                target = tounicode_flate.pop()
                cmap = (b"/CIDInit /ProcSet findresource begin 12 dict begin "
                        b"begincmap 1 begincodespacerange <00> <FF> "
                        b"endcodespacerange endcmap end end")
                o = self.pdf.add_stream(b"/Filter/FlateDecode",
                                        flate_ops(cmap, target))
                return b"/ToUnicode %d 0 R" % o
            if tounicode_hex:
                target = tounicode_hex.pop()
                hx = RNG.randbytes(max(0, (target - 1) // 2)).hex().encode() + b">"
                o = self.pdf.add_stream(b"/Filter/ASCIIHexDecode", hx)
                return b"/ToUnicode %d 0 R" % o
            return b""

        self._widths_left = widths_budget
        self.widths_used = 0

        def widths_entry():
            if self._widths_left <= 0:
                return b"/Widths[%s]" % b" ".join(b"500" for _ in range(8))
            self._widths_left -= 1
            self.widths_used += 1
            arr = pdf.add(b"[" + b" ".join(
                b"%d" % RNG.randint(200, 900) for _ in range(95)) + b"]")
            return b"/Widths %d 0 R" % arr

        for i in range(n_type1):
            enc = (b"/Encoding %d 0 R" % encs[i % len(encs)]) if (encs and i % 19 == 0) \
                else b"/Encoding/WinAnsiEncoding"
            self.type1.append(pdf.add(
                b"<</Type/Font/Subtype/Type1/BaseFont%s%s%s>>"
                % (STD_FONTS[i % len(STD_FONTS)], enc, tounicode_entry(i))))
        for i in range(n_tt):
            d = descs[i % len(descs)] if descs else 0
            fd = (b"/FontDescriptor %d 0 R" % d) if d else b""
            self.tt.append(pdf.add(
                b"<</Type/Font/Subtype/TrueType/BaseFont/FicSans%d"
                b"/FirstChar 32/LastChar 126%s%s%s>>"
                % (i % 40, widths_entry(), fd, tounicode_entry(i))))
        cids = []
        for i in range(n_cidft2):
            d = descs[(i * 7) % len(descs)] if descs else 0
            fd = (b"/FontDescriptor %d 0 R" % d) if d else b""
            cids.append(pdf.add(
                b"<</Type/Font/Subtype/CIDFontType2/BaseFont/FicCID%d"
                b"/CIDSystemInfo<</Registry(Adobe)/Ordering(Identity)"
                b"/Supplement 0>>%s/DW 1000/CIDToGIDMap/Identity>>" % (i % 40, fd)))
        for i in range(n_type0):
            c = cids[i % len(cids)] if cids else 0
            self.type0.append(pdf.add(
                b"<</Type/Font/Subtype/Type0/BaseFont/FicCID%d"
                b"/Encoding/Identity-H/DescendantFonts[%d 0 R]%s>>"
                % (i % 40, c, tounicode_entry(i))))
        self.all = self.type1 + self.tt + self.type0


def spread_images(pdf: Builder, L: dict, prefix_typed: str, prefix_untyped: str,
                  jbig2_globals: list) -> list:
    """Create every image object of the census; return their obj numbers."""
    imgs = []

    def cls(key, fn, *a):
        for target in L.pop(key, []):
            imgs.append(fn(pdf, target, *a))

    for typed, pre in ((True, prefix_typed), (False, prefix_untyped)):
        wt = typed
        cls(pre + "/Image|/FlateDecode",
            lambda p, t, w=wt: flate_image(p, t, w))
        cls(pre + "/Image|/DCTDecode", lambda p, t, w=wt: dct_image(p, t, w))
        cls(pre + "/Image|[/FlateDecode/DCTDecode]",
            lambda p, t, w=wt: flate_dct_image(p, t, w))
        cls(pre + "/Image|/JBIG2Decode",
            lambda p, t, w=wt: jbig2_image(p, t, w,
                                           jbig2_globals[len(imgs) % len(jbig2_globals)]
                                           if jbig2_globals else 0))
        cls(pre + "/Image|/CCITTFaxDecode", lambda p, t, w=wt: ccitt_image(p, t, w))
        cls(pre + "/Image|/JPXDecode", lambda p, t, w=wt: jpx_image(p, t, w))
        cls(pre + "/Image|raw", lambda p, t, w=wt: raw_image(p, t, w))
    RNG.shuffle(imgs)
    return imgs


def generic_leftovers(pdf: Builder, L: dict) -> list:
    """Emit any census stream class not consumed elsewhere; return obj numbers."""
    extras = []
    for key in sorted(L.keys()):
        lengths = L.pop(key)
        t, st, filt = key.split("|")
        for target in lengths:
            inner = b""
            if t != "-":
                inner += b"/Type" + t.encode()
            if st != "-":
                inner += b"/Subtype" + st.encode()
            if filt == "raw":
                data = RNG.randbytes(target)
            elif filt == "/FlateDecode":
                inner += b"/Filter/FlateDecode"
                data = flate_random(target)
            elif filt == "/ASCIIHexDecode":
                inner += b"/Filter/ASCIIHexDecode"
                data = RNG.randbytes(max(0, (target - 1) // 2)).hex().encode() + b">"
            elif filt == "[/ASCII85Decode/FlateDecode]":
                import base64
                inner += b"/Filter[/ASCII85Decode/FlateDecode]"
                z = zlib.compress(RNG.randbytes(max(0, int(target * 0.79))), 6)
                data = base64.a85encode(z) + b"~>"
            else:
                inner += b"/Filter" + filt.encode()
                data = RNG.randbytes(target)
            extras.append(pdf.add_stream(inner, data))
    return extras


# ---------------------------------------------------------------- fixture A
def build_pje(out_path: Path):
    L = {k: v[:] for k, v in json.load(open(HERE / "lengths-pje.json")).items()}
    for v in L.values():
        RNG.shuffle(v)
    pdf = Builder("1.5")
    TARGET_OBJS = 65570
    NPAGES = 12732

    # shared header images (img0 gray smask + img1 rgb), like the PJe seal
    xi = L["/XObject|/Image|/FlateDecode"]
    t0, t1 = xi.pop(), xi.pop()
    w = 86
    h0 = max(1, (t0 - 11) // w)
    img0 = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/ColorSpace/DeviceGray/Width %d/Height %d"
        b"/BitsPerComponent 8/Filter/FlateDecode" % (w, h0),
        zlib.compress(RNG.randbytes(w * h0), 6))
    h1 = max(1, (t1 - 11) // (w * 3))
    img1 = pdf.add_stream(
        b"/Type/XObject/Subtype/Image/ColorSpace/DeviceRGB/Width %d/Height %d"
        b"/BitsPerComponent 8/SMask %d 0 R/Filter/FlateDecode" % (w, h1, img0),
        zlib.compress(RNG.randbytes(w * h1 * 3), 6))

    # raw streams: JBIG2 globals + PieceInfo leftovers
    raw_lengths = L.pop("-|-|raw")
    nonzero = [x for x in raw_lengths if x > 0]
    zeros = len(raw_lengths) - len(nonzero)
    glob_targets = take(nonzero, 20)
    jbig2_globals = [pdf.add_stream(b"", RNG.randbytes(t)) for t in glob_targets]
    misc_raw = [pdf.add_stream(b"", RNG.randbytes(t)) for t in nonzero]
    misc_raw += [pdf.add_stream(b"", b"") for _ in range(zeros)]

    # fonts
    flate_misc = L.pop("-|-|/FlateDecode")
    contents_targets = take(flate_misc, NPAGES)
    fontfile_flate = L.pop("-|/Type1C|/FlateDecode", [])
    fontfile_flate += L.pop("-|/Type1C|[/ASCII85Decode/FlateDecode]", [])
    pool = FontPool(pdf, n_type1=2087, n_tt=1511, n_type0=1412, n_cidft2=691,
                    n_desc=1338,
                    tounicode_flate=flate_misc,
                    tounicode_hex=L.pop("-|-|/ASCIIHexDecode", []),
                    fontfile_flate=fontfile_flate,
                    fontfile_hex=L.pop("-|/CIDFontType0C|/ASCIIHexDecode", []),
                    n_enc=110, widths_budget=1450)
    # /Type/Stream fontfile-ish blobs, hung off descriptors via PieceInfo instead
    type_stream = [pdf.add_stream(b"/Type/Stream/Length1 %d/Filter/FlateDecode"
                                  % (t * 2), flate_random(t))
                   for t in L.pop("/Stream|-|/FlateDecode", [])]

    extgs = [pdf.add(b"<</Type/ExtGState/BM/Normal/CA 1/ca 1/LW %d>>" % (i % 4 + 1))
             for i in range(333)]

    imgs = spread_images(pdf, L, "/XObject|", "-|", jbig2_globals)

    # XMP metadata streams
    xmp = [pdf.add_stream(b"/Type/Metadata/Subtype/XML", xmp_packet(t))
           for t in L.pop("/Metadata|/XML|raw", [])]

    # pattern streams
    patterns = [pdf.add_stream(
        b"/Type/Pattern/PatternType 1/PaintType 1/TilingType 1"
        b"/BBox[0 0 8 8]/XStep 8/YStep 8/Resources<<>>/Filter/FlateDecode",
        flate_ops(b"0.5 g 0 0 8 8 re f", t))
        for t in L.pop("/Pattern|-|/FlateDecode", [])]

    # forms: 12,732 page forms + extras
    form_flate = L.pop("/XObject|/Form|/FlateDecode")
    form_raw = L.pop("/XObject|/Form|raw", [])
    extra_form_targets = form_flate[NPAGES:]
    page_form_targets = form_flate[:NPAGES]

    extra_forms = []
    img_i = 0

    def form_resources(idx: int, child_form: int = 0) -> bytes:
        nonlocal img_i
        # per-document font locality: forms of one "document" share one small
        # font cluster, like the source PJe merge (keeps per-page closures small)
        seg = idx // 74
        f1 = pool.type1[(seg * 11) % len(pool.type1)]
        f2 = pool.all[(seg * 29 + 5) % len(pool.all)]
        r = b"/ProcSet[/PDF/Text/ImageB/ImageC/ImageI]/Font<</F1 %d 0 R/F2 %d 0 R>>" % (f1, f2)
        xo = b""
        if img_i < len(imgs) and idx % 3 != 2:
            xo += b"/Im%d %d 0 R" % (img_i % 10, imgs[img_i])
            img_i += 1
        if child_form:
            xo += b"/Fx %d 0 R" % child_form
        if xo:
            r += b"/XObject<<" + xo + b">>"
        if idx % 200 == 0 and patterns:
            r += b"/Pattern<</P1 %d 0 R>>" % patterns[idx // 200 % len(patterns)]
        if idx % 40 == 0:
            r += b"/ExtGState<</GS1 %d 0 R>>" % extgs[idx % len(extgs)]
        return b"<<" + r + b">>"

    for j, t in enumerate(extra_form_targets):
        base = (b"BT /F1 9 Tf 40 800 Td (%s) Tj ET"
                % LOREM[: 60 + j % 40].encode())
        meta = b"/Metadata %d 0 R" % xmp[j % len(xmp)] if (xmp and j % 2 == 0) else b""
        extra_forms.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.32 841.92]"
            b"/Matrix[1 0 0 1 0 0]/Resources%s%s/Filter/FlateDecode"
            % (form_resources(j * 31 + 5), meta),
            flate_ops(base, t)))
    for j, t in enumerate(form_raw):
        base = b"BT /F1 9 Tf 40 780 Td (Documento sintetico %d) Tj ET" % j
        extra_forms.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.32 841.92]"
            b"/Matrix[1 0 0 1 0 0]/Resources%s" % form_resources(j * 17 + 3),
            plain_ops(base, t)))

    page_forms = []
    for i, t in enumerate(page_form_targets):
        lines = []
        y = 800
        for k in range(6):
            frag = LOREM[(i * 7 + k * 61) % 200: (i * 7 + k * 61) % 200 + 72]
            lines.append(b"BT /F1 10 Tf 46 %d Td (%s) Tj ET" % (y, frag.encode()))
            y -= 16
        child = extra_forms[i % len(extra_forms)] if i % 49 == 0 else 0
        meta = b""
        if xmp and i % 116 == 0:
            meta = b"/Metadata %d 0 R" % xmp[(i // 116) % len(xmp)]
        page_forms.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 -19.84 595.32 841.92]"
            b"/Matrix[1 0 0 1 0 19.84]/Resources%s%s/Filter/FlateDecode"
            % (form_resources(i, child), meta),
            flate_ops(b"\n".join(lines), t)))

    # pages + contents + annots + dests
    NANNOTS = 728
    annot_pages = set(range(0, NANNOTS * 17, 17))
    page_nums = [pdf.reserve() for _ in range(NPAGES)]
    dest_arrays, annots = [], {}
    for i in sorted(annot_pages):
        tgt = page_nums[i]  # self-referential dest, like the original
        d = pdf.add(b"[%d 0 R/XYZ null null 0]" % tgt)
        dest_arrays.append(d)
        annots[i] = pdf.add(
            b"<</Subtype/Link/A<</S/GoTo/D %d 0 R>>/C[0 0 1]/Border[0 0 0]"
            b"/Rect[152.02 570.4 442.98 584.4]>>" % d)

    # parent placeholders fixed later
    parents = {}
    contents = []
    pf = [pool.type1[0], pool.type1[1 % len(pool.type1)],
          pool.type1[2 % len(pool.type1)], pool.type1[3 % len(pool.type1)]]
    for i in range(NPAGES):
        ops = (b"q 51.6 0 0 51.6 266.7 786 cm /img1 Do Q\n"
               b"q BT /F1 9 Tf 498 806 Td (Fls.: %d) Tj ET Q\n"
               b"q 1 0 0 1 0 0 cm /Xf0 Do Q" % (i + 1))
        contents.append(pdf.add_stream(b"/Filter/FlateDecode",
                                       flate_ops(ops, contents_targets[i])))

    # page tree, bottom-up 10-ary
    def build_tree(children, counts):
        levels = [list(zip(children, counts))]
        while len(levels[-1]) > 1:
            cur = levels[-1]
            nxt = []
            for s in range(0, len(cur), 10):
                grp = cur[s:s + 10]
                num = pdf.reserve()
                nxt.append((num, sum(c for _, c in grp), grp))
            levels.append([(n, c) for n, c, _ in nxt])
            for num, cnt, grp in nxt:
                parents.update({child: num for child, _ in grp})
                pdf.put(num, b"<</Type/Pages/Kids[%s]/Count %d/Parent @P%d@>>"
                        % (b" ".join(b"%d 0 R" % ch for ch, _ in grp), cnt, num))
        return levels[-1][0][0]

    root_pages = build_tree(page_nums, [1] * NPAGES)

    # fix parent placeholders in internal nodes; root's own /Parent removed
    for n, body in list(pdf.objs.items()):
        tag = b"/Parent @P%d@" % n
        if tag in body:
            if n == root_pages:
                pdf.objs[n] = body.replace(tag, b"/ITXT(1.3.26)")
            else:
                pdf.objs[n] = body.replace(tag, b"/Parent %d 0 R" % parents[n])

    for i in range(NPAGES):
        res = (b"/Font<</F1 %d 0 R/F2 %d 0 R/F3 %d 0 R/F4 %d 0 R>>"
               b"/XObject<</img0 %d 0 R/img1 %d 0 R/Xf0 %d 0 R>>"
               % (pf[0], pf[1], pf[2], pf[3], img0, img1, page_forms[i]))
        extra = b""
        if i in annots:
            extra += b"/Annots[%d 0 R]" % annots[i]
        pdf.put(page_nums[i],
                b"<</Type/Page/Contents %d 0 R/Resources<<%s>>%s"
                b"/Parent %d 0 R/MediaBox[0 0 595 842]>>"
                % (contents[i], res, extra, parents[page_nums[i]]))

    extras = generic_leftovers(pdf, L)
    assert not L, L

    # outlines: 176 flat entries
    NOUT = 176
    out_root = pdf.reserve()
    out_items = [pdf.reserve() for _ in range(NOUT)]
    named = []
    for j, n in enumerate(out_items):
        pg = page_nums[(j * 72) % NPAGES]
        prev = b"/Prev %d 0 R" % out_items[j - 1] if j else b""
        nxt = b"/Next %d 0 R" % out_items[j + 1] if j + 1 < NOUT else b""
        pdf.put(n, b"<</Title(01/01/2026 - Documento Ficticio %03d)"
                b"/A<</S/GoTo/D[%d 0 R/XYZ null null 0]>>/Parent %d 0 R%s%s>>"
                % (j + 1, pg, out_root, prev, nxt))
        named.append((b"(dest%03d)" % j, pg))
    pdf.put(out_root, b"<</Type/Outlines/First %d 0 R/Last %d 0 R/Count %d>>"
            % (out_items[0], out_items[-1], NOUT))

    dest_leaf = pdf.add(b"<</Names[" + b" ".join(
        b"%s[%d 0 R/XYZ null null 0]" % (nm, pg) for nm, pg in named) + b"]>>")
    names_root = pdf.add(b"<</Dests %d 0 R>>" % dest_leaf)

    info = pdf.add(
        b"<</Title(PROCESSO: 0000000-00.0000.5.05.0000 - ACAO TRABALHISTA - "
        b"RITO ORDINARIO)"
        b"/Subject(RECLAMANTE: PARTE AUTORA FICTICIA                            ; "
        b"RECLAMADO: RECLAMADA FICTICIA S.A.)"
        b"/Keywords(DIREITO DO TRABALHO \\(0\\) / Direito Individual do Trabalho "
        b"\\(0\\) / Verbas Remuneratorias, Indenizatorias e Beneficios \\(0\\))"
        b"/Author(Processo Judicial Eletronico)/Creator(PJe - 2.19.3)"
        b"/Producer(OpenPDF 1.3.26)"
        b"/CreationDate(D:20260101080000-03'00')/ModDate(D:20260101080000-03'00')>>")

    # filler to hit the exact original object count; everything reachable via
    # an anchor array on the CATALOG (page splitters rebuild the catalog, so
    # none of this bleeds into per-page split outputs -- like the original,
    # whose bulky side objects hang off /Outlines and font structures)
    filler_needed = TARGET_OBJS - pdf.count - 2  # anchor array + catalog
    assert filler_needed >= 0, filler_needed
    filler = [pdf.add(b"[%d 0 R/XYZ null null 0]" % page_nums[(k * 997) % NPAGES])
              for k in range(filler_needed)]
    anchor = pdf.add(b"[" + b" ".join(
        b"%d 0 R" % x
        for x in (filler + misc_raw + type_stream + extras + imgs[img_i:]))
        + b"]")
    catalog = pdf.add(b"<</Names %d 0 R/Type/Catalog/Outlines %d 0 R/Pages %d 0 R"
                      b"/PieceInfo<</PDQfill<</Private %d 0 R>>>>>>"
                      % (names_root, out_root, root_pages, anchor))

    assert pdf.count == TARGET_OBJS, pdf.count
    pdf.write(out_path,
              b"<</Info %d 0 R/ID [<f1283887edb47ad64a6737686df39e30>"
              b"<f1283887edb47ad64a6737686df39e30>]/Root %d 0 R/Size @SIZE@>>"
              % (info, catalog))


# ---------------------------------------------------------------- fixture B
def build_trf4(out_path: Path):
    L = {k: v[:] for k, v in json.load(open(HERE / "lengths-trf4.json")).items()}
    for v in L.values():
        RNG.shuffle(v)
    pdf = Builder("1.7")
    TARGET_OBJS = 17861
    NPAGES = 2642

    pages_obj = pdf.reserve()   # object 1: flat page tree, holds MediaBox
    res_obj = pdf.reserve()     # object 2: THE shared resources dict

    flate_misc = L.pop("-|-|/FlateDecode")
    contents_targets = take(flate_misc, NPAGES)

    page_nums, content_nums = [], []
    for i in range(NPAGES):
        p = pdf.reserve()
        base = (b"2 J\n0.57 w\nq 0 J 1 w 0 j 0 G 0 g "
                b"1.0000 0 0 1.0000 0.0000 0.0000 cm /TPL%d Do Q\n" % i)
        c = pdf.add_stream(b"/Filter/FlateDecode",
                           flate_ops(base, contents_targets[i]))
        pdf.put(p, b"<</Type/Page/Parent %d 0 R/Resources %d 0 R/Contents %d 0 R>>"
                % (pages_obj, res_obj, c))
        page_nums.append(p)
        content_nums.append(c)

    # fonts (FPDI-copied zoo)
    pool = FontPool(pdf, n_type1=1005, n_tt=233, n_type0=298, n_cidft2=298,
                    n_desc=593,
                    tounicode_flate=flate_misc,
                    tounicode_hex=[],
                    fontfile_flate=L.pop("-|/Type1C|/FlateDecode", []),
                    fontfile_hex=[],
                    n_enc=1, widths_budget=380)
    f1 = pool.type1[0]

    extgs = [pdf.add(b"<</Type/ExtGState/BM/Normal/CA 1/ca 1>>")
             for _ in range(138)]
    imgs = spread_images(pdf, L, "/XObject|", "-|", [])

    shadings = [pdf.add(b"<</ShadingType 2/ColorSpace/DeviceRGB"
                        b"/Coords[0 0 1 1]/Function<</FunctionType 2/Domain[0 1]"
                        b"/C0[0 0 0]/C1[1 1 1]/N 1>>>>") for _ in range(32)]
    pattern_dicts = [pdf.add(b"<</Type/Pattern/PatternType 2/Shading %d 0 R>>"
                             % shadings[i % len(shadings)]) for i in range(64)]
    pattern_streams = [pdf.add_stream(
        b"/Type/Pattern/PatternType 1/PaintType 1/TilingType 1/BBox[0 0 8 8]"
        b"/XStep 8/YStep 8/Resources<<>>/Filter/FlateDecode",
        flate_ops(b"0.5 g 0 0 8 8 re f", t))
        for t in L.pop("/Pattern|-|/FlateDecode", [])]

    idx_cs = [pdf.add(b"[/Indexed/DeviceRGB 255 <" +
                      RNG.randbytes(768).hex().encode() + b">]")
              for _ in range(7)]

    # TPL form XObjects, each with an indirect nested resources dict
    form_flate = L.pop("/XObject|/Form|/FlateDecode")
    form_raw = L.pop("/XObject|/Form|raw", [])
    tpl_targets = form_flate[:NPAGES]
    extra_targets = form_flate[NPAGES:]

    def tpl_resources(idx, child=0):
        # per-document font clusters (~600 source docs), like FPDI's copies
        seg = idx // 5
        r = (b"/ProcSet[/PDF/Text/ImageB/ImageC/ImageI]"
             b"/Font<</F1 %d 0 R/F2 %d 0 R>>"
             % (pool.type1[(seg * 7) % len(pool.type1)],
                pool.all[(seg * 13 + 2) % len(pool.all)]))
        xo = b""
        if imgs and idx % 12 == 0:
            xo += b"/Im1 %d 0 R" % imgs[(idx // 12) % len(imgs)]
        if child:
            xo += b"/Fx %d 0 R" % child
        r += b"/XObject<<" + xo + b">>"
        if idx % 19 == 0:
            r += b"/ExtGState<</GS1 %d 0 R>>" % extgs[idx % len(extgs)]
        if idx % 41 == 0:
            r += b"/Pattern<</P1 %d 0 R>>" % pattern_dicts[idx % len(pattern_dicts)]
        return r

    extra_forms = []
    for j, t in enumerate(extra_targets):
        rd = pdf.add(b"<<" + tpl_resources(j * 29 + 7) + b">>")
        extra_forms.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.28 841.89]"
            b"/Resources %d 0 R/Filter/FlateDecode" % rd,
            flate_ops(b"BT /F1 9 Tf 50 400 Td (Anexo sintetico %d) Tj ET" % j, t)))
    for j, t in enumerate(form_raw):
        rd = pdf.add(b"<<" + tpl_resources(j * 3 + 1) + b">>")
        extra_forms.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.28 841.89]"
            b"/Resources %d 0 R" % rd,
            plain_ops(b"BT /F1 9 Tf 50 400 Td (Anexo bruto %d) Tj ET" % j, t)))

    tpls = []
    for i, t in enumerate(tpl_targets):
        child = extra_forms[i % len(extra_forms)] if i % 83 == 0 else 0
        rd = pdf.add(b"<<" + tpl_resources(i, child) + b">>")
        lines = []
        y = 780
        for k in range(8):
            frag = LOREM[(i * 11 + k * 47) % 210: (i * 11 + k * 47) % 210 + 68]
            lines.append(b"BT /F1 10 Tf 52 %d Td (Fl. %d: %s) Tj ET"
                         % (y, i + 1, frag.encode()))
            y -= 18
        tpls.append(pdf.add_stream(
            b"/Type/XObject/Subtype/Form/FormType 1/BBox[0 0 595.28 841.89]"
            b"/Resources %d 0 R/Filter/FlateDecode" % rd,
            flate_ops(b"\n".join(lines), t)))

    # raw streams -> PieceInfo blobs on the Pages node via anchor later
    misc_raw = [pdf.add_stream(b"", RNG.randbytes(t))
                for t in L.pop("-|-|raw", [])]

    extras = generic_leftovers(pdf, L)
    assert not L, L

    # object 2: the killer shared resources dict listing every TPL
    xobj_entries = b"".join(b"/TPL%d %d 0 R" % (i, tpls[i]) for i in range(NPAGES))
    pdf.put(res_obj, b"<</ProcSet[/PDF/Text/ImageB/ImageC/ImageI]"
            b"/Font<</F1 %d 0 R>>/XObject<<%s>>>>" % (f1, xobj_entries))

    # outlines: 2,473 items, depth 3 (1 top + L2 + L3)
    NOUT = 2473
    out_root = pdf.reserve()
    top = pdf.reserve()
    rest = NOUT - 1
    n2 = 246
    l2 = [pdf.reserve() for _ in range(n2)]
    l3_counts = []
    base_kids, extra_kid = divmod(rest - n2, n2)
    for j in range(n2):
        l3_counts.append(base_kids + (1 if j < extra_kid else 0))
    l3 = [[pdf.reserve() for _ in range(c)] for c in l3_counts]
    assert 1 + n2 + sum(l3_counts) == NOUT

    pdf.put(out_root, b"<</Type/Outlines/First %d 0 R/Last %d 0 R>>" % (top, top))
    pdf.put(top, b"<</Title(PROCESSO 0000000-00.0000.4.00.0000/XX - Parte 1)"
            b"/Parent %d 0 R/First %d 0 R/Last %d 0 R"
            b"/Dest[%d 0 R/XYZ 0 841.89 null]/Count 0>>"
            % (out_root, l2[0], l2[-1], page_nums[0]))
    for j, n in enumerate(l2):
        pg = page_nums[(j * NPAGES) // n2]
        prev = b"/Prev %d 0 R" % l2[j - 1] if j else b""
        nxt = b"/Next %d 0 R" % l2[j + 1] if j + 1 < n2 else b""
        kids = (b"/First %d 0 R/Last %d 0 R/Count 0"
                % (l3[j][0], l3[j][-1])) if l3[j] else b"/Count 0"
        pdf.put(n, b"<</Title(Evento %d - Documento sintetico)/Parent %d 0 R"
                b"/Dest[%d 0 R/XYZ 0 841.89 null]%s%s%s>>"
                % (j + 1, top, pg, kids, prev, nxt))
        for k, m in enumerate(l3[j]):
            pg2 = page_nums[((j * NPAGES) // n2 + k) % NPAGES]
            prev2 = b"/Prev %d 0 R" % l3[j][k - 1] if k else b""
            nxt2 = b"/Next %d 0 R" % l3[j][k + 1] if k + 1 < len(l3[j]) else b""
            pdf.put(m, b"<</Title(ANEXO%d - PECA%d)/Parent %d 0 R"
                    b"/Dest[%d 0 R/XYZ 0 841.89 null]%s%s>>"
                    % (k + 1, j + 1, n, pg2, prev2, nxt2))

    info = pdf.add(
        b"<</Producer(FPDF 1.86)"
        b"/Author<54524634202D20496E666F726DC3A174696361>"
        b"/Subject(Documento Unificado)/Title(Documento Unificado)"
        b"/CreationDate(D:20260101120000-03'00')>>")
    filler_needed = TARGET_OBJS - pdf.count - 2  # anchor array + catalog
    assert filler_needed >= 0, filler_needed
    filler = [pdf.add(b"[%d 0 R/XYZ 0 841.89 null]"
                      % page_nums[(k * 991) % NPAGES])
              for k in range(filler_needed)]
    anchor = pdf.add(b"[" + b" ".join(
        b"%d 0 R" % x for x in (filler + misc_raw + extras + idx_cs
                                + pattern_streams + imgs)) + b"]")
    catalog = pdf.add(b"<</Type/Catalog/Pages %d 0 R/Outlines %d 0 R"
                      b"/PageMode/UseOutlines"
                      b"/PieceInfo<</PDQfill<</Private %d 0 R>>>>>>"
                      % (pages_obj, out_root, anchor))

    kids = b" ".join(b"%d 0 R" % p for p in page_nums)
    pdf.put(pages_obj,
            b"<</Type/Pages/Kids[%s]/Count %d/MediaBox[0 0 595.28 841.89]>>"
            % (kids, NPAGES))

    assert pdf.count == TARGET_OBJS, pdf.count
    pdf.write(out_path, b"<<\n/Size @SIZE@\n/Root %d 0 R\n/Info %d 0 R\n>>"
              % (catalog, info))


if __name__ == "__main__":
    out_a = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("anon-pje-like-12732p.pdf")
    out_b = Path(sys.argv[2]) if len(sys.argv) > 2 else Path("anon-trf4-like-2642p.pdf")
    print("building B (TRF4-like, 2642p)...")
    build_trf4(out_b)
    print("  ->", out_b, out_b.stat().st_size, "bytes")
    print("building A (PJe-like, 12732p)...")
    build_pje(out_a)
    print("  ->", out_a, out_a.stat().st_size, "bytes")
