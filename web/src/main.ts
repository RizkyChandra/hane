// Entry point for the DOM shell (D-005).
//
// The shell owns no scene state. It sizes the canvas, forwards input, and hands
// off to the engine. Everything about the document lives in WASM linear memory
// (D-004).

/** What `hane_gl_probe` reports about this machine's WebGL2 (#17). */
interface GlProbe {
  readonly max_texture_size: number;
  readonly max_draw_buffers: number;
  readonly float_render_targets: boolean;
  readonly float_blend: boolean;
  readonly float_linear_filter: boolean;
  /** False when a required extension is absent — the renderer cannot start. */
  readonly supported: boolean;
  /** Comma-separated, empty when `supported`. */
  readonly missing_extensions: string;
  summary(): string;
}

/**
 * The P0 `extern "C"` exports.
 *
 * These live on the **instance**, not on the glue module: wasm-bindgen only
 * writes JS wrappers for the items it generated, and passes a raw
 * `#[unsafe(no_mangle)]` export straight through to `WebAssembly.Instance`.
 * `init()` hands that object back, which is the only way to reach them.
 */
interface HaneRaw {
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

/**
 * The `#[wasm_bindgen]` exports: named exports of the generated module.
 *
 * Hand-written rather than imported from the generated `hane.d.ts`, because
 * that file only exists after a `cargo build` — typechecking the shell must not
 * require a Rust toolchain. It is a small surface and a mismatch shows up
 * immediately at load, in the self-check below.
 */
interface HaneGlue {
  /** wasm-bindgen's `init`; fetches and instantiates `hane_bg.wasm`. */
  default(): Promise<HaneRaw>;
  /** Creates the WebGL2 context on `canvas` and probes it. Cached; logs once. */
  hane_gl_probe(canvas: HTMLCanvasElement): GlProbe;
  hane_gl_context_lost(): boolean;
}

/** Both halves of the engine, kept apart because they are reached differently. */
interface Hane {
  glue: HaneGlue;
  raw: HaneRaw;
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
 * `--target web` glue, loaded as a runtime module rather than a bundled import.
 *
 * The `.wasm` and its glue are build outputs in `public/`, absent from a fresh
 * checkout, so a static import would break `tsc --noEmit` for anyone who has
 * not run cargo. A dynamic import of a URL keeps the type-check independent of
 * the Rust build, which is the same reason `Hane` is declared by hand.
 */
async function loadEngine(): Promise<Hane> {
  const url = new URL("hane.js", document.baseURI).href;
  const glue = (await import(/* @vite-ignore */ url)) as HaneGlue;
  return { glue, raw: await glue.default() };
}

function formatVersion(v: number): string {
  return `${Math.floor(v / 10000)}.${Math.floor(v / 100) % 100}.${v % 100}`;
}

/**
 * Status goes in the DOM, not on the canvas.
 *
 * It used to be drawn with a 2D context. It cannot be any more: a canvas gets
 * exactly one context for its lifetime, and #17 claims that context for WebGL2.
 * D-005 wanted the chrome in the DOM anyway.
 */
function report(lines: string[]): void {
  const panel = document.querySelector("#left-panel");
  if (!panel) return;
  let pre = panel.querySelector("pre");
  if (!pre) {
    pre = document.createElement("pre");
    pre.id = "status";
    panel.append(pre);
  }
  pre.textContent = lines.join("\n");
}

// P0 placeholder plus the P2 probe. hane-gpu takes the canvas over when #19
// lands; until then this is a live check that the engine loaded, that its
// geometry answers correctly through the wasm boundary — a straight-line cubic
// must measure exactly 3.0 and flatten to its two endpoints — and that a
// WebGL2 context can be made and can do what the renderer will need.
loadEngine().then(
  (hane) => {
    const straight = [0, 0, 1, 0, 2, 0, 3, 0] as const;
    const length = hane.raw.hane_curve_length(...straight);
    const points = hane.raw.hane_flatten_count(...straight, 0.1);
    const ok = Math.abs(length - 3) < 1e-9 && points === 2;

    const lines = [
      `hane ${formatVersion(hane.raw.hane_version())} — engine loaded`,
      `straight cubic: length ${length.toFixed(12)}, ${points} points`,
      ok ? "self-check passed" : "SELF-CHECK FAILED",
      "",
    ];

    try {
      const probe = hane.glue.hane_gl_probe(canvas!);
      lines.push(
        `max texture size    ${probe.max_texture_size}`,
        `max draw buffers    ${probe.max_draw_buffers}`,
        `float render target ${probe.float_render_targets}`,
        `float blend         ${probe.float_blend}`,
        `float linear filter ${probe.float_linear_filter}`,
        "",
        probe.supported
          ? "webgl2 ok — no renderer yet, see docs/PLAN.md (P2)"
          : `WEBGL2 UNSUPPORTED — missing ${probe.missing_extensions}`,
      );
    } catch (err: unknown) {
      // Only thrown when there is no WebGL2 context at all. Said out loud
      // rather than swallowed: the alternative is a blank artboard.
      lines.push(`NO WEBGL2 CONTEXT — ${String(err)}`);
    }
    report(lines);

    // Everything the engine exposes, for the console and for the benchmark
    // page, which does not exist yet. `hane_gl_probe` is cached, so that page
    // calling it again costs nothing and logs nothing a second time.
    (globalThis as { hane?: unknown }).hane = hane;
  },
  (err: unknown) => {
    report(["hane — engine failed to load", String(err)]);
  },
);
