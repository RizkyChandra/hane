// The GPU/oracle diff page (#19–#24, D-002).
//
// Renders every fixture of `hane_raster::corpus` through one of the two GPU
// backends and POSTs the framebuffers back to `scripts/gpu-diff.py`, which
// writes them where `crates/hane-gpu/tests/oracle_diff.rs` reads them. The
// comparison itself happens there and only there: one rule judges both sides,
// and the same rule judges both backends.
//
// `?backend=` is the manual override — `webgl2`, `webgpu`, or absent for the
// automatic pick. `?bench=<n>` additionally times n passes over the whole
// corpus on every backend this browser has, which is #24's "benchmarked against
// WebGL2 on identical scenes".
//
// This page draws nothing a person can see. It cannot: the point is the exact
// bytes, and anything that scaled or presented them would round twice.

/** The renderer exports of `hane-wasm`. See `crates/hane-wasm/src/wgpurender.rs`. */
interface HaneGlue {
  /** wasm-bindgen's `init`; fetches and instantiates `hane_bg.wasm`. */
  default(): Promise<unknown>;
  hane_gl_probe(canvas: HTMLCanvasElement): { summary(): string; supported: boolean };
  hane_wgpu_probe(): Promise<string>;
  /** `""`/`"auto"` picks; a backend name is an override that must exist. */
  hane_backend(preferred: string): Promise<string>;
  hane_fixture_count(): number;
  hane_fixture_name(index: number): string;
  /** Four header bytes (width, height as LE u16) then premultiplied RGBA. */
  hane_render_fixture(
    canvas: HTMLCanvasElement,
    index: number,
    backend: string,
  ): Promise<Uint8Array>;
}

const statusEl = document.querySelector<HTMLElement>("#status")!;
const out = document.querySelector<HTMLElement>("#out")!;
const canvas = document.querySelector<HTMLCanvasElement>("#gl")!;

function log(line: string): void {
  out.textContent += `${line}\n`;
}

/** Base64 of a byte array, in chunks so a large fixture cannot blow the stack. */
function base64(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
}

interface Rendered {
  name: string;
  width: number;
  height: number;
  /** Premultiplied RGBA, row-major from the top left, base64. */
  pixels: string;
}

/** One backend's capability line. */
async function probe(glue: HaneGlue, backend: string): Promise<string> {
  if (backend === "webgpu") return await glue.hane_wgpu_probe();
  return glue.hane_gl_probe(canvas).summary();
}

/** Renders the whole corpus once, collecting pixels unless only timing it. */
async function renderCorpus(
  glue: HaneGlue,
  backend: string,
  collect: Rendered[] | null,
  failures: string[],
): Promise<void> {
  const n = glue.hane_fixture_count();
  for (let i = 0; i < n; i++) {
    const name = glue.hane_fixture_name(i);
    if (collect) statusEl.textContent = `${backend} ${i + 1}/${n}: ${name}`;
    try {
      const buf = await glue.hane_render_fixture(canvas, i, backend);
      if (collect) {
        const width = buf[0]! | (buf[1]! << 8);
        const height = buf[2]! | (buf[3]! << 8);
        collect.push({ name, width, height, pixels: base64(buf.subarray(4)) });
      }
    } catch (e) {
      failures.push(`${backend} ${name}: ${e}`);
      log(`FAILED ${name}: ${e}`);
    }
    // Yield, so a slow software rasterizer does not trip the browser's
    // unresponsive-page watchdog partway through the corpus.
    await new Promise((r) => setTimeout(r, 0));
  }
}

interface Timing {
  backend: string;
  passes: number;
  msPerCorpus: number;
}

/**
 * Times both backends over the same fixtures in the same session.
 *
 * One warm pass first and it is not counted: the first render of a backend
 * compiles its shaders, and on a software adapter that costs more than every
 * later pass put together.
 */
async function benchmark(glue: HaneGlue, passes: number): Promise<Timing[]> {
  const rows: Timing[] = [];
  for (const backend of ["webgl2", "webgpu"]) {
    try {
      await glue.hane_backend(backend);
    } catch {
      log(`bench: no ${backend} here`);
      continue;
    }
    const failures: string[] = [];
    statusEl.textContent = `bench ${backend}: warmup`;
    await renderCorpus(glue, backend, null, failures);
    const times: number[] = [];
    for (let p = 0; p < passes; p++) {
      statusEl.textContent = `bench ${backend}: pass ${p + 1}/${passes}`;
      const t0 = performance.now();
      await renderCorpus(glue, backend, null, failures);
      times.push(performance.now() - t0);
    }
    // Median, the convention BENCHMARKS.md holds every other harness to.
    times.sort((a, b) => a - b);
    const msPerCorpus = times[times.length >> 1]!;
    rows.push({ backend, passes, msPerCorpus });
    log(`bench ${backend}: ${msPerCorpus.toFixed(1)} ms per corpus pass`);
  }
  return rows;
}

async function main(): Promise<void> {
  // See `main.ts`: the glue is a build output, so it is imported by URL rather
  // than bundled, which keeps `tsc --noEmit` independent of a Rust toolchain.
  const url = new URL("hane.js", document.baseURI).href;
  const glue = (await import(/* @vite-ignore */ url)) as HaneGlue;
  await glue.default();

  const params = new URLSearchParams(location.search);
  const backend = await glue.hane_backend(params.get("backend") ?? "auto");
  const summary = await probe(glue, backend);
  log(`backend ${backend}: ${summary}`);

  const fixtures: Rendered[] = [];
  const failures: string[] = [];
  await renderCorpus(glue, backend, fixtures, failures);
  log(`rendered ${fixtures.length} of ${glue.hane_fixture_count()}`);

  const passes = Number(params.get("bench") ?? 0);
  const bench = passes > 0 ? await benchmark(glue, passes) : [];

  statusEl.textContent = "reporting";
  await fetch("/", {
    method: "POST",
    body: JSON.stringify({
      userAgent: navigator.userAgent,
      backend,
      probe: summary,
      // How many the corpus holds, so the runner can tell a partial dump from
      // a corpus that shrank -- one is a bug and the other is a rename.
      expected: glue.hane_fixture_count(),
      failures,
      bench,
      fixtures,
    }),
  });
  statusEl.textContent = "done";
}

main().catch(async (e: unknown) => {
  statusEl.textContent = `failed: ${e}`;
  await fetch("/", { method: "POST", body: JSON.stringify({ error: String(e) }) });
});

// This file is a module, not a script: without an export its top-level names
// would collide with the other pages under a shared tsconfig.
export {};
