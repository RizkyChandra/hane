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
