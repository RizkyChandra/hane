// Entry point for the DOM shell (D-005).
//
// The shell owns no scene state. It sizes the canvas, forwards input, and will
// hand the canvas to the WASM engine once `hane-wasm` exists. Everything about
// the document lives in WASM linear memory (D-004).

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

// P0 placeholder. hane-gpu takes this over in P2; until then the canvas only
// proves the shell, the sizing and the device-pixel-ratio handling work.
const ctx = canvas.getContext("2d");
if (ctx) {
  ctx.fillStyle = "#888";
  ctx.font = "14px system-ui, sans-serif";
  ctx.fillText("hane — P0. No renderer yet.", 16, 28);
}
