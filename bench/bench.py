"""Times yt-dlp against cheek on the same links and the same formats.

    python bench/bench.py --yt-dlp yt-dlp --ffmpeg ffmpeg --cheek target/release/cheek [--runs 2]

For sites yt-dlp has to resolve, cheek runs as `yt-dlp -J ... | cheek --info-json -`, so
both sides pay for the same extraction and the difference is the download itself.
Writes a Markdown table to bench/RESULTS.md.
"""
import argparse
import hashlib
import json
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import time

CASES = [
    {
        "name": "Sintel 1080p, direct file (1.1 GiB)",
        "url": "https://download.blender.org/durian/movies/Sintel.2010.1080p.mkv",
        "format": "b",
        "md5_url": "https://download.blender.org/durian/movies/Sintel.2010.1080p.mkv.md5",
        "native": True,
        # Python's TLS stack trips over an expired certificate in this server's chain; curl
        # and cheek (rustls) don't. Only switched off for yt-dlp, and only here.
        "ytdlp_args": ["--no-check-certificates"],
    },
    {
        # archive.org sits across the Atlantic from here and paces each connection.
        "name": "Sintel 2K, archive.org (296 MiB)",
        "url": "https://archive.org/download/Sintel/sintel-2048-surround.mp4",
        "format": "b",
        "native": True,
        "ytdlp_args": ["--no-check-certificates"],  # same certificate trouble as above
    },
    {
        "name": "Caminandes 3, YouTube 1080p + audio",
        "url": "https://www.youtube.com/watch?v=SkVqJ1SGeL0",
        "format": "bv*[height<=1080][ext=mp4]+ba[ext=m4a]/b",
    },
    {
        "name": "Sintel, YouTube 1080p + audio",
        "url": "https://www.youtube.com/watch?v=eRsGyueVLvQ",
        "format": "bv*[height<=1080][ext=mp4]+ba[ext=m4a]/b",
    },
]


def run(cmd, **kw):
    t = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, **kw)
    if p.returncode != 0:
        sys.exit(f"failed: {' '.join(map(str, cmd))}\n{p.stderr.decode(errors='replace')[-2000:]}")
    return time.perf_counter() - t, p.stdout


def only_file(d: pathlib.Path) -> pathlib.Path:
    files = [f for f in d.iterdir() if f.is_file()]
    assert len(files) == 1, files
    return files[0]


def md5(path):
    h = hashlib.md5()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--yt-dlp", default="yt-dlp")
    ap.add_argument("--ffmpeg", default="ffmpeg")
    ap.add_argument("--cheek", default="target/release/cheek")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--only", help="substring of a case name")
    a = ap.parse_args()
    ytdlp_version = subprocess.run([a.yt_dlp, "--version"], capture_output=True, text=True).stdout.strip()
    cheek_version = subprocess.run([a.cheek, "--version"], capture_output=True, text=True).stdout.strip()

    rows = []
    for case in CASES:
        if a.only and a.only not in case["name"]:
            continue
        print(f"== {case['name']}", flush=True)
        times = {"yt-dlp": [], "cheek": [], "resolve": []}
        sizes = {}
        for _ in range(a.runs):
            with tempfile.TemporaryDirectory() as tmp:
                tmp = pathlib.Path(tmp)
                (tmp / "y").mkdir()
                (tmp / "c").mkdir()
                common = ["-f", case["format"], "--no-playlist", "--no-part", "--ffmpeg-location", a.ffmpeg, "-P", str(tmp / "y"), "-o", "%(id)s.%(ext)s"]
                dt, _ = run([a.yt_dlp, *common, *case.get("ytdlp_args", []), case["url"]])
                times["yt-dlp"].append(dt)
                yfile = only_file(tmp / "y")

                cheek = [a.cheek, "-q", "-P", str(tmp / "c"), "-o", "%(id)s.%(ext)s", "--ffmpeg", a.ffmpeg]
                if case.get("native"):
                    dt, _ = run([*cheek, case["url"]])
                    times["cheek"].append(dt)
                else:
                    rt, info = run([a.yt_dlp, "-J", "--no-playlist", "-f", case["format"], case["url"]])
                    dt, _ = run([*cheek, "--info-json", "-"], input=info)
                    times["resolve"].append(rt)
                    times["cheek"].append(rt + dt)
                cfile = only_file(tmp / "c")
                sizes = {"yt-dlp": yfile.stat().st_size, "cheek": cfile.stat().st_size}
                if case.get("md5_url"):
                    want = subprocess.run(["curl", "-sL", case["md5_url"]], capture_output=True, text=True).stdout.split()[0]
                    for who, f in (("yt-dlp", yfile), ("cheek", cfile)):
                        assert md5(f) == want, f"{who} file doesn't match the published MD5"
                    print("   md5 ok for both", flush=True)
            print(f"   yt-dlp {times['yt-dlp'][-1]:.1f} s   cheek {times['cheek'][-1]:.1f} s", flush=True)
        best = {k: min(v) for k, v in times.items() if v}
        mib = sizes["cheek"] / (1 << 20)
        rows.append((case["name"], mib, best["yt-dlp"], best["cheek"], best.get("resolve")))

    out = ["| Download | Size | yt-dlp | cheek | Speed-up |", "| --- | ---: | ---: | ---: | ---: |"]
    for name, mib, y, c, r in rows:
        note = f" (incl. {r:.1f} s yt-dlp -J)" if r else ""
        out.append(f"| {name} | {mib:.0f} MiB | {y:.1f} s | {c:.1f} s{note} | **{y / c:.1f}x** |")
    env = f"\n{platform.system()} {platform.release()}, {ytdlp_version and 'yt-dlp ' + ytdlp_version}, {cheek_version}, best of {a.runs} run(s), same network, files written to a temp folder.\n"
    text = "\n".join(out) + "\n" + env
    print(text)
    # A partial run (--only) doesn't replace the full table.
    (pathlib.Path(__file__).parent / ("RESULTS-partial.md" if a.only else "RESULTS.md")).write_text(text, encoding="utf-8")


if __name__ == "__main__":
    main()
