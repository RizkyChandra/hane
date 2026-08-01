# hane

羽 *feather*; 跳ね, the flick that ends a brush stroke.

A vector design engine for the web — an Affinity Designer successor that runs in the browser,
so platform stops mattering.

> **Status: P0.** Nothing works yet. The workspace, CI and geometry primitives exist; the
> renderer does not. See [`docs/PLAN.md`](docs/PLAN.md) for the full plan and
> [issues](https://github.com/RizkyChandra/hane/issues) for what's next.

## What this is

| | |
|---|---|
| Target | Web only. Rust → WASM, DOM shell in TypeScript. |
| Scale | 100k+ objects at 60fps. |
| Dependencies | **Zero.** Every algorithm is ours — geometry, rasterizer, boolean ops, fonts, SVG. |
| Renderer | Ours, from day one. WebGL2 first, WebGPU second. |

The scale target is what drives the architecture. At 100k objects the rasterizer's inner loop
barely matters; what matters is never touching the 99% of objects that are off-screen or
unchanged. **Tiling, culling and cache invalidation are the product.**

## Layout

```
crates/
  hane-geom     points, vectors, affine transforms, rects, Bezier primitives
  hane-path     paths, winding, stroke expansion, boolean operations
  hane-raster   CPU scanline AA rasterizer — the correctness oracle
  hane-gpu      tile-based GPU rasterizer, WebGL2 → WebGPU
  hane-scene    scene graph, spatial index, tile cache, culling
  hane-svg      namespace-aware XML, SVG path grammar, import/export
  hane-edit     tools, selection, hit testing, undo command log
  hane-text     OpenType parsing, shaping, layout
  hane-wasm     wasm-bindgen surface — the only crate with external dependencies
web/            TypeScript + Vite shell
```

## Two decisions worth knowing up front

**The CPU rasterizer is the oracle** (D-002). `hane-raster` is written before `hane-gpu` and
optimised for being *obviously correct* rather than fast. Every GPU render is diffed against it
per-pixel in CI. A from-scratch GPU rasterizer with no reference implementation is
undebuggable.

**Zero dependencies is enforced, not trusted** (D-001). `scripts/check-zero-deps.py` runs first
in CI and fails the build if any crate outside `hane-wasm` gains an external dependency —
including dev-dependencies. Adding one requires editing that script on purpose, in a diff
someone reviews.

## Building

```sh
cargo test --workspace
python3 scripts/check-zero-deps.py
```

## Prior art

[Inkscape](https://inkscape.org), [Graphite](https://graphite.rs) and
[Penpot](https://penpot.app) all exist and are free. Penpot's Rust/WASM renderer in particular
is worth reading before touching `hane-gpu`.

## License

MIT OR Apache-2.0
