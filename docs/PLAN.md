# hane — a vector design engine for the web

## Context

Affinity Designer was the best vector tool available, it never came to Linux, and post-Canva
its future is uncertain. **hane** (羽 feather; 跳ね, the flick that ends a brush stroke) is a
replacement built as a web app so platform stops mattering. It keeps what made Affinity good:
speed, no modal dialogs, live everything.

Decided:

| | |
|---|---|
| Target | **Web only.** No desktop. Tauri wrapper later if ever. |
| Language | **Rust → WASM**, DOM shell in TypeScript |
| Scale | **100k+ objects, Figma-scale** |
| Dependencies | **Zero algorithm dependencies.** Every algorithm is ours. |
| Renderer | **Ours, from day one.** |
| UI chrome | **DOM/HTML.** Engine owns one `<canvas>`. |

The zero-dependency and own-renderer calls were made deliberately with the costs on the table.
They change what this is: **not an editor project that needs an engine, but an engine project
that ends in an editor.** The plan below is structured accordingly.

Location: `/home/yoshirakou/work/dev/hane`. Greenfield.

### Honest timeline

Solo, this is **2–3 years** to something comparable to Affinity's core. Three phases (GPU
rasterizer, boolean ops, text) are each capable of eating six months alone. The phase ordering
below is chosen so that a **real, demoable thing exists at ~6 months** rather than at the end.

---

## Architecture

```
┌─ Browser tab ────────────────────────────────────────────┐
│                                                          │
│  TS / DOM shell        panels · toolbars · menus         │
│                        inputs · dialogs · a11y           │
│                        (owns NO scene state)             │
│           ↕  commands in / view-models out               │
│  ┌─ hane, Rust → WASM ─────────────────────────────────┐ │
│  │  scene    arena, generational ids, spatial index    │ │
│  │  edit     tools, transforms, undo command log       │ │
│  │  path     stroke expansion, dashes, boolean ops     │ │
│  │  geom     bezier math, affine, flattening           │ │
│  │  raster   CPU reference rasterizer — the oracle     │ │
│  │  gpu      tile rasterizer, WebGL2 → WebGPU          │ │
│  └─────────────────────────────────────────────────────┘ │
│  <canvas>                                                │
└──────────────────────────────────────────────────────────┘
```

### Crate layout

```
crates/
  hane-geom/     Point, Vec2, Affine, Rect. Quad/cubic bezier: eval, derivative,
                 de Casteljau split, tight bbox, adaptive flattening, arc length,
                 curve–curve intersection.
  hane-path/     Path elements, subpaths, winding. Stroke expansion, joins, caps,
                 dashes. Boolean ops.
  hane-raster/   CPU scanline AA rasterizer. The correctness oracle.
  hane-gpu/      Tile-based GPU rasterizer. WebGL2 first, WebGPU later.
  hane-scene/    Scene graph arena, spatial index, tile cache, culling, dirty tracking.
  hane-svg/      XML parser (namespace-aware), SVG path grammar, import + export.
  hane-edit/     Tools, selection, hit testing, transforms, undo command log.
  hane-text/     OpenType parsing, shaping, layout.
  hane-wasm/     wasm-bindgen surface, GL context, event plumbing.
web/             TS + Vite shell.
```

**Every crate except `hane-wasm` has an empty `[dependencies]`.** That is the zero-dep decision
expressed as something CI can check, not a matter of discipline — see Verification.

---

## Decisions

ADR-style and numbered, matching the `DECISION-LOG.md` convention already used in `sable`.

**D-001 — Zero algorithm dependencies; `wasm-bindgen`/`web-sys`/`std` stay.**
Every algorithm is ours. The browser binding layer is generated FFI glue for WebGL, canvas and
events — not logic — and stays. `std` stays.

**D-002 — The CPU rasterizer is the oracle, and it is written first.**
`hane-raster` is a straightforward scanline AA rasterizer, optimised for being *obviously
correct* rather than fast. Every `hane-gpu` output is diffed against it per-pixel in CI. This
is the single most important decision in the plan: **a from-scratch GPU rasterizer with no
reference implementation is undebuggable.** `sable` reached the same conclusion from the other
direction (D-007, "CPU compositing, GPU for display only").

**D-003 — WebGL2 first, WebGPU second.**
WebGPU is ~82% globally and **Firefox on Linux still lacks it**. WebGL2 is the floor, and it
has no compute shaders — which rules out the modern sparse-strip and compute-binning designs
and pushes us toward stencil-then-cover or fragment-shader coverage accumulation. Choosing
this constraint up front avoids designing a rasterizer we can't ship.

**D-004 — Scene graph lives in WASM linear memory, never in JS.**
Arena with generational `u32` ids. TS holds ids only, sends commands, and receives small
view-models (current selection; the ~40 layer rows actually visible). At 100k objects you
cannot marshal a tree across the boundary per frame.

**D-005 — DOM for chrome, canvas for artboard only.**
Panels, toolbars, menus and text inputs are HTML. This buys native text entry, IME,
accessibility, focus and scrolling — a large pile we would otherwise write ourselves, in a
sandbox where it's especially painful. Figma and Penpot both do this.

**D-006 — Tiling, culling and cache invalidation are the product.**
At 100k objects the rasterizer's inner loop barely matters; what matters is never touching the
99% of objects that are off-screen or unchanged. Own quadtree over document-space bboxes,
tuned for heavy incremental update during drag.

**D-007 — Undo is a command log, not snapshots.**
Every edit is a reversible command. Snapshotting a 100k-node document per undo step is dead on
arrival. Also leaves a clean seam if multiplayer ever happens.

**D-008 — Own scene graph is the document; SVG is import/export.**
Native format is our own serialization. SVG import is lossy by nature. SVG export is ours.
SVG-native was rejected: an XML DOM at 100k nodes is not viable, and it cannot carry live
boolean ops or non-destructive effects.

**D-009 — Text is last, and it is the one to reconsider if the project stalls.**
Latin-only shaping is achievable solo. Full Unicode — bidi, complex scripts, mark positioning,
font fallback — is not, and this is the piece where writing it ourselves has the worst ratio of
effort to product value. Deliberately sequenced last so that decision can be made with the rest
of the engine already working.

---

## Phases

### P0 — Geometry · `hane-geom` · ~3–4 weeks

`Point`, `Vec2`, `Affine` (2×3), `Rect`. Quadratic and cubic beziers: evaluation, derivative,
de Casteljau split, **tight** bbox via derivative roots, adaptive error-bounded flattening,
arc length by Gauss–Legendre. Path element representation.

Pure math, no I/O, fully testable natively. Everything downstream sits on this, so it gets the
most test coverage per line in the project.

### P1 — CPU rasterizer · `hane-raster` · ~3–4 weeks

Scanline anti-aliased fill, nonzero and even-odd winding. Solid colour, then linear and radial
gradients. Correctness over speed — this becomes the oracle for everything after it.

Direct prior art: `sable/engine/src/select.cpp` is already a nonzero-winding polygon rasterizer
with analytic horizontal coverage and 4× vertical AA. Port the approach.

### P2 — GPU rasterizer · `hane-gpu` · ~8–12 weeks · **hardest of the three**

WebGL2, tile-based. Bin paths into tiles, accumulate coverage per tile, composite. Realistic
approaches given no compute shaders: stencil-then-cover, or fragment-shader coverage
accumulation into a coverage texture.

Gated the whole way by per-pixel diff against P1. Expect to spend most of this phase on
conflation artifacts, correct winding at tile boundaries, and clipping.

### P3 — Scene, tiling, culling · `hane-scene` · ~4–6 weeks · **first demo**

Arena and generational ids. Own quadtree over document-space bboxes. Viewport culling, tile
cache, dirty tracking. Pan/zoom/rotate view transform — port `sable/app/src/view_transform.hpp`.

**Gate: 100k filled shapes, p99 frame time under 16ms, in Chrome and in Firefox on Linux.**
This is the thesis of the whole project. If it doesn't hold here, everything after it is built
on sand — stop and fix it before continuing.

> **~6 month mark.** A genuinely fast viewer of 100k filled vector shapes. Not an editor yet,
> but real, demoable, and proof the hard part works.

### P4 — Strokes · `hane-path` · ~6–8 weeks

Offset curves, joins (miter/round/bevel), caps, dash patterns, stroke alignment. Harder than it
sounds: cusps, self-intersection in offsets, and tight-curvature blowup are the whole problem.

### P5 — Editor · `hane-edit` · ~8–12 weeks

Selection (click, shift-add, marquee), transform box with scale/rotate/skew, snapping, pen
tool, node and handle editing, groups, z-order, undo/redo. Hit testing via quadtree broad phase
plus exact path test. `sable/engine/src/linework.cpp` has nearest-control-point hit testing and
adaptive sampling worth porting.

> **This is where it becomes an editor.**

### P6 — Boolean ops · `hane-path` · ~8–12 weeks · **hardest single algorithm**

Curve–curve intersection (bezier clipping or recursive subdivision), planar subdivision or
sweep line, winding-number classification. Union, subtract, intersect, divide, with live
preview — the Affinity feature most worth stealing.

The difficulty is entirely robustness: coincident edges, degenerate curves, near-tangent
intersections, and floating-point classification near boundaries. Budget accordingly; this is
the phase most likely to overrun.

### P7 — SVG · `hane-svg` · ~3–4 weeks

Namespace-aware XML parser, SVG path `d` grammar, transforms, styles, groups, gradients.
Import and export. Note `sable/engine/src/xml.hpp` explicitly documents that it handles no
namespaces and no entities — this needs a real parser, which its own header says is the right
call at the third format.

### P8 — Text · `hane-text` · ~4–6 months

OpenType table parsing (`cmap`, `glyf`/`loca`, `hmtx`, `head`, `hhea`, `maxp`, `kern`, `GSUB`,
`GPOS`), glyph outline extraction into our path type, shaping with ligatures and kerning,
line breaking, grapheme clustering. Latin first.

Full bidi and complex scripts are explicitly out of scope — see D-009.

---

## Verification

**The zero-dep rule is enforced, not trusted.** A CI check asserts every crate except
`hane-wasm` resolves to zero external dependencies. A dependency can only enter by someone
deliberately editing that check.

**The oracle is the backbone.** Every `hane-gpu` render is diffed per-pixel against
`hane-raster` on a fixture corpus, tolerance-gated, in CI. This is what makes P2 tractable at
all.

**Golden images.** Fixture documents render to committed PNGs, compared with a tolerance.
Standard practice for graphics engines and the only practical regression net for rasterization.

**Unit tests per crate**, native, no browser, fast. Geometry invariants get a hand-rolled
deterministic fuzzer with a fixed-seed LCG (zero-dep means no `proptest`): split-then-rejoin
equals the original, flattening respects its error bound, boolean ops conserve area on
disjoint inputs, undo/redo round-trips.

**Benchmark page** reporting p50/p99 frame time plus encode time and tile-cache hit rate at
1k / 10k / 100k / 500k objects, over a scripted pan+zoom. Run in Chrome (WebGPU) and Firefox on
Linux (WebGL2). Recorded in `BENCHMARKS.md`; this is the one metric that must never silently
regress.

**Per phase**, one end-to-end check of the headline capability. P5's is "select three shapes,
rotate them, undo twice, redo once."

---

## Prior art to port from `sable`

Algorithms transfer even though it's C++ → Rust:

| Source | Use |
|---|---|
| `engine/src/select.cpp` | Nonzero-winding AA polygon rasterizer, analytic horizontal + 4× vertical AA → **P1** |
| `app/src/view_transform.hpp` | Pan/zoom/rotate screen↔canvas transform, round-trip tested → **P3** |
| `engine/src/linework.cpp` | Adaptive curve sampling, nearest-control-point hit testing → **P5** |
| `docs/DECISION-LOG.md` | The ADR convention itself; D-030 gradients (premultiplied interpolation, ordered dither) |

---

## Risks

| Risk | Reality |
|---|---|
| Three phases can each eat 6 months | P2, P6 and P8 are all genuinely hard. The ordering puts a demoable result at P3, before any of them except P2. |
| WebGL2 has no compute shaders | Rules out the sparse-strip and compute-binning designs that make modern renderers fast. Stencil-then-cover is the realistic fallback. Locked in at D-003 so it constrains the design rather than surprising it. |
| Boolean op robustness | The hardest problem here. Note that kurbo's authors — domain experts — have had this open for years. Expect P6 to overrun. |
| Text scope | See D-009. The piece most likely to justify revisiting the zero-dep rule. |
| No proptest / criterion | Hand-rolled deterministic harnesses instead. Small cost, but real. |
| Prior art exists | [Graphite](https://graphite.rs) and [Penpot](https://penpot.app) are open-source Rust/WASM vector editors. Worth reading their renderers before P2 even though we're not depending on them. |

---

## Execution

Repo: **https://github.com/RizkyChandra/hane** — exists, public, empty. Authed as `RizkyChandra`
with `repo` scope.

### Step 1 — Bootstrap (sequential, blocks everything)

Cargo workspace, the nine crate skeletons from the layout above, `web/` Vite shell, and CI —
including the **zero-dependency check** from D-001, which must exist before any code so it can
never be quietly violated. Plus `hane-geom`'s primitive types (`Point`, `Vec2`, `Affine`,
`Rect`, `PathEl`), because every other issue imports them.

Committed and pushed to `main` before any issue work starts.

### Step 2 — Milestones, labels, issues

Milestones `P0`–`P8` matching the phases. Labels: `crate:*`, `agent-ready`, `needs-human`,
`hard`. Roughly 55 issues, each with explicit acceptance criteria.

| Milestone | Issues | Notes |
|---|---|---|
| P0 geometry | ~10 | Mostly `agent-ready` — well-specified math with testable invariants |
| P1 CPU raster | ~6 | Mixed |
| P2 GPU raster | ~8 | `needs-human` · `hard` |
| P3 scene/tiling | ~7 | Mixed |
| P4 strokes | ~5 | `hard` |
| P5 editor | ~9 | Mostly `agent-ready` |
| P6 booleans | ~5 | `needs-human` · `hard` |
| P7 SVG | ~5 | `agent-ready` |
| P8 text | ~6 | `hard` |

### Step 3 — Agent waves

Agents run with **worktree isolation** so parallel work doesn't collide, one issue each, each
opening a PR against `main`.

**Wave A — 4 agents, all inside `hane-geom`, separate modules:**
1. Bezier evaluation, derivatives, de Casteljau split
2. Tight bbox via derivative roots + adaptive error-bounded flattening
3. Arc length (Gauss–Legendre), parameterization utilities
4. Deterministic fixed-seed LCG fuzz harness + geometry invariant tests

**Wave B — 4 agents, genuinely independent crates:**
1. `hane-raster` — CPU scanline AA fill, nonzero + even-odd *(depends on Wave A)*
2. `hane-svg` — namespace-aware XML parser + SVG path `d` grammar *(needs only Step 1 types)*
3. `hane-scene` — arena, generational ids, quadtree over `Rect` *(needs only Step 1 types)*
4. Golden-image harness + benchmark page scaffold *(independent)*

### What does not parallelize

Worth being straight about: **the phases are a dependency chain, not a work queue.** P1 needs
P0's types; P2 needs P1 as its oracle; P3 needs P2. Fanning agents across P0–P3 at once would
mostly produce agents blocked on each other.

And the three hard phases — **P2 (GPU rasterizer), P6 (boolean ops), P8 (text)** — are
research-grade work with tight feedback loops and judgment calls on numerical robustness. Those
are yours. Agents are genuinely useful around them: fixture corpora, golden images, test
harnesses, benchmark scaffolding, porting the `sable` algorithms. They are not useful *inside*
them, and issues there are labelled `needs-human` for that reason.

Realistically: agents carry P0, P5, P7 and most of P1 and P3. That is still a large fraction of
the two years.

---

## First step

Bootstrap the repo and CI (Step 1), then create milestones and issues (Step 2), then launch
Wave A. **P0 — `hane-geom`** is the real technical start: bezier math, affine transforms,
flattening, tight bboxes, and the test harness everything above it leans on for the next two
years.
