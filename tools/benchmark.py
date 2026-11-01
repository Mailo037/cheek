"""Repeatable range/fragment benchmark; optional real URL for HTTP/2 and FFmpeg tests.
Run: python tools/benchmark.py --binary target/release/cheek.exe --runs 3
Optional: --baseline old-cheek.exe --url URL --extra-arg=--embed-metadata
CPU/RSS sampling uses psutil when installed. Energy needs Android batterystats.
"""
import argparse
import hashlib
import http.server
import json
import subprocess
import tempfile
import threading
import time
from pathlib import Path

try:
    import psutil
except ImportError:
    psutil = None

BLOCK = bytes(range(256)) * 4096
DATA = b"".join(bytes([i]) * len(BLOCK) for i in range(24))

class Fixture(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_GET(self):
        is_page = self.path == "/page"
        if self.path in ("/page", "/resolved-song.mp3"):
            time.sleep(0.08)  # Controlled request latency makes redundant resolution visible.
        if is_page:
            body = b'<title>Resolved song</title><meta property="og:audio" content="/resolved-song.mp3">'
        elif self.path == "/resolved-song.mp3":
            body = DATA[:4 * 1024 * 1024]
        elif self.path.startswith("/segment/"):
            i = int(self.path.rsplit("/", 1)[1])
            time.sleep(0.6 if i == 0 else 0.025)
            body = bytes([i]) * len(BLOCK)
        else:
            body = DATA
        start, end = 0, len(body) - 1
        ranged = None if is_page else self.headers.get("Range")
        if ranged:
            start, end = map(int, ranged.removeprefix("bytes=").split("-"))
            end = min(end, len(body) - 1)
        self.send_response(206 if ranged else 200)
        self.send_header("Content-Type", "text/html" if is_page else "audio/mpeg")
        if ranged:
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(body)}")
        self.send_header("Content-Length", str(end - start + 1))
        self.end_headers()
        try:
            for at in range(start, end + 1, 65536):
                self.wfile.write(body[at:min(at + 65536, end + 1)])
                self.wfile.flush()
                time.sleep(0.001)
        except (ConnectionError, OSError):
            pass


def measure(binary, flags, fixture, url, extra, folder, expected=None):
    args = [str(binary), "--json", "-P", str(folder), "-o", "result.%(ext)s"]
    args += (["--", url] if url else ["--info-json", str(fixture)])
    # Options must precede the positional -- terminator.
    args = args[:1] + flags + extra + args[1:]
    began = time.perf_counter()
    events = []
    peak_rss = cpu = 0
    with (folder / "stderr.txt").open("w") as stderr:
        proc = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8")
        def read():
            for line in proc.stdout:
                try:
                    events.append((time.perf_counter() - began, json.loads(line)))
                except ValueError:
                    pass
        reader = threading.Thread(target=read, daemon=True)
        reader.start()
        while proc.poll() is None:
            if time.perf_counter() - began > 180:
                if psutil:
                    try:
                        for child in psutil.Process(proc.pid).children(recursive=True):
                            child.kill()
                    except psutil.Error:
                        pass
                proc.kill()
                proc.wait()
                break
            if psutil:
                try:
                    processes = [psutil.Process(proc.pid)]
                    processes += processes[0].children(recursive=True)
                    peak_rss = max(peak_rss, sum(p.memory_info().rss for p in processes))
                    cpu = max(cpu, sum(p.cpu_times().user + p.cpu_times().system for p in processes))
                except psutil.Error:
                    pass
            time.sleep(0.02)
        reader.join(timeout=5)
    elapsed = time.perf_counter() - began
    info = next((t for t, e in events if e.get("type") == "info"), None)
    post = next((t for t, e in events if e.get("type") == "stage"), None)
    files = list(folder.glob("result.*"))
    exact = None
    if expected is not None:
        exact = len(files) == 1 and hashlib.sha256(files[0].read_bytes()).digest() == hashlib.sha256(expected).digest()
    return dict(exit=proc.returncode, seconds=elapsed, resolve_seconds=info,
                phase_seconds={e["phase"]: e["seconds"] for _, e in events if e.get("type") == "timing"},
                transfer_seconds=(post or elapsed) - (info or 0),
                postprocess_seconds=elapsed - post if post else 0,
                cpu_seconds_sampled=cpu if psutil else None,
                peak_rss_bytes_sampled=peak_rss if psutil else None, byte_exact=exact)


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--binary", required=True, type=Path)
    ap.add_argument("--baseline", type=Path)
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--url")
    ap.add_argument("--resolve-fixture", action="store_true", help="resolve an HTML page pointing to a 4 MiB file with controlled request latency")
    ap.add_argument("--extra-arg", action="append", default=[])
    ap.add_argument("--report", type=Path, default=Path("benchmark-results.json"))
    opt = ap.parse_args()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f"http://127.0.0.1:{server.server_port}"
    modes = [("fixed-http1", ["--fixed-connections"]), ("adaptive-http1", []),
             ("adaptive-auto", ["--http-protocol", "auto"]), ("power-save", ["--power-save"])]
    if opt.baseline:
        modes.insert(0, ("baseline", []))
    report = []
    try:
        with tempfile.TemporaryDirectory(prefix="cheek-bench-") as tmp:
            for scenario in (["resolve"] if opt.resolve_fixture else ["remote"] if opt.url else ["ranges", "fragments"]):
                stream = dict(url=base + "/file", ext="bin", kind="muxed", protocol="http")
                if scenario == "fragments":
                    stream.update(protocol="fragments", fragments=[base + f"/segment/{i}" for i in range(24)])
                fixture = Path(tmp) / "info.json"
                fixture.write_text(json.dumps(dict(id="bench", title="bench", extractor="fixture",
                                                  webpage_url=base, streams=[stream])), encoding="utf-8")
                for mode, flags in modes:
                    for run in range(max(1, opt.runs)):
                        folder = Path(tmp) / f"{scenario}-{mode}-{run}"
                        folder.mkdir()
                        result = measure((opt.baseline if mode == "baseline" else opt.binary).resolve(),
                                         flags, fixture, base + "/page" if scenario == "resolve" else opt.url, opt.extra_arg, folder,
                                         expected=DATA[:4 * 1024 * 1024] if scenario == "resolve" else DATA if not opt.url else None)
                        result.update(scenario=scenario, mode=mode, run=run)
                        report.append(result)
                        print(json.dumps(result), flush=True)
    finally:
        server.shutdown()
        opt.report.write_text(json.dumps(report, indent=2), encoding="utf-8")
    if any(r["exit"] or r["byte_exact"] is False for r in report):
        raise SystemExit(1)

if __name__ == "__main__":
    main()
