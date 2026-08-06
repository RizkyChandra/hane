// The DOM shell (D-005).
//
// It owns no scene state. It sizes the canvas, forwards input as numbers, asks
// the engine for a frame, and rebuilds the panels from one small JSON
// view-model. Every document fact lives in WASM linear memory (D-004) — this
// file never learns what a shape is.

/** What `hane_gl_probe` reports about this machine's WebGL2 (#17). */
interface GlProbe {
  readonly supported: boolean;
  readonly missing_extensions: string;
  summary(): string;
}

/**
 * The engine's exports.
 *
 * Hand-written rather than imported from the generated `hane.d.ts`, because
 * that file only exists after a `cargo build` — typechecking the shell must not
 * require a Rust toolchain. A mismatch shows up immediately at load.
 */
interface Hane {
  /** wasm-bindgen's `init`; fetches and instantiates `hane_bg.wasm`. */
  default(): Promise<{ hane_version(): number }>;
  hane_gl_probe(canvas: HTMLCanvasElement): GlProbe;
  /** `""`/`"auto"` picks a backend; a name is an override (#24). */
  hane_backend(preferred: string): Promise<string>;

  hane_app_init(width: number, height: number, dpr: number): void;
  hane_app_resize(width: number, height: number, dpr: number): void;
  hane_app_tool(name: string): void;
  hane_app_pointer(
    phase: "down" | "move" | "up",
    x: number,
    y: number,
    shift: boolean,
    alt: boolean,
    middle: boolean,
  ): void;
  hane_app_wheel(x: number, y: number, dx: number, dy: number, zoom: boolean): void;
  hane_app_action(name: string): boolean;
  hane_app_select(slot: number, additive: boolean): void;
  hane_app_set_style(fill: string, stroke: string, width: number): void;
  hane_app_state(): string;
  hane_app_svg(): string;
  hane_app_open_svg(source: string): string;
  hane_app_render(canvas: HTMLCanvasElement): void;
}

/** One row of the layer panel. */
interface Layer {
  slot: number;
  name: string;
  selected: boolean;
}

/** The whole view-model. Mirrors `App::state_json` in `crates/hane-wasm`. */
interface State {
  tool: string;
  zoom: number;
  shapes: number;
  selected: number;
  canUndo: boolean;
  canRedo: boolean;
  drawing: boolean;
  fill: string;
  fillOn: boolean;
  stroke: string;
  strokeOn: boolean;
  width: number;
  layers: Layer[];
}

const el = <T extends Element>(selector: string): T => {
  const found = document.querySelector<T>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
};

const canvas = el<HTMLCanvasElement>("#artboard");
const statusEl = el<HTMLElement>("#status");
const layers = el<HTMLUListElement>("#layers");
const filePicker = el<HTMLInputElement>("#file");
const fillOn = el<HTMLInputElement>("#fill-on");
const fill = el<HTMLInputElement>("#fill");
const strokeOn = el<HTMLInputElement>("#stroke-on");
const stroke = el<HTMLInputElement>("#stroke");
const widthInput = el<HTMLInputElement>("#width");
const hint = el<HTMLElement>("#hint");
const zoomLabel = el<HTMLElement>("#zoom");

/**
 * `--target web` glue, loaded as a runtime module rather than a bundled import.
 *
 * The `.wasm` and its glue are build outputs in `public/`, absent from a fresh
 * checkout, so a static import would break `tsc --noEmit` for anyone who has
 * not run cargo. Same reason `Hane` is declared by hand above.
 */
async function loadEngine(): Promise<Hane> {
  const url = new URL("hane.js", document.baseURI).href;
  const hane = (await import(/* @vite-ignore */ url)) as Hane;
  await hane.default();
  return hane;
}

function formatVersion(v: number): string {
  return `${Math.floor(v / 10000)}.${Math.floor(v / 100) % 100}.${v % 100}`;
}

async function main(): Promise<void> {
  let hane: Hane;
  try {
    hane = await loadEngine();
  } catch (err: unknown) {
    // Said out loud rather than swallowed: the alternative is an editor whose
    // every button silently does nothing.
    statusEl.textContent = `engine failed to load\n${String(err)}`;
    return;
  }

  const dpr = () => window.devicePixelRatio || 1;

  /** Sizes the backing store to device pixels, not CSS pixels. */
  function resize(): void {
    const rect = canvas.getBoundingClientRect();
    const w = Math.max(1, Math.round(rect.width * dpr()));
    const h = Math.max(1, Math.round(rect.height * dpr()));
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
      hane.hane_app_resize(w, h, dpr());
      draw();
    }
  }

  // ---- the frame loop ----------------------------------------------------

  let pending = false;
  /**
   * Asks for a frame at the next repaint, and no more than one.
   *
   * Nothing here animates on its own: a frame is drawn because something
   * changed, so an idle editor costs no GPU at all.
   */
  function draw(): void {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => {
      pending = false;
      try {
        hane.hane_app_render(canvas);
      } catch (err: unknown) {
        statusEl.textContent = `render failed\n${String(err)}`;
      }
      sync();
    });
  }

  // ---- the panels --------------------------------------------------------

  let lastJson = "";
  /** Rebuilds the panels, but only when the engine's answer actually changed. */
  function sync(): void {
    const json = hane.hane_app_state();
    if (json === lastJson) return;
    lastJson = json;
    const state = JSON.parse(json) as State;

    for (const button of document.querySelectorAll<HTMLButtonElement>("[data-tool]")) {
      button.ariaPressed = String(button.dataset["tool"] === state.tool);
    }
    disable("undo", !state.canUndo);
    disable("redo", !state.canRedo);
    disable("delete", state.selected === 0);
    zoomLabel.textContent = `${Math.round(state.zoom * 100)}%`;

    // The colour inputs are only written when the engine disagrees: assigning
    // `value` while the user has the picker open closes it.
    if (fill.value !== state.fill) fill.value = state.fill;
    if (stroke.value !== state.stroke) stroke.value = state.stroke;
    fillOn.checked = state.fillOn;
    strokeOn.checked = state.strokeOn;
    if (document.activeElement !== widthInput) widthInput.value = String(state.width);
    hint.textContent = state.drawing
      ? "Drawing — click the first node to close, Enter to end, Esc to drop it."
      : state.selected > 0
        ? `${state.selected} selected of ${state.shapes}`
        : "Nothing selected — this is what the next shape gets.";

    layers.replaceChildren(
      ...state.layers.map((layer) => {
        const row = document.createElement("li");
        row.textContent = layer.name;
        row.ariaSelected = String(layer.selected);
        row.addEventListener("pointerdown", (e) => {
          hane.hane_app_select(layer.slot, e.shiftKey || e.metaKey || e.ctrlKey);
          draw();
        });
        return row;
      }),
    );
  }

  function disable(action: string, off: boolean): void {
    const button = document.querySelector<HTMLButtonElement>(`[data-action="${action}"]`);
    if (button) button.disabled = off;
  }

  // ---- input -------------------------------------------------------------

  /** Canvas-relative device pixels, which is the only space the engine takes. */
  function at(e: PointerEvent | WheelEvent): [number, number] {
    const rect = canvas.getBoundingClientRect();
    return [(e.clientX - rect.left) * dpr(), (e.clientY - rect.top) * dpr()];
  }

  canvas.addEventListener("pointerdown", (e) => {
    // Capture, so a drag that leaves the canvas — or the window — still ends
    // with a pointerup, instead of leaving a gesture stuck open.
    canvas.setPointerCapture(e.pointerId);
    const [x, y] = at(e);
    hane.hane_app_pointer("down", x, y, e.shiftKey, e.altKey, e.button === 1);
    draw();
  });

  canvas.addEventListener("pointermove", (e) => {
    const [x, y] = at(e);
    hane.hane_app_pointer("move", x, y, e.shiftKey, e.altKey, false);
    draw();
  });

  for (const kind of ["pointerup", "pointercancel"] as const) {
    canvas.addEventListener(kind, (e) => {
      const [x, y] = at(e);
      hane.hane_app_pointer("up", x, y, e.shiftKey, e.altKey, false);
      draw();
    });
  }

  canvas.addEventListener(
    "wheel",
    (e) => {
      // Without this the page scrolls and the browser zooms the whole UI on a
      // pinch, which is never what a canvas gesture means.
      e.preventDefault();
      const [x, y] = at(e);
      // ctrl is what a trackpad pinch arrives as, on every platform.
      hane.hane_app_wheel(x, y, e.deltaX, e.deltaY, e.ctrlKey || e.metaKey);
      draw();
    },
    { passive: false },
  );

  // The engine draws its own selection chrome and there is nothing to drop on
  // the canvas, so the browser's context menu on a right-drag is only in the
  // way. Middle-drag panning needs the auxiliary click suppressed too.
  canvas.addEventListener("contextmenu", (e) => e.preventDefault());
  canvas.addEventListener("auxclick", (e) => e.preventDefault());

  for (const button of document.querySelectorAll<HTMLButtonElement>("[data-tool]")) {
    button.addEventListener("click", () => {
      hane.hane_app_tool(button.dataset["tool"] ?? "select");
      draw();
    });
  }

  for (const button of document.querySelectorAll<HTMLButtonElement>("[data-action]")) {
    button.addEventListener("click", () => {
      hane.hane_app_action(button.dataset["action"] ?? "");
      draw();
    });
  }

  // ---- style -------------------------------------------------------------

  function pushStyle(): void {
    hane.hane_app_set_style(
      fillOn.checked ? fill.value : "",
      strokeOn.checked ? stroke.value : "",
      Number(widthInput.value) || 1,
    );
    draw();
  }

  for (const input of [fillOn, fill, strokeOn, stroke, widthInput]) {
    // `input` and not `change`: a colour picker drags, and a live preview is
    // the whole point of a design tool.
    input.addEventListener("input", () => {
      // Turning a colour on is implied by touching its swatch; asking the user
      // to tick the box as well is a step nobody means to skip.
      if (input === fill) fillOn.checked = true;
      if (input === stroke) strokeOn.checked = true;
      pushStyle();
    });
  }

  // ---- files -------------------------------------------------------------

  el<HTMLButtonElement>("#save").addEventListener("click", () => {
    const blob = new Blob([hane.hane_app_svg()], { type: "image/svg+xml" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = "drawing.svg";
    link.click();
    URL.revokeObjectURL(url);
  });

  el<HTMLButtonElement>("#open").addEventListener("click", () => filePicker.click());

  filePicker.addEventListener("change", async () => {
    const file = filePicker.files?.[0];
    if (!file) return;
    const report = hane.hane_app_open_svg(await file.text());
    // A file that only half-opened has to say so before it is saved over.
    if (report) hint.textContent = report;
    // Cleared so that opening the same file twice fires `change` both times.
    filePicker.value = "";
    lastJson = "";
    draw();
  });

  // ---- keyboard ----------------------------------------------------------

  const TOOL_KEYS: Record<string, string> = {
    v: "select",
    m: "rect",
    e: "ellipse",
    p: "pen",
    r: "rotate",
    h: "pan",
  };

  window.addEventListener("keydown", (e) => {
    // A shortcut must never eat what someone is typing into a field.
    const target = e.target as HTMLElement | null;
    if (target && (target.tagName === "INPUT" || target.isContentEditable)) return;

    const mod = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    let handled = true;
    if (mod && key === "z") hane.hane_app_action(e.shiftKey ? "redo" : "undo");
    else if (mod && key === "y") hane.hane_app_action("redo");
    else if (mod && key === "a") hane.hane_app_action("select-all");
    else if (mod && key === "0") hane.hane_app_action("zoom-fit");
    else if (mod && key === "s") el<HTMLButtonElement>("#save").click();
    else if (mod && key === "o") filePicker.click();
    else if (mod) handled = false;
    else if (key === "delete" || key === "backspace") hane.hane_app_action("delete");
    else if (key === "escape") hane.hane_app_action("escape");
    else if (key === "enter") hane.hane_app_action("finish");
    else if (key in TOOL_KEYS) hane.hane_app_tool(TOOL_KEYS[key] as string);
    else handled = false;

    if (handled) {
      e.preventDefault();
      draw();
    }
  });

  // ---- start -------------------------------------------------------------

  const rect = canvas.getBoundingClientRect();
  canvas.width = Math.max(1, Math.round(rect.width * dpr()));
  canvas.height = Math.max(1, Math.round(rect.height * dpr()));
  hane.hane_app_init(canvas.width, canvas.height, dpr());
  new ResizeObserver(resize).observe(canvas);
  // Moving a window between a Retina and a normal display changes the ratio
  // without changing the element's size, so the observer never fires.
  window.addEventListener("resize", resize);
  pushStyle();
  draw();

  const probe = hane.hane_gl_probe(canvas);
  const backend = await hane.hane_backend("auto").catch((e: unknown) => `unavailable (${e})`);
  const version = formatVersion((await hane.default()).hane_version());
  statusEl.textContent = probe.supported
    ? `hane ${version}\nbackend ${backend}\n${probe.summary()}`
    : `hane ${version}\nWEBGL2 UNSUPPORTED — missing ${probe.missing_extensions}`;

  // For the console, and for the two harness pages that share this build.
  (globalThis as { hane?: unknown }).hane = hane;
}

void main();

// This file is a module, not a script: without an export its top-level names
// would collide with the other pages under a shared tsconfig.
export {};
