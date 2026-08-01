# hane

羽 *feather*; 跳ね, the flick that ends a brush stroke.

A vector design engine for the web — an Affinity Designer successor that runs in the browser,
so platform stops mattering.

> **Status: the engine works. There is no application yet.**
>
> Geometry, both rasterizers, the scene graph, stroking, boolean operations, SVG round-trip,
> text, and the editing primitives are all built and tested. What does not exist is the thing
> a user opens: no tools bound to a pointer, no panels, no file open/save. `web/` is a shell
> and a benchmark page.
>
> See [`docs/PLAN.md`](docs/PLAN.md) for the phases and [`BENCHMARKS.md`](BENCHMARKS.md) for
> the measurements.

## Where it actually stands

Every claim below is a number some test asserts, not an estimate.

| | |
|---|---|
| GPU vs CPU oracle | **41 of 41** comparable fixtures match in Chromium *and* Firefox; 31 bit-exact, worst difference **1 count** (D-002) |
| Glyph outlines | **7,158,110 glyphs** across 900 faces, control-point-exact against fontTools |
| Text shaping | **99.98%** exact against HarfBuzz over 34,314 comparisons |
| SVG round-trip | **16,074 real files**, worst coordinate delta **0.0**; RMSE 0 against librsvg |
| Boolean ops | union/intersect area error **2.6e-7 / 9.3e-8**; `union + intersect − (A+B)` = 8.9e-16 |
| 100k objects | CPU frame p99 **2.8 ms** (Chromium) / **3.4 ms** (Firefox) of a 16 ms budget |
| Dependencies | **zero**, enforced in CI before anything else runs |

Known-not-robust, because saying so is the point: boolean shallow-angle crossings fail at about
1 pair in 200 on an adversarial corpus (asserted as a *rate*, not hidden); the stroker exceeds
tolerance on 1 of 3000 random cubics near a cusp; `extreme_coords` cannot be GPU-compared
because `f32` has a 64-pixel quantum at 1e9.

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

## Branches and releases

Work lands on **`dev`** (the default branch). **`main`** moves only for a release: `dev` merges
in, a `v*` tag is pushed, and GoReleaser builds the wasm, bundles the shell and publishes the
archive.

Each release ships one artifact — the built web bundle. Unpack it and serve the directory:

```sh
tar xzf hane_1.0.0_web.tar.gz
python3 -m http.server 8080
```

The page loads `hane.wasm` and self-checks a straight-line cubic against its known length, so a
release that opens correctly has proved the whole toolchain end to end.

## Prior art

[Inkscape](https://inkscape.org), [Graphite](https://graphite.rs) and
[Penpot](https://penpot.app) all exist and are free. Penpot's Rust/WASM renderer in particular
is worth reading before touching `hane-gpu`.

## License

MIT OR Apache-2.0
