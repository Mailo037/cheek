"""Renders docs/hero.png in Hamstr's Raster look: the hamster, its cheeks lit up, and a
terminal showing a download.

    python docs/render_hero.py --fonts ../Hamstr/app/src/main/res/font
"""
import argparse
import pathlib
import random

from PIL import Image, ImageDraw, ImageFilter, ImageFont

GRID = [
    "...###....###...",
    "..#####..#####..",
    "...##########...",
    "..############..",
    ".##############.",
    ".##############.",
    "####0######0####",
    "################",
    "#***########***#",
    ".***########***.",
    "..############..",
    "....########....",
]
CANVAS, OVERLAY, SURFACE, LINE = (20, 20, 20), (30, 30, 30), (38, 38, 38), (54, 54, 54)
INK, MUTED, GHOST, ACCENT, GREEN = (238, 238, 238), (170, 170, 170), (138, 138, 138), (255, 154, 60), (16, 185, 129)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--fonts", default="../Hamstr/app/src/main/res/font")
    a = ap.parse_args()
    fonts = pathlib.Path(a.fonts)
    f = lambda name, size: ImageFont.truetype(str(fonts / name), size)

    W, H = 2400, 1100
    img = Image.new("RGBA", (W, H), CANVAS + (255,))

    glow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse((-300, -500, 1100, 700), fill=ACCENT + (40,))
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(220)))

    # Character raster fading out downwards, kept away from the mark.
    rnd = random.Random(5)
    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    mono = f("jetbrains_mono_400.ttf", 24)
    for y in range(0, 520, 30):
        k = (1 - y / 520) ** 1.4
        for x in range(0, W, 30):
            if 90 < x < 560 and 90 < y < 470:
                continue
            if rnd.random() > 0.5 * k:
                continue
            ch = " .:-=+*#"[1 + int(k * 6 * rnd.uniform(0.3, 1.1)) % 7]
            col = ACCENT if rnd.random() < 0.05 else INK
            d.text((x + 15, y + 15), ch, font=mono, fill=col + (int(10 + 46 * k),), anchor="mm")
    img.alpha_composite(layer)

    d = ImageDraw.Draw(img)
    # The hamster, with the cheeks doing the work.
    cell = 27
    mf = f("jetbrains_mono_600.ttf", 31)
    ox, oy = 110, 120
    for r, line in enumerate(GRID):
        for c, ch in enumerate(line):
            if ch == ".":
                continue
            d.text((ox + c * cell + cell / 2, oy + r * cell * 1.1 + cell * 0.55), ch, font=mf, fill=ACCENT if ch == "*" else INK, anchor="mm")

    d.text((112, 520), "cheek", font=f("jetbrains_mono_600.ttf", 150), fill=INK)
    d.text((118, 720), "Fast media downloads in Rust.", font=f("inter_500.ttf", 54), fill=INK)
    d.text((118, 802), "Parallel ranges, parallel HLS, resumable,", font=f("inter_400.ttf", 36), fill=MUTED)
    d.text((118, 850), "ffmpeg merge. yt-dlp compatible.", font=f("inter_400.ttf", 36), fill=MUTED)
    bf = f("jetbrains_mono_600.ttf", 28)
    x = 118
    for label, hot in (("4 MB binary", False), ("1.7x on archive.org", True), ("android", False)):
        t = f"[ {label.upper()} ]  "
        d.text((x, 950), t, font=bf, fill=ACCENT if hot else GHOST)
        x += d.textlength(t, font=bf)

    # Terminal panel.
    px, py, pw, ph = 1180, 170, 1120, 760
    shadow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    ImageDraw.Draw(shadow).rounded_rectangle((px + 20, py + 40, px + pw + 20, py + ph + 40), 32, fill=(0, 0, 0, 160))
    img.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(40)))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((px, py, px + pw, py + ph), 32, fill=OVERLAY + (255,), outline=LINE + (255,), width=2)
    for i, col in enumerate(((255, 95, 87), (254, 188, 46), (40, 200, 64))):
        d.ellipse((px + 34 + i * 34, py + 30, px + 54 + i * 34, py + 50), fill=col)
    tf = f("jetbrains_mono_400.ttf", 27)
    tb = f("jetbrains_mono_600.ttf", 27)
    lines = [
        [("$ ", GHOST), ("cheek ", ACCENT), ("https://archive.org/download/Sintel/…", INK)],
        [("[Generic] ", GHOST), ("sintel-2048-surround  (1 stream, 296 MiB)", MUTED)],
        [],
        "BAR",
        [],
        [("[cheek] ", GHOST), ("296.3 MiB in 13.3 s (22.3 MiB/s)", INK)],
        [("        -> ", GHOST), ("sintel-2048-surround [h3k1…].mp4", GREEN)],
        [],
        [("$ ", GHOST), ("yt-dlp -J URL ", INK), ("| ", GHOST), ("cheek --info-json -", ACCENT)],
        [("[Youtube] ", GHOST), ("Caminandes 3: Llamigos  (2 streams, 25 MiB)", MUTED)],
        [("[cheek] ", GHOST), ("merging", INK)],
    ]
    y = py + 100
    for parts in lines:
        if parts == "BAR":
            bx, bw = px + 44, 440
            d.text((bx, y), "*", font=tb, fill=ACCENT)
            d.rounded_rectangle((bx + 40, y + 12, bx + 40 + bw, y + 22), 5, fill=LINE)
            d.rounded_rectangle((bx + 40, y + 12, bx + 40 + int(bw * 0.72), y + 22), 5, fill=ACCENT)
            d.text((bx + 40 + bw + 24, y), "213/296 MiB  22.4 MiB/s", font=tf, fill=MUTED)
            y += 50
            continue
        x = px + 44
        for text, col in parts:
            d.text((x, y), text, font=tb if col == ACCENT else tf, fill=col)
            x += d.textlength(text, font=tf)
        y += 50

    out = pathlib.Path(__file__).parent / "hero.png"
    img.convert("RGB").save(out, optimize=True)
    print(out)


if __name__ == "__main__":
    main()
