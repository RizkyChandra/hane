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
