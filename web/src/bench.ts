// The P3 gate benchmark page (#31).
//
// Drives `hane-wasm`'s bench harness — the same code and the same scripted
// camera the native `gate_bench` example runs — and times each phase with
// `performance.now()`.
//
// What is NOT measured here, and cannot be: the GPU. P2 is unbuilt, so there
// is no canvas, no upload and no composite. See the harness's module docs and
// BENCHMARKS.md; the numbers below are the CPU side of a frame only.

/** The bench exports of `hane-wasm`. See `crates/hane-wasm/src/bench.rs`. */
interface BenchExports {
  hane_version(): number;
  hane_bench_init(n: number, seed: number, w: number, h: number, pan: number): number;
  hane_bench_cull(): number;
  hane_bench_encode(): number;
  hane_bench_raster(): number;
  hane_bench_visible(): number;
  hane_bench_tiles(): number;
  hane_bench_hit_rate(): number;
  hane_bench_bytes(): number;
}

// Kept in lockstep with `gate_bench.rs`. A browser row and a native row of the
// same size have to describe the same work or the comparison is noise.
const FRAMES = 300;
const WARMUP = FRAMES >> 3;
const WIDTH = 1280;
const HEIGHT = 720;
const SIZES = [1_000, 10_000, 100_000, 500_000];
const SPEEDS: [string, number][] = [
  ["navigate, 4 px/frame", 4],
  ["flick, 40 px/frame", 40],
];

const statusEl = document.querySelector<HTMLElement>("#status")!;
const out = document.querySelector<HTMLElement>("#out")!;

function log(line: string): void {
  out.textContent += `${line}\n`;
}

/**
 * The smallest non-zero gap `performance.now()` will report, in milliseconds.
 *
 * Not a curiosity: Firefox clamps the clock to 1 ms unless the document is
 * cross-origin isolated, which would quantise every phase below into "0 or 1"
 * and make the percentiles fiction. The runner serves COOP/COEP headers to
 * avoid that, and this number is published so a reader can see whether it
 * worked rather than trust that it did.
 */
function timerResolutionMs(): number {
  let best = Infinity;
  for (let i = 0; i < 20_000; i++) {
    const a = performance.now();
    let b = performance.now();
    while (b === a) b = performance.now();
    best = Math.min(best, b - a);
  }
  return best;
}

/** Nearest-rank percentile of a millisecond series. */
function pct(samples: number[], p: number): number {
  const s = [...samples].sort((a, b) => a - b);
  const i = Math.ceil((p / 100) * s.length);
  return s[Math.min(Math.max(i - 1, 0), s.length - 1)] ?? 0;
}

interface Pair {
  p50: number;
  p99: number;
}

interface Row {
  n: number;
  script: string;
  buildMs: number;
  visible: number;
  tiles: number;
  missesPerFrame: number;
  segmentsPerFrame: number;
  hitRate: number;
  residentMiB: number;
  coldFrameMs: number;
  coldRasterMs: number;
  cull: Pair;
  encode: Pair;
  raster: Pair;
  /** The CPU cost of a GPU frame with the GPU removed. */
  frame: Pair;
}

async function run(hane: BenchExports, n: number, script: string, pan: number): Promise<Row> {
  statusEl.textContent = `n = ${n}, ${script} — building…`;
  await new Promise((r) => setTimeout(r, 0));
  const buildStart = performance.now();
  hane.hane_bench_init(n, n, WIDTH, HEIGHT, pan);
  const buildMs = performance.now() - buildStart;

  const cull: number[] = [];
  const encode: number[] = [];
  const raster: number[] = [];
  let misses = 0;
  let segments = 0;
  let visible = 0;
  for (let frame = 0; frame < FRAMES; frame++) {
    let t = performance.now();
    misses += hane.hane_bench_cull();
    cull.push(performance.now() - t);

    t = performance.now();
    segments += hane.hane_bench_encode();
    encode.push(performance.now() - t);

    t = performance.now();
    hane.hane_bench_raster();
    raster.push(performance.now() - t);

    visible += hane.hane_bench_visible();
    // Yield often enough that the tab is never declared unresponsive. Between
    // frames, never inside one, so nothing lands in a timed span.
    if (frame % 8 === 7) {
      statusEl.textContent = `n = ${n}, ${script} — frame ${frame + 1}/${FRAMES}`;
      await new Promise((r) => setTimeout(r, 0));
    }
  }

  const coldFrameMs = (cull[0] ?? 0) + (encode[0] ?? 0);
  const coldRasterMs = raster[0] ?? 0;
  const warm = <T>(a: T[]): T[] => a.slice(WARMUP);
  const [c, e, r] = [warm(cull), warm(encode), warm(raster)];
  // p99 of a sum is not the sum of the p99s, so the combined series is summed
  // per frame before the percentile is taken.
  const frames = c.map((v, i) => v + (e[i] ?? 0));
  const phase = (s: number[]) => ({ p50: pct(s, 50), p99: pct(s, 99) });

  return {
    n,
    script,
    buildMs,
    visible: visible / FRAMES,
    tiles: hane.hane_bench_tiles(),
    missesPerFrame: misses / FRAMES,
    segmentsPerFrame: segments / FRAMES,
    hitRate: hane.hane_bench_hit_rate(),
    residentMiB: hane.hane_bench_bytes() / (1 << 20),
    coldFrameMs,
    coldRasterMs,
    cull: phase(c),
    encode: phase(e),
    raster: phase(r),
    frame: phase(frames),
  };
}

async function main(): Promise<void> {
  const url = new URL("hane.wasm", document.baseURI);
  const { instance } = await WebAssembly.instantiateStreaming(fetch(url), {});
  const hane = instance.exports as unknown as BenchExports;

  const resolution = timerResolutionMs();
  const report = {
    userAgent: navigator.userAgent,
    crossOriginIsolated: globalThis.crossOriginIsolated,
    timerResolutionMs: resolution,
    engine: hane.hane_version(),
    frames: FRAMES,
    warmup: WARMUP,
    width: WIDTH,
    height: HEIGHT,
    rows: [] as Row[],
  };
  log(`${navigator.userAgent}`);
  log(`performance.now() resolution ${resolution.toFixed(4)} ms, crossOriginIsolated=${globalThis.crossOriginIsolated}`);

  for (const n of SIZES) {
    for (const [script, pan] of SPEEDS) {
      const row = await run(hane, n, script, pan);
      report.rows.push(row);
      log(
        `n=${n} ${script}: cull+encode p50 ${row.frame.p50.toFixed(3)} ` +
          `p99 ${row.frame.p99.toFixed(3)} ms, ` +
          `raster p99 ${row.raster.p99.toFixed(1)} ms, ` +
          `hit rate ${(row.hitRate * 100).toFixed(1)}%`,
      );
    }
  }

  statusEl.textContent = "done";
  // The runner script collects the results this way; opened by hand, the POST
  // just fails and the page keeps its own report.
  await fetch("./result", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(report),
  }).catch(() => undefined);
}

main().catch((err: unknown) => {
  statusEl.textContent = `FAILED: ${String(err)}`;
  void fetch("./result", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ error: String(err) }),
  }).catch(() => undefined);
});
