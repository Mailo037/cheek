"""Renders docs/releases/v0.2.0.png, the picture on top of the 0.2.0 release notes, in
the same look as docs/hero.png.

    python docs/render_release.py --fonts ../Hamstr/app/src/main/res/font
"""
import argparse
import pathlib
import random

from PIL import Image, ImageDraw, ImageFilter, ImageFont

CANVAS, OVERLAY, LINE = (20, 20, 20), (30, 30, 30), (54, 54, 54)
INK, MUTED, GHOST, ACCENT, GREEN = (238, 238, 238), (170, 170, 170), (138, 138, 138), (255, 154, 60), (16, 185, 129)
VERSION = "0.2.0"
FEATURES = [
    ("COVERS + TAGS", "--embed-thumbnail, --embed-metadata"),
    ("FRAGMENT RESUME", "HLS and DASH continue where they stopped"),
    ("PICK FROM JSON", "-f works on --info-json, no second yt-dlp run"),
]


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

    rnd = random.Random(11)
    layer = Image.new("RGBA", (W, H), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    mono = f("jetbrains_mono_400.ttf", 24)
    for y in range(0, 420, 30):
        k = (1 - y / 420) ** 1.4
        for x in range(0, W, 30):
            if rnd.random() > 0.5 * k:
                continue
            ch = " .:-=+*#"[1 + int(k * 6 * rnd.uniform(0.3, 1.1)) % 7]
            col = ACCENT if rnd.random() < 0.05 else INK
            d.text((x + 15, y + 15), ch, font=mono, fill=col + (int(10 + 46 * k),), anchor="mm")
    img.alpha_composite(layer)

    d = ImageDraw.Draw(img)
    d.text((112, 150), "cheek", font=f("jetbrains_mono_600.ttf", 150), fill=INK)
    d.text((120, 345), f"[ v{VERSION} ]", font=f("jetbrains_mono_600.ttf", 44), fill=ACCENT)
    d.text((118, 440), "Covers in every file,", font=f("inter_500.ttf", 54), fill=INK)
    d.text((118, 510), "downloads that pick up again.", font=f("inter_500.ttf", 54), fill=INK)
    y = 650
    for title, sub in FEATURES:
        d.text((118, y), "*", font=f("jetbrains_mono_600.ttf", 30), fill=ACCENT)
        d.text((160, y), title, font=f("jetbrains_mono_600.ttf", 30), fill=INK)
        d.text((160, y + 44), sub, font=f("inter_400.ttf", 32), fill=MUTED)
        y += 120

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
        [("$ ", GHOST), ("yt-dlp -J URL > info.json", INK)],
        [("$ ", GHOST), ("cheek ", ACCENT), ("--info-json info.json \\", INK)],
        [("    -f ", INK), ('"bv*[height<=720]+ba"', ACCENT), (" --embed-thumbnail", INK)],
        [("[Youtube] ", GHOST), ("Caminandes 3: Llamigos  (2 streams)", MUTED)],
        [("[cheek] ", GHOST), ("720p + opus, picked from the JSON", INK)],
        [("[cheek] ", GHOST), ("tagging", INK), ("  cover, title, artist", MUTED)],
        [("        -> ", GHOST), ("Caminandes 3 [SkVqJ1SGeL0].mp4", GREEN)],
        [],
        [("$ ", GHOST), ("cheek ", ACCENT), ("https://…/master.m3u8", INK)],
        [("[cheek] ", GHOST), ("resuming at fragment 412/980", INK)],
        "BAR",
    ]
    y = py + 100
    for parts in lines:
        if parts == "BAR":
            bx, bw = px + 44, 440
            d.text((bx, y), "*", font=tb, fill=ACCENT)
            d.rounded_rectangle((bx + 40, y + 12, bx + 40 + bw, y + 22), 5, fill=LINE)
            d.rounded_rectangle((bx + 40, y + 12, bx + 40 + int(bw * 0.42), y + 22), 5, fill=ACCENT)
            d.text((bx + 40 + bw + 24, y), "412/980  18.1 MiB/s", font=tf, fill=MUTED)
            y += 50
            continue
        x = px + 44
        for text, col in parts:
            d.text((x, y), text, font=tb if col == ACCENT else tf, fill=col)
            x += d.textlength(text, font=tf)
        y += 50

    out = pathlib.Path(__file__).parent / "releases" / f"v{VERSION}.png"
    out.parent.mkdir(exist_ok=True)
    img.convert("RGB").save(out, optimize=True)
    print(out)


if __name__ == "__main__":
    main()
