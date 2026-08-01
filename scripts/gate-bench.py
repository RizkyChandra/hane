#!/usr/bin/env python3
"""Run the P3 gate benchmark (#31) in real browsers and print markdown.

    python3 scripts/gate-bench.py [--browser chromium] [--browser firefox]

Expects `web/dist` to already contain `bench.html` and `hane.wasm`:

    cargo build --release --target wasm32-unknown-unknown -p hane-wasm
    mkdir -p web/public
    cp target/wasm32-unknown-unknown/release/hane_wasm.wasm web/public/hane.wasm
    cd web && npm ci && npm run build

No webdriver. The browser is launched headless at a local URL, the page runs
itself and POSTs its results back here. That is one moving part instead of
three, and it works for Firefox on a machine with no geckodriver.

The one header that matters is cross-origin isolation: without COOP/COEP,
Firefox clamps `performance.now()` to 1 ms, which would quantise a 0.2 ms
phase into "0 or 1" and turn every percentile below into fiction. The page
reports the resolution it actually measured so the reader can check.
"""

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
import threading
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / "web" / "dist"

# Chromium is the only one of the two that needs coaxing: headless Chrome
# throttles background timers and, without a GPU, falls back to SwiftShader,
# which is irrelevant here (nothing draws) but slow to start.
LAUNCH = {
    "chromium": ["--headless=new", "--no-sandbox", "--disable-gpu",
                 "--disable-background-timer-throttling",
                 "--disable-renderer-backgrounding", "--user-data-dir={profile}"],
    "firefox": ["--headless", "--profile", "{profile}"],
}

# Belt and braces against the timer clamp: COOP/COEP should already have lifted
# it, but a Firefox that ignored the headers would otherwise report a plausible
# looking table of quantised nonsense.
FIREFOX_PREFS = 'user_pref("privacy.reduceTimerPrecision", false);\n'


class Handler(SimpleHTTPRequestHandler):
    """Serves `web/dist` cross-origin isolated, and collects one POST."""

    result: dict | None = None
    done = threading.Event()

    def __init__(self, *a, **kw):
        super().__init__(*a, directory=str(DIST), **kw)

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        super().end_headers()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        Handler.result = json.loads(body)
        self.send_response(204)
        self.end_headers()
        Handler.done.set()

    def log_message(self, *_):
        pass


def run(browser: str, exe: str, timeout: float) -> dict:
    Handler.result, Handler.done = None, threading.Event()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/bench.html"
    print(f"# {browser}: {url}", file=sys.stderr)

    with tempfile.TemporaryDirectory() as profile:
        if browser == "firefox":
            (Path(profile) / "user.js").write_text(FIREFOX_PREFS)
        argv = [exe] + [a.format(profile=profile) for a in LAUNCH[browser]] + [url]
        proc = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            if not Handler.done.wait(timeout):
                raise TimeoutError(f"{browser} did not report within {timeout:.0f}s")
        finally:
            proc.terminate()
            proc.wait(timeout=30)
            server.shutdown()
    return Handler.result


def markdown(browser: str, report: dict) -> str:
    lines = [
        f"### {browser}",
        "",
        f"`{report['userAgent']}`",
        "",
        f"{report['frames']} frames per row, first {report['warmup']} discarded. "
        f"Viewport {report['width']}x{report['height']}. "
        f"`performance.now()` resolution {report['timerResolutionMs']:.4f} ms "
        f"(cross-origin isolated: {report['crossOriginIsolated']}).",
        "",
        "| n | script | visible | tiles | miss/frame | hit rate | cull p99 | encode p99 |"
        " **frame p99** | raster p99 | cold frame |",
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for r in report["rows"]:
        lines.append(
            f"| {r['n']} | {r['script']} | {r['visible']:.0f} | {r['tiles']} |"
            f" {r['missesPerFrame']:.2f} | {r['hitRate'] * 100:.1f}% |"
            f" {r['cull']['p99']:.3f} | {r['encode']['p99']:.3f} |"
            f" **{r['frame']['p99']:.3f}** | {r['raster']['p99']:.1f} |"
            f" {r['coldFrameMs']:.2f} |"
        )
    lines.append("")
    lines.append("All times in milliseconds. **frame** is `cull + encode`: the CPU cost of a")
    lines.append("GPU frame with the GPU removed, because P2 does not exist yet.")
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--browser", action="append", choices=sorted(LAUNCH),
                    help="repeatable; default is every one found on PATH")
    ap.add_argument("--timeout", type=float, default=3600.0)
    args = ap.parse_args()

    if not (DIST / "bench.html").is_file() or not (DIST / "hane.wasm").is_file():
        print(f"{DIST} has no bench.html/hane.wasm -- see this script's docstring", file=sys.stderr)
        return 1

    wanted = args.browser or sorted(LAUNCH)
    found = [(b, shutil.which(b)) for b in wanted]
    missing = [b for b, exe in found if exe is None]
    if missing and args.browser:
        print(f"not on PATH: {', '.join(missing)}", file=sys.stderr)
        return 1

    failed = False
    for browser, exe in found:
        if exe is None:
            print(f"# {browser} not on PATH, skipped", file=sys.stderr)
            continue
        report = run(browser, exe, args.timeout)
        if "error" in report:
            print(f"{browser} failed: {report['error']}", file=sys.stderr)
            failed = True
            continue
        print(markdown(browser, report))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
