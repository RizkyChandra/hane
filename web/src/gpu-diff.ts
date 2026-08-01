// The GPU/oracle diff page (#19–#23, D-002).
//
// Renders every fixture of `hane_raster::corpus` through the WebGL2 renderer
// and POSTs the framebuffers back to `scripts/gpu-diff.py`, which writes them
// where `crates/hane-gpu/tests/oracle_diff.rs` reads them. The comparison
// itself happens there and only there: one rule judges both sides.
//
// This page draws nothing a person can see. It cannot: the point is the exact
// bytes, and anything that scaled or presented them would round twice.

/** The renderer exports of `hane-wasm`. See `crates/hane-wasm/src/glrender.rs`. */
interface HaneGlue {
  /** wasm-bindgen's `init`; fetches and instantiates `hane_bg.wasm`. */
  default(): Promise<unknown>;
  hane_gl_probe(canvas: HTMLCanvasElement): { summary(): string; supported: boolean };
  hane_fixture_count(): number;
  hane_fixture_name(index: number): string;
  /** Four header bytes (width, height as LE u16) then premultiplied RGBA. */
  hane_gl_render_fixture(canvas: HTMLCanvasElement, index: number): Uint8Array;
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

async function main(): Promise<void> {
  // See `main.ts`: the glue is a build output, so it is imported by URL rather
  // than bundled, which keeps `tsc --noEmit` independent of a Rust toolchain.
  const url = new URL("hane.js", document.baseURI).href;
  const glue = (await import(/* @vite-ignore */ url)) as HaneGlue;
  await glue.default();

  const probe = glue.hane_gl_probe(canvas);
  log(probe.summary());
  if (!probe.supported) throw new Error(probe.summary());

  const fixtures: Rendered[] = [];
  const failures: string[] = [];
  const n = glue.hane_fixture_count();
  for (let i = 0; i < n; i++) {
    const name = glue.hane_fixture_name(i);
    statusEl.textContent = `rendering ${i + 1}/${n}: ${name}`;
    try {
      const buf = glue.hane_gl_render_fixture(canvas, i);
      const width = buf[0]! | (buf[1]! << 8);
      const height = buf[2]! | (buf[3]! << 8);
      fixtures.push({ name, width, height, pixels: base64(buf.subarray(4)) });
    } catch (e) {
      failures.push(`${name}: ${e}`);
      log(`FAILED ${name}: ${e}`);
    }
    // Yield, so a slow software rasterizer does not trip the browser's
    // unresponsive-page watchdog partway through the corpus.
    await new Promise((r) => setTimeout(r, 0));
  }

  log(`rendered ${fixtures.length} of ${n}`);
  statusEl.textContent = "reporting";
  await fetch("/", {
    method: "POST",
    body: JSON.stringify({
      userAgent: navigator.userAgent,
      probe: probe.summary(),
      // How many the corpus holds, so the runner can tell a partial dump from
      // a corpus that shrank -- one is a bug and the other is a rename.
      expected: n,
      failures,
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
