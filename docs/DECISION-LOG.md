# Decision log

Numbered, argued decisions. Same convention as `sable`. The full reasoning for each lives in
[`PLAN.md`](PLAN.md); this file is the index.

| # | Decision |
|---|---|
| D-001 | Zero algorithm dependencies. `wasm-bindgen`/`web-sys`/`std` stay. Enforced by `scripts/check-zero-deps.py` in CI. |
| D-002 | The CPU rasterizer is the oracle, and it is written first. Every GPU render is diffed against it per-pixel. |
| D-003 | WebGL2 first, WebGPU second. WebGL2 has no compute shaders; the rasterizer design must live within that. |
| D-004 | The scene graph lives in WASM linear memory, never in JS. |
| D-005 | DOM for chrome, canvas for artboard only. |
| D-006 | Tiling, culling and cache invalidation are the product. |
| D-007 | Undo is a command log, not snapshots. |
| D-008 | Own scene graph is the document; SVG is import/export. |
| D-009 | Text is last, and is the decision to revisit if the project stalls. |
| D-010 | `hane-gpu` emits draw data; `hane-wasm` submits it. The GL API is never called from `hane-gpu`. |
| D-011 | The GPU rasterizer runs the CPU oracle's own coverage loop in a fragment shader, not stencil-then-cover and not signed-area accumulation. |
| D-012 | The WebGPU backend keeps that loop unchanged and spends compute on what surrounds it: one storage-buffer framebuffer, one workgroup per tile, one pipeline, no per-draw copy. |

---

## D-010 — `hane-gpu` emits draw data, `hane-wasm` submits it

D-001 exempts only `hane-wasm` from the dependency ban, but `hane-gpu` needs WebGL — a
contradiction that would otherwise stall all of P2.

Resolved by splitting along the data/effect line rather than widening the exemption:

- **`hane-gpu`** owns everything that can be decided without a GL context: tile binning, coverage
  computation, vertex and index buffer contents, uniform values, and the ordered list of draw
  commands. It has no dependencies, calls no GL, and is tested natively with `cargo test`.
- **`hane-wasm`** owns the context: it holds `web-sys`, creates buffers and programs, uploads the
  bytes `hane-gpu` produced, and issues the draws.

This is the same instinct as D-002. A renderer whose logic can only be exercised inside a browser
with a live GPU is a renderer you cannot debug, and P2 is hard enough already. Keeping the
decisions in a pure, natively-testable crate means the per-pixel diff against `hane-raster` can
run in CI on the parts that actually contain the bugs.

The cost is one extra hop for every draw. That is a real cost and it is accepted: an indirection
per draw call is nothing next to being unable to unit-test the tile binner.

---

## D-011 -- the fragment shader runs the oracle's algorithm

D-003 chose WebGL2 and left the rasterizer's shape open: with no compute shaders, it is
stencil-then-cover or coverage in a fragment shader. #19 had to pick one.

The criterion that decided it is not throughput. It is that P2's acceptance is a per-pixel diff
against `hane-raster` (D-002) over a corpus built to be hostile: `pentagram` has a winding-2
region meeting a winding-0 one inside a single pixel, `nested_triangles` reaches winding 3,
`annulus_reverse_winding` and `annulus_same_winding` are the same geometry with opposite correct
answers.

- **Signed-area accumulation**, the usual GPU answer, integrates the winding number over a pixel.
  That equals coverage only where the winding is 0 or 1, and at each of the pentagram's five
  crossing vertices it is out by up to a quarter of a pixel -- around 60 counts out of 255.
- **Stencil-then-cover** is exact about the winding and has no anti-aliasing without MSAA, which
  `glctx.rs` disables on purpose: a second, differently quantised anti-aliasing puts the output
  permanently out of the oracle's reach.
- **Running the oracle's loop per pixel** -- sixteen sample lines, the crossings inside that pixel
  sorted, analytic horizontal spans -- is the same computation in `f32`, so the only thing left to
  differ is the mantissa.

Measured, that is 41 of 41 comparable fixtures within max 1 count and mean 0.012, in Chromium 150
on SwiftShader and Firefox 153, with 30 of them bit-exact. `BENCHMARKS.md` has the table.

The cost is real: sorting a handful of crossings per fragment is more work than either
alternative, and it is paid for by binning edges per 16-pixel tile. It is accepted because a
renderer that cannot reproduce the oracle is not a faster renderer, it is a different picture.

---


## D-012 -- WebGPU spends its compute shaders on the submit path, not the rasterizer


D-003 promised a second backend and #24 asked the obvious question: compute shaders open up
designs WebGL2 cannot express, so which one does the second backend use?

None of them, for the rasterizer. D-011's argument was never about the shader stage. It was about
a corpus where `pentagram` puts a winding-2 sector and a winding-0 one inside one pixel, and
where the acceptance criterion is a per-pixel diff against `hane-raster`. Signed-area
accumulation is still out by ~60 counts there whether it runs in a fragment shader or a
workgroup, and a compute-binned sparse-strip renderer would be a *third* set of rounding to
reconcile with the oracle. So `wgpurender.rs`'s coverage loop is a line-for-line port of
`glrender.rs`'s: sixteen sample lines, sorted crossings, analytic spans.

What compute changed is the submit path, and there it changed nearly everything:

- **The framebuffer is a storage buffer of packed RGBA8.** An invocation reads and writes its own
  pixel, so the full-target blit WebGL2 forced *before every single draw* -- a fragment shader
  cannot read the pixel it is about to write -- is gone. That was the standing `ponytail:` note
  at the top of `glrender.rs`, and it is now deleted rather than deferred.
- **The 8-bit round trip goes with it.** The GL path rounded, divided by 255 and trusted the
  driver to store the byte back unchanged. Here the byte *is* the value.
- **One workgroup is one tile.** `TILE_SIZE` is 16 and a 16x16 workgroup is 256 invocations, so
  the tile instances `hane-gpu` already emits are the dispatch grid. No vertex stage, no quad.
- **One pipeline.** Clip reset, clip path, fill, group push and group pop are five modes of one
  entry point, against three programs, two vertex shaders and a fixed-function blend state.

`hane-gpu` is untouched (D-010): both backends consume the same `DrawData`, which is what lets
one tolerance table judge both. Measured on the same corpus, with the same
`cargo test -p hane-gpu --test oracle_diff`:

| | fixtures | bit-exact | worst max | worst mean |
|---|---:|---:|---:|---:|
| WebGL2, Chromium 150 | 41 | 31 | 1 | 0.0060 |
| WebGPU, Chromium 150 / Dawn on SwiftShader | 41 | 33 | 1 | 0.0060 |
| WebGPU, Firefox 153 / wgpu | 41 | 30 | 1 | 0.0114 |

Backend selection is automatic -- WebGPU when `requestAdapter` gives one, WebGL2 otherwise -- and
`hane_backend("webgl2" | "webgpu")` is the override, refusing a name it cannot honour rather than
quietly substituting the other. A benchmark that reported the wrong backend's number would be
worse than no override at all.

The cost is a build flag: web-sys keeps its WebGPU bindings behind `--cfg=web_sys_unstable_apis`,
which has to reach web-sys itself and therefore lives in `.cargo/config.toml` as a rustflag for
the whole workspace. It is accepted because the alternative is hand-rolling the same bindings
through `js_sys::Reflect`, which is the same code with no type checking.
