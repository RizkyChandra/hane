#!/usr/bin/env python3
"""Render the fixture corpus on a real GPU and dump it for the oracle diff (#19, #24).

    python3 scripts/gpu-diff.py [--browser chromium] [--browser firefox]
                                [--backend webgl2|webgpu|auto] [--bench N]
    cargo test -p hane-gpu --test oracle_diff

The first command drives a headless browser at `gpu-diff.html`, which renders
every fixture through one of the two GPU backends and POSTs the framebuffers
back here; they are written to `crates/hane-gpu/tests/gpu-out/<name>.bin` as raw
premultiplied RGBA. The second command is the comparison -- it lives in the
Rust harness and nowhere else, because D-002 is only worth anything if one rule
judges both sides. The harness does not care which backend produced the bytes,
which is the point: the same tolerance table judges both.

`--bench N` additionally times N passes over the whole corpus on *every*
backend the browser has, in one session on identical scenes (#24).

Expects `web/dist` to already contain `gpu-diff.html`, `hane.js` and
`hane_bg.wasm`:

    cargo build --release --target wasm32-unknown-unknown -p hane-wasm
    wasm-bindgen --target web --out-dir web/public --out-name hane \\
        target/wasm32-unknown-unknown/release/hane_wasm.wasm
    cd web && npm ci && npm run build

Same shape as `gate-bench.py`, and for the same reasons: no webdriver, and
COOP/COEP so the page is cross-origin isolated. The one addition is
`--use-gl=swiftshader`: on a machine with no GPU, headless Chromium's software
WebGL2 is a conformant implementation and a legitimate target for a per-pixel
diff -- it is the *arithmetic* under test, not the silicon.
"""

import argparse
import base64
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
OUT = ROOT / "crates" / "hane-gpu" / "tests" / "gpu-out"

LAUNCH = {
    # SwiftShader by name rather than by fallback: `--disable-gpu` alone gives
    # some builds no WebGL2 at all, and a silent "context creation failed" is
    # indistinguishable from a renderer bug. It is spelled `--use-angle`, not
    # `--use-gl`: WebGPU needs `--enable-features=Vulkan` for Dawn to find
    # Vulkan-SwiftShader, and `--use-gl=swiftshader` alongside it leaves the GPU
    # process asking for `gl=none,angle=none` and dying at startup -- which
    # arrives much later as "a valid external Instance reference no longer
    # exists" from `requestDevice`. ANGLE-on-SwiftShader serves WebGL2 just as
    # well, so one launch covers both backends and `--backend auto` means
    # something.
    "chromium": ["--headless=new", "--no-sandbox", "--use-angle=swiftshader",
                 "--enable-unsafe-swiftshader", "--enable-unsafe-webgpu",
                 "--enable-features=Vulkan",
                 "--disable-background-timer-throttling",
                 "--disable-renderer-backgrounding", "--user-data-dir={profile}"],
    "firefox": ["--headless", "--profile", "{profile}"],
}

# Firefox will not create a WebGL context in a headless session without being
# told that software rendering is acceptable. `dom.webgpu.enabled` is the same
# door for the other backend: on Linux, Firefox 153 ships WebGPU behind it.
FIREFOX_PREFS = "\n".join([
    'user_pref("webgl.force-enabled", true);',
    'user_pref("webgl.disabled", false);',
    'user_pref("gfx.webrender.software", true);',
    'user_pref("webgl.forbid-software", false);',
    'user_pref("dom.webgpu.enabled", true);',
    "",
])


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


def run(browser: str, exe: str, timeout: float, verbose: bool, query: str) -> dict:
    Handler.result, Handler.done = None, threading.Event()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/gpu-diff.html{query}"
    print(f"# {browser}: {url}", file=sys.stderr)

    with tempfile.TemporaryDirectory() as profile:
        if browser == "firefox":
            (Path(profile) / "user.js").write_text(FIREFOX_PREFS)
        argv = [exe] + [a.format(profile=profile) for a in LAUNCH[browser]] + [url]
        sink = None if verbose else subprocess.DEVNULL
        proc = subprocess.Popen(argv, stdout=sink, stderr=sink)
        try:
            if not Handler.done.wait(timeout):
                raise TimeoutError(f"{browser} did not report within {timeout:.0f}s")
        finally:
            proc.terminate()
            proc.wait(timeout=30)
            server.shutdown()
    return Handler.result


def write(report: dict) -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("*.bin"):
        old.unlink()
    for f in report["fixtures"]:
        pixels = base64.b64decode(f["pixels"])
        want = f["width"] * f["height"] * 4
        if len(pixels) != want:
            print(f"{f['name']}: {len(pixels)} bytes for {f['width']}x{f['height']}",
                  file=sys.stderr)
            continue
        (OUT / f"{f['name']}.bin").write_bytes(pixels)
    return len(list(OUT.glob("*.bin")))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--browser", action="append", choices=sorted(LAUNCH),
                    help="repeatable; default is chromium alone")
    ap.add_argument("--backend", choices=["auto", "webgl2", "webgpu"], default="auto",
                    help="which GPU backend renders the corpus; auto prefers WebGPU")
    ap.add_argument("--bench", type=int, default=0, metavar="N",
                    help="also time N corpus passes on every backend present")
    ap.add_argument("--timeout", type=float, default=1800.0)
    ap.add_argument("--verbose", action="store_true", help="let the browser talk")
    args = ap.parse_args()
    query = f"?backend={args.backend}&bench={args.bench}"

    if not (DIST / "gpu-diff.html").is_file():
        print(f"{DIST} has no gpu-diff.html -- see this script's docstring", file=sys.stderr)
        return 1

    for browser in args.browser or ["chromium"]:
        exe = shutil.which(browser)
        if exe is None:
            print(f"# {browser} not on PATH, skipped", file=sys.stderr)
            continue
        report = run(browser, exe, args.timeout, args.verbose, query)
        if "error" in report:
            print(f"{browser} failed: {report['error']}", file=sys.stderr)
            return 1
        for line in report.get("failures", []):
            print(f"{browser}: {line}", file=sys.stderr)
        n = write(report)
        expected = report.get("expected", n)
        if n != expected:
            # A partial dump is worse than none: the Rust harness would report
            # the missing ones as "not compared" and the rest as passing.
            print(f"{browser}: {n} of {expected} fixtures came back", file=sys.stderr)
            return 1
        print(f"# {browser}: backend {report['backend']} -- {report['probe']}", file=sys.stderr)
        print(f"# {browser}: {report['userAgent']}", file=sys.stderr)
        for row in report.get("bench", []):
            print(f"# {browser}: {row['backend']} {row['msPerCorpus']:.1f} ms per corpus "
                  f"pass, median of {row['passes']}", file=sys.stderr)
        print(f"wrote {n} fixtures to {OUT}")
        print("now run: cargo test -p hane-gpu --test oracle_diff")
    return 0


if __name__ == "__main__":
    sys.exit(main())
