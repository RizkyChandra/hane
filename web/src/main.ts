// Entry point for the DOM shell (D-005).
//
// The shell owns no scene state. It sizes the canvas, forwards input, and hands
// off to the engine. Everything about the document lives in WASM linear memory
// (D-004).

/** The raw exports of `hane-wasm`. Scalars only at P0 — see that crate's docs. */
interface HaneExports {
  hane_version(): number;
  hane_flatten_count(
    x0: number, y0: number, x1: number, y1: number,
    x2: number, y2: number, x3: number, y3: number,
    tolerance: number,
  ): number;
  hane_curve_length(
    x0: number, y0: number, x1: number, y1: number,
    x2: number, y2: number, x3: number, y3: number,
  ): number;
}

const canvas = document.querySelector<HTMLCanvasElement>("#artboard");
if (!canvas) throw new Error("missing #artboard canvas");

/** Size the backing store to device pixels, not CSS pixels. */
function resize(c: HTMLCanvasElement): void {
  const dpr = window.devicePixelRatio || 1;
  const rect = c.getBoundingClientRect();
  const w = Math.max(1, Math.round(rect.width * dpr));
  const h = Math.max(1, Math.round(rect.height * dpr));
  if (c.width !== w || c.height !== h) {
    c.width = w;
    c.height = h;
  }
}

new ResizeObserver(() => resize(canvas)).observe(canvas);
resize(canvas);

/**
 * No `wasm-bindgen` at P0 — every export is numbers in, numbers out, so there
 * is no glue to generate and no import object to supply. Rust's allocator on
 * `wasm32-unknown-unknown` is self-contained, so the module imports nothing.
 */
async function loadEngine(): Promise<HaneExports> {
  const url = new URL("hane.wasm", document.baseURI);
  const { instance } = await WebAssembly.instantiateStreaming(fetch(url), {});
  return instance.exports as unknown as HaneExports;
}

function formatVersion(v: number): string {
  return `${Math.floor(v / 10000)}.${Math.floor(v / 100) % 100}.${v % 100}`;
}

function report(lines: string[]): void {
  const ctx = canvas!.getContext("2d");
  if (!ctx) return;
  const dpr = window.devicePixelRatio || 1;
  ctx.save();
  ctx.scale(dpr, dpr);
  ctx.fillStyle = "#888";
  ctx.font = "13px ui-monospace, monospace";
  lines.forEach((line, i) => ctx.fillText(line, 16, 28 + i * 20));
  ctx.restore();
}

// P0 placeholder. hane-gpu takes the canvas over in P2. Until then this is a
// live check that the engine loaded and its geometry answers correctly through
// the wasm boundary — a straight-line cubic must measure exactly 3.0 and
// flatten to its two endpoints.
loadEngine().then(
  (hane) => {
    const straight = [0, 0, 1, 0, 2, 0, 3, 0] as const;
    const length = hane.hane_curve_length(...straight);
    const points = hane.hane_flatten_count(...straight, 0.1);
    const ok = Math.abs(length - 3) < 1e-9 && points === 2;
    report([
      `hane ${formatVersion(hane.hane_version())} — engine loaded`,
      `straight cubic: length ${length.toFixed(12)}, ${points} points`,
      ok ? "self-check passed" : "SELF-CHECK FAILED",
      "",
      "No renderer yet — see docs/PLAN.md (P2).",
    ]);
  },
  (err: unknown) => {
    report(["hane — engine failed to load", String(err)]);
  },
);
