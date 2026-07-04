#!/usr/bin/env python3
"""Generate the pdq README benchmark SVG (light/dark via prefers-color-scheme)."""
import sys

W = 920
PAD = 40
LABEL_W = 74            # tool-name column
VALUE_ROOM = 128        # right-side room for tip labels
BAR_H = 14
ROW_PITCH = 26
PANEL_TITLE_H = 30
PANEL_GAP = 26
BAR_X = PAD + LABEL_W
PLOT_W = W - BAR_X - PAD - VALUE_ROOM

# tool, ms (None = timeout), note; cap = per-panel axis max in ms
PANELS = [
    dict(title="Page count", sub="12,732 pages · 200 MB", cap=22.0, rows=[
        ("pdq", 6.1, "6.1 ms", None),
        ("qpdf", 14.5, "14.5 ms", "2.4×"),
        ("Poppler", 20.5, "20.5 ms", "3.4×"),
        ("MuPDF", 1288, "1.29 s", "211×"),
    ]),
    dict(title="Split into single pages", sub="12,732 pages · 200 MB", cap=4940.0, rows=[
        ("pdq", 1050, "1.05 s", None),
        ("qpdf", 4940, "4.94 s", "4.7×"),
        ("Poppler", None, "&#215;  timeout &#8212; 6 of 12,732 files after 120 s", None),
    ]),
    dict(title="Split into single pages", sub="2,642 pages · 26 MB", cap=2000.0, rows=[
        ("pdq", 280, "280 ms", None),
        ("qpdf", None, "&#215;  timeout &#8212; 1,295 of 2,642 files after 120 s", None),
        ("Poppler", None, "&#215;  timeout &#8212; 113 of 2,642 files after 120 s", None),
    ]),
    dict(title="Extract pages 5000–5100", sub="from 12,732 pages", cap=356.0, rows=[
        ("pdq", 37.3, "37 ms", None),
        ("MuPDF", 60.0, "60 ms", "1.6×"),
        ("qpdf", 355.4, "355 ms", "9.5×"),
    ]),
    dict(title="Full rewrite", sub="2,642 pages", cap=136.3, rows=[
        ("pdq", 86.6, "87 ms", None),
        ("MuPDF", 115.9, "116 ms", "1.3×"),
        ("qpdf", 136.3, "136 ms", "1.6×"),
    ]),
    dict(title="Full rewrite", sub="12,732 pages · pdq peak heap 43 MB", cap=746.8, rows=[
        ("MuPDF", 506.7, "507 ms", None),
        ("pdq", 618.7, "619 ms", "1.2×"),
        ("qpdf", 746.8, "747 ms", "1.5×"),
    ]),
    dict(title="Merge", sub="12,732 + 2,642 pages", cap=9450.0, rows=[
        ("pdq", 830, "0.83 s", None),
        ("qpdf", 1421, "1.42 s", "1.7×"),
        ("MuPDF", 9446, "9.45 s", "11.4×"),
        ("Poppler", 24820, "24.8 s", "30×"),
    ]),
]

HEADER_H = 96
FOOTER_H = 104

def panel_height(p):
    return PANEL_TITLE_H + len(p["rows"]) * ROW_PITCH

H = PAD + HEADER_H + sum(panel_height(p) for p in PANELS) \
    + PANEL_GAP * (len(PANELS) - 1) + FOOTER_H

def bar_path(x, y, w, h, r=4):
    w = max(w, 3.0)
    r = min(r, w / 2, h / 2)
    return (f"M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} "
            f"v{h - 2 * r:.1f} a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} Z")

out = []
out.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
    f'width="{W}" height="{H}" role="img" '
    f'aria-label="pdq benchmark results against qpdf, MuPDF and Poppler">')
out.append(f"""<title>pdq benchmarks — real-world PDFs</title>
<style>
  text {{ font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }}
  .surface {{ fill: #fcfcfb; }}
  .border  {{ stroke: rgba(11,11,11,0.10); }}
  .ink1  {{ fill: #0b0b0b; }}
  .ink2  {{ fill: #52514e; }}
  .muted {{ fill: #898781; }}
  .accent {{ fill: #2a78d6; }}
  .gray   {{ fill: #c3c2b7; }}
  .inbar  {{ fill: #52514e; }}
  .hatchline {{ stroke: #b5b3ab; }}
  .hatchbg   {{ fill: #f0efec; }}
  .crit  {{ fill: #d03b3b; }}
  .break {{ stroke: #fcfcfb; }}
  .rule  {{ stroke: #e1e0d9; }}
  @media (prefers-color-scheme: dark) {{
    .surface {{ fill: #1a1a19; }}
    .border  {{ stroke: rgba(255,255,255,0.10); }}
    .ink1  {{ fill: #ffffff; }}
    .ink2  {{ fill: #c3c2b7; }}
    .muted {{ fill: #898781; }}
    .accent {{ fill: #3987e5; }}
    .gray   {{ fill: #52514e; }}
    .inbar  {{ fill: #c3c2b7; }}
    .hatchline {{ stroke: #52514e; }}
    .hatchbg   {{ fill: #242423; }}
    .crit  {{ fill: #e66767; }}
    .break {{ stroke: #1a1a19; }}
    .rule  {{ stroke: #2c2c2a; }}
  }}
</style>
<defs>
  <pattern id="hatch" patternUnits="userSpaceOnUse" width="7" height="7" patternTransform="rotate(45)">
    <line x1="0" y1="0" x2="0" y2="7" stroke-width="1.6" class="hatchline"/>
  </pattern>
</defs>""")

out.append(f'<rect class="surface border" x="0.5" y="0.5" width="{W-1}" height="{H-1}" rx="14" stroke-width="1"/>')

y = PAD + 6
out.append(f'<text class="ink1" x="{PAD}" y="{y + 14}" font-size="19" font-weight="650">pdq benchmarks</text>')
out.append(f'<text class="ink2" x="{PAD}" y="{y + 40}" font-size="13">'
           f'Real-world court PDFs &#183; pdq <tspan class="muted">vs</tspan> qpdf, MuPDF (mutool) and Poppler &#183; lower is better</text>')
sw = 9
out.append(f'<rect class="accent" x="{W - PAD - 52}" y="{y + 6}" width="{sw}" height="{sw}" rx="2"/>')
out.append(f'<text class="ink2" x="{W - PAD - 38}" y="{y + 14}" font-size="12">pdq</text>')

y = PAD + HEADER_H
for i, p in enumerate(PANELS):
    out.append(f'<text class="ink1" x="{PAD}" y="{y + 12}" font-size="13.5" font-weight="600">{p["title"]}'
               f'<tspan class="muted" font-weight="400" dx="10">{p["sub"]}</tspan></text>')
    by = y + PANEL_TITLE_H
    for tool, ms, label, mult in p["rows"]:
        cy = by + BAR_H / 2
        out.append(f'<text class="ink2" x="{BAR_X - 12}" y="{cy + 4}" font-size="12" text-anchor="end">{tool}</text>')
        if ms is None:
            bw = PLOT_W + VALUE_ROOM - 12
            out.append(f'<rect class="hatchbg" x="{BAR_X}" y="{by}" width="{bw:.1f}" height="{BAR_H}" rx="4"/>')
            out.append(f'<rect fill="url(#hatch)" x="{BAR_X}" y="{by}" width="{bw:.1f}" height="{BAR_H}" rx="4"/>')
            out.append(f'<text class="ink2" x="{BAR_X + 10}" y="{cy + 4}" font-size="11.5">{label}</text>')
        else:
            frac = ms / p["cap"]
            if frac <= 1.001:
                bw = frac * PLOT_W
                cls = "accent" if tool == "pdq" else "gray"
                out.append(f'<path class="{cls}" d="{bar_path(BAR_X, by, bw, BAR_H)}"/>')
                weight = "650" if mult is None else "400"
                cls_v = "ink1" if mult is None else "ink2"
                tip = f'<text class="{cls_v}" x="{BAR_X + max(bw,3) + 10}" y="{cy + 4}" font-size="12" font-weight="{weight}">{label}'
                if mult:
                    tip += f'<tspan class="muted" font-weight="400" dx="5">&#183;</tspan><tspan class="muted" font-weight="400" dx="5">{mult}</tspan>'
                tip += '</text>'
                out.append(tip)
            else:
                bw = PLOT_W + VALUE_ROOM - 12
                out.append(f'<path class="gray" d="{bar_path(BAR_X, by, bw, BAR_H)}"/>')
                bx = BAR_X + bw - 46
                for dx in (0, 7):
                    out.append(f'<line class="break" x1="{bx+dx}" y1="{by-2}" x2="{bx+dx-6}" y2="{by+BAR_H+2}" stroke-width="3.5"/>')
                tip = f'<text class="inbar" x="{BAR_X + bw - 58}" y="{cy + 4}" font-size="11.5" text-anchor="end" font-weight="500">{label}'
                if mult:
                    tip += f'<tspan font-weight="400" dx="5">&#183;</tspan><tspan font-weight="400" dx="5">{mult}</tspan>'
                tip += '</text>'
                out.append(tip)
        by += ROW_PITCH
    y = by + (PANEL_GAP if i < len(PANELS) - 1 else 0)

fy = H - FOOTER_H + 10
out.append(f'<line class="rule" x1="{PAD}" y1="{fy}" x2="{W - PAD}" y2="{fy}" stroke-width="1"/>')
FOOT_COLS = [
    ("Method", ["hyperfine mean &#183; warmup 1, 5 runs", "count &amp; rewrites: 10 runs &#183; 120 s timeout"]),
    ("Validation", ["every output checked with qpdf --check", "bars scaled per scenario &#183; axis breaks marked"]),
    ("Environment", ["Apple M4 Max &#183; qpdf 12.3.2 &#183; MuPDF 1.28.0", "Poppler 26.02 &#183; macOS &#183; 2026-07-04"]),
]
col_w = (W - 2 * PAD) / len(FOOT_COLS)
for ci, (label, lines) in enumerate(FOOT_COLS):
    cx = PAD + ci * col_w
    out.append(f'<text class="muted" x="{cx:.0f}" y="{fy + 24}" font-size="9.5" font-weight="600" '
               f'letter-spacing="0.9" style="text-transform:uppercase">{label.upper()}</text>')
    for li, line in enumerate(lines):
        out.append(f'<text class="ink2" x="{cx:.0f}" y="{fy + 44 + li * 17}" font-size="11">{line}</text>')
out.append('</svg>')

svg = "\n".join(out)
path = sys.argv[1] if len(sys.argv) > 1 else "benchmark.svg"
with open(path, "w") as f:
    f.write(svg)
print(f"{path}: {len(svg)} bytes, {W}x{H}")
