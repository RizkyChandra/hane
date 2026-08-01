# Benchmarks

Numbers here are medians, produced by hand-rolled harnesses -- D-001 rules out `criterion`. The
exception is the P3 gate (#31), which is stated as a p99 and is reported as percentiles.
Every harness lives in an `examples/` directory of the crate it measures and is run with
`cargo run --release`; a debug build measures the optimiser, not the code.

Re-run a harness before trusting a number on a different machine. Absolute times are hardware;
the ratios are the finding.

---

## Spatial index: quadtree vs linear scan (#26, #27)

```sh
cargo run --release --example spatial_bench
```

**Verdict: the quadtree wins and ships. The linear scan is not part of `hane-scene`** -- it
exists only inside `crates/hane-scene/examples/spatial_bench.rs`, as the thing being measured
against and as the oracle that asserts the tree returns the same ids. There is no second index
in the library and no switch between them.

That was not the obvious outcome. The scan beats the tree at everything except querying, and it
wins the full drag frame outright below ~20k items.

### What was measured

- **Data.** `n` boxes of 4-40 units, uniform over a document that grows with `n` so density is
  constant. Both implementations get the identical item list, from `hane_geom::fuzz::Rng`.
- **Viewport query.** A fixed 1280x720 rectangle. It returns a roughly constant ~500 items at
  every `n`, which is the real culling workload: the screen does not get bigger when the
  document does.
- **Quarter-document query.** A rectangle a quarter of the document on a side, so the answer is
  a large fraction of the items and the tree cannot win on selectivity alone.
- **Move one item.** Remove at the old box, insert at the new one, one pixel away. Measured in
  batches of 500 scattered items -- the pessimistic case, since a real selection is usually
  spatially clustered and would keep the tree's descents in cache.
- **Drag frame.** 500 moves plus one viewport cull, the workload D-006 is tuned for. Derived
  from the two medians above, since it is their sum.
- **Build.** Bulk load, repeated insertion, and filling the scan's array. Dropping the result
  happens after the clock is read.

Machine: AMD Ryzen 9 9950X3D, rustc 1.97.1, `opt-level = 3`, `lto = true`. Run-to-run spread
across repeated runs was under 3%.

Ratios are scan/quadtree: **above 1.00 the quadtree wins.**

### n = 1000

Document 1265 x 1265. Viewport query returns 221 items, quarter-document query returns 55.

| measurement | quadtree | linear scan | ratio |
|---|---:|---:|---:|
| build (us) | 10.9 bulk / 11.5 repeated insert | 0.7 | 0.06x |
| viewport query (us) | 0.32 | 2.05 | 6.40x |
| quarter-document query (us) | 0.15 | 3.06 | 19.77x |
| move one item (ns) | 43.3 | 0.4 | 0.01x |
| drag frame: 500 moves + 1 viewport query (us) | 22.0 | 2.3 | 0.10x |

### n = 10000

Document 4000 x 4000. Viewport query returns 420 items, quarter-document query returns 456.

| measurement | quadtree | linear scan | ratio |
|---|---:|---:|---:|
| build (us) | 141.1 bulk / 168.0 repeated insert | 6.3 | 0.04x |
| viewport query (us) | 0.88 | 37.97 | 43.04x |
| quarter-document query (us) | 1.58 | 38.80 | 24.53x |
| move one item (ns) | 119.9 | 0.4 | 0.00x |
| drag frame: 500 moves + 1 viewport query (us) | 60.8 | 38.2 | 0.63x |

### n = 30000

Document 6928 x 6928. Viewport query returns 497 items, quarter-document query returns 1557.

| measurement | quadtree | linear scan | ratio |
|---|---:|---:|---:|
| build (us) | 485.9 bulk / 658.5 repeated insert | 22.1 | 0.05x |
| viewport query (us) | 2.63 | 105.26 | 40.09x |
| quarter-document query (us) | 4.78 | 116.87 | 24.44x |
| move one item (ns) | 173.1 | 0.4 | 0.00x |
| drag frame: 500 moves + 1 viewport query (us) | 89.2 | 105.5 | 1.18x |

### n = 100000

Document 12649 x 12649. Viewport query returns 549 items, quarter-document query returns 5021.

| measurement | quadtree | linear scan | ratio |
|---|---:|---:|---:|
| build (us) | 1850.5 bulk / 2533.0 repeated insert | 79.5 | 0.04x |
| viewport query (us) | 3.67 | 322.29 | 87.86x |
| quarter-document query (us) | 12.19 | 387.42 | 31.79x |
| move one item (ns) | 287.5 | 0.5 | 0.00x |
| drag frame: 500 moves + 1 viewport query (us) | 147.4 | 322.5 | 2.19x |

### Crossover

**Query: no crossover.** The quadtree is ahead at every size measured, from 6x at 1k to 88x at
100k, and the gap widens because the scan is linear in `n` while the tree is linear in the
answer. Even the quarter-document query, where the answer is 5% of the document, is 32x.

**Update: no crossover, the other way.** A move costs the tree 43-288ns against the scan's
0.4ns -- 100x to 600x. The scan's update is free by construction: it maintains no index, so
there is nothing to maintain. Its cost was already paid, in the query.

**Drag frame: crossover between 10k and 30k, near 20k items.** Below it, 500 moves cost the
tree more than a whole linear cull costs the scan. Above it, the scan's per-frame cull grows
past the tree's fixed update bill and never comes back: at 100k the tree is 2.2x ahead, at 500k
it would be roughly 10x.

**Build: no crossover.** Filling an array beats building a tree by 16-26x at every size, and
always will. It is paid once per document load, 1.9ms at 100k, so it does not decide anything.
Bulk load beats repeated insertion by 1.04x at 1k, 1.19x at 10k and 1.37x at 100k -- the gap
grows because insertion re-walks from the root for every item while the partition build touches
each item once per level, in order.

### Why the quadtree ships anyway

The scan wins the drag frame at 1k and 10k, and `PLAN.md` targets 100k+ objects at a 16ms p99.
At 100k the scan spends 322us per cull. That is 2% of the frame budget for one viewport query,
before anything is drawn, on every frame whether or not anything moved -- and tile invalidation
means culling is not once per frame. The tree spends 3.7us. At 500k, the other number in the
plan's perf gate, the scan alone would be over 1.6ms per cull.

The drag advantage is also the softest of the three numbers. It assumes 500 scattered items
move every frame; a clustered selection keeps the tree's descents in cache, and the tree's
`update` is currently two independent descents where one fused walk would do (see the
`ponytail:` note on `Quadtree::update`). Halving update cost moves the crossover to ~10k.

The honest summary: below 10k items neither structure matters -- both answer in microseconds,
and the scan is an order of magnitude less code. The quadtree earns its place at the scale hane
was specified for, and nowhere else.

### Things these numbers do not cover

- One thread. No measurement of a parallel cull.
- Uniform density. A document with everything piled in one corner drives every item to the
  depth cap and turns queries into scans of one node's list; the correctness tests cover it,
  the benchmark does not.
- The scan's query writes ids into a `Vec` exactly as the tree's does, so neither gets credit
  for a cheaper output path.

---

## Tile binning: what should `TILE_SIZE` be? (#18)

```sh
cargo run --release --example bin_bench
```

**Verdict: 16 px, and the honest version is that 8 and 16 are within 10% of each other.** The
combined model bottoms out at 8 for dense scenes and at 16 for sparse ones, which is a tie. 16
wins it on the two costs the model does not see: it emits half as many bin entries to upload
per frame, and it has a quarter as many tiles for the P3 cache and for whatever per-tile state
D-003 forces on a WebGL2 rasterizer. 4 px and 32 px and up are not close.

Only half of the trade-off is measurable without a GPU, and the two halves point opposite ways:

- **Binning, measured.** Falls monotonically as tiles grow -- a bigger tile means fewer
  `(tile, segment)` entries per segment and a smaller grid to sweep. On its own it says "128".
- **Fill, modelled.** WebGL2 has no compute shaders (D-003), so a tile is drawn by running its
  whole segment list over its whole pixel area: `entries * size^2` segment-pixels. It rises
  with the tile, because a bigger tile pulls in segments that miss most of its pixels. On its
  own it says "4".

The `sum` column adds the two normalised at the 16 px point. That weighting is a choice, not a
measurement, and it is the weakest number on this page -- which is exactly why the 8-vs-16 call
was made on entry count and tile count instead.

### What was measured

- **Scene.** Closed 8-segment blobs, 8 to 200 px across, scattered over a 1920x1080 artboard: a
  third lines, a third quadratics, a third cubics. That is what an illustration looks like after
  culling -- many small shapes rather than a few screen-spanning ones. A screen-spanning path is
  the case where the tile size stops mattering, since it lands in every tile whatever the size.
- **Flattening tolerance** 0.5 device px, the same one a rasterizer would draw with. Bins are
  exact for the polyline a curve flattens to, so the two have to agree.
- **Segment counts** 1k, 10k and 50k. 50k covers every one of the 8160 tiles at 16 px; it is the
  pessimistic end, not the expected one.
- **`entries`** is the total `(tile, segment)` pairs -- the size of the buffer that goes to the
  GPU. **`non-empty tiles`** is the number of tiles anything is drawn into; empty tiles are not
  in the output at all and cost nothing downstream.

Machine and build as above: AMD Ryzen 9 9950X3D, `opt-level = 3`, `lto = true`. Two full runs
produced byte-identical bins and timings within 3%.

### 1000 segments

| tile | bin (us) | entries | entries/seg | non-empty tiles | segs/tile | segment-pixels (model) |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 632.8 | 33331 | 33.33 | 22026 | 1.51 | 0.5M |
| 8 | 369.2 | 16642 | 16.64 | 9029 | 1.84 | 1.1M |
| 16 | 190.1 | 8512 | 8.51 | 3499 | 2.43 | 2.2M |
| 32 | 168.5 | 4546 | 4.55 | 1259 | 3.61 | 4.7M |
| 64 | 160.4 | 2597 | 2.60 | 410 | 6.33 | 10.6M |
| 128 | 157.7 | 1755 | 1.75 | 127 | 13.82 | 28.8M |

Flattening alone: 73.6 us for 9414 polyline points. Relative to 16 px, lower is better:

| tile | bin cost | fill cost (model) | sum |
|---:|---:|---:|---:|
| 4 | 3.33x | 0.24x | 3.57x |
| 8 | 1.94x | 0.49x | 2.43x |
| 16 | 1.00x | 1.00x | **2.00x** |
| 32 | 0.89x | 2.14x | 3.02x |
| 64 | 0.84x | 4.88x | 5.73x |
| 128 | 0.83x | 13.20x | 14.03x |

### 10000 segments

| tile | bin (us) | entries | entries/seg | non-empty tiles | segs/tile | segment-pixels (model) |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 5408.8 | 312513 | 31.25 | 106521 | 2.93 | 5.0M |
| 8 | 3767.7 | 156655 | 15.67 | 30763 | 5.09 | 10.0M |
| 16 | 2849.2 | 80154 | 8.02 | 8095 | 9.90 | 20.5M |
| 32 | 2318.5 | 42787 | 4.28 | 2040 | 20.97 | 43.8M |
| 64 | 1995.4 | 24772 | 2.48 | 510 | 48.57 | 101.5M |
| 128 | 1769.3 | 16744 | 1.67 | 135 | 124.03 | 274.3M |

Flattening alone: 758.9 us for 91994 polyline points. Relative to 16 px, lower is better:

| tile | bin cost | fill cost (model) | sum |
|---:|---:|---:|---:|
| 4 | 1.90x | 0.24x | 2.14x |
| 8 | 1.32x | 0.49x | **1.81x** |
| 16 | 1.00x | 1.00x | 2.00x |
| 32 | 0.81x | 2.14x | 2.95x |
| 64 | 0.70x | 4.94x | 5.65x |
| 128 | 0.62x | 13.37x | 13.99x |

### 50000 segments

| tile | bin (us) | entries | entries/seg | non-empty tiles | segs/tile | segment-pixels (model) |
|---:|---:|---:|---:|---:|---:|---:|
| 4 | 27013.0 | 1570916 | 31.42 | 129548 | 12.13 | 25.1M |
| 8 | 19581.0 | 786995 | 15.74 | 32400 | 24.29 | 50.4M |
| 16 | 14822.1 | 403080 | 8.06 | 8160 | 49.40 | 103.2M |
| 32 | 12094.5 | 215022 | 4.30 | 2040 | 105.40 | 220.2M |
| 64 | 10287.8 | 124760 | 2.50 | 510 | 244.63 | 511.0M |
| 128 | 9359.7 | 84554 | 1.69 | 135 | 626.33 | 1385.3M |

Flattening alone: 4110.2 us for 459732 polyline points. Relative to 16 px, lower is better:

| tile | bin cost | fill cost (model) | sum |
|---:|---:|---:|---:|
| 4 | 1.82x | 0.24x | 2.07x |
| 8 | 1.32x | 0.49x | **1.81x** |
| 16 | 1.00x | 1.00x | 2.00x |
| 32 | 0.82x | 2.13x | 2.95x |
| 64 | 0.69x | 4.95x | 5.65x |
| 128 | 0.63x | 13.43x | 14.06x |

### How the numbers scale

**Binning is linear in the entries it emits, not in the segments it is given.** Entries per
segment is a property of the tile size and the scene alone -- 8.0 at 16 px at every count
measured, 31.3 at 4 px, 1.7 at 128 px -- so the entry column is stable across `n` and the time
column tracks it. At 16 px that is 190us for 1k, 2.85ms for 10k, 14.8ms for 50k: roughly 285ns
per segment or 36ns per entry, flat.

**About a quarter of it is flattening**, which is work the rasterizer needs done anyway and is
identical at every tile size. It is the floor the bin column cannot fall below however big the
tiles get, which is most of why 128 px is only 1.6x faster than 16 px rather than 5x.

**The per-frame budget is the finding, not the tile size.** `PLAN.md` allows 16ms p99. 10k
visible segments costs 2.85ms, 18% of the frame, before a single pixel is filled. 50k costs
14.8ms and blows it outright. Binning is not something to run from scratch every frame at that
scale, which is the whole argument for the P3 tile cache: re-bin what moved, keep the rest.

### Things these numbers do not cover

- **Fill is a model, not a measurement.** `entries * size^2` assumes a tile costs its whole
  pixel area per binned segment. A rasterizer that bounds each segment inside the tile first
  would flatten that curve and push the answer towards smaller tiles.
- **Draw submission is not counted at all.** If the rasterizer ends up one draw per non-empty
  tile rather than one instanced draw for all of them, the tile column moves the answer up, not
  down: 30763 draws at 8 px against 8095 at 16 px, at 10k segments.
- One thread, one artboard size, one flattening tolerance. Zooming in raises the segment count
  per tile without raising the segment count, which this does not separate.
- Uniform scatter. A document with everything piled into one corner leaves most tiles empty and
  the rest saturated; the correctness tests cover it, the benchmark does not.
## Viewport culling and the tile cache (#28, #30)

```sh
cargo run --release --example tile_bench
```

**Verdict: both criteria hold with two orders of magnitude to spare on the cull, and the tile
cache's hit rate is decided by the edit pattern, not by the document size.** Culling a 100k
scene to 2395 visible items costs 5.6us against a budget of 1000us. The cache holds 98% over a
scripted pan and zoom, and it is worth knowing that a stream of *scattered* edits is what takes
that apart -- 20 of them a frame drops it to 62%, while the same 20 edits in one dragged
selection stay at 93%.

Machine and build as above: AMD Ryzen 9 9950X3D, `opt-level = 3`, `lto = true`.

### What was measured

- **Cull.** `View::visible_bounds` plus one `Quadtree::query`, over 64 viewports along a
  diagonal sweep so that no two measured frames answer the same query. Scene generation is
  identical to `spatial_bench` -- `n` boxes of 4-40 units over a document that grows with `n`
  -- so the two tables describe the same documents.
- **Rotated.** The same viewport turned 30 degrees. The query rect is the bounding box of the
  rotated viewport (#28), which is why the item count roughly doubles: that is the cost of the
  design, measured rather than argued.
- **2560x1440.** Four times the area, which at this density is what it takes to reach the
  ~2k visible items #28's criterion names.
- **Tile cache.** 300 frames of a steady drag with a 1.0078x per-frame zoom under it, crossing
  a power-of-two level boundary every ~90 frames. 128px overdraw, 64 MiB budget, 256x256 RGBA8
  tiles. Nothing is rendered: a miss allocates a tile-sized buffer and stores it, because what
  a render costs belongs to #31 and mixing it in would bury the hit rate.
- **Edits.** Each edit invalidates both the box the object left and the box it arrived at, and
  every edit lands inside the visible region -- an off-screen edit dirties nothing resident and
  would flatter the hit rate for free.

### Culling

| n | viewport | items returned | time (us) |
|---:|---|---:|---:|
| 1000 | 1280x720, no margin | 156 | 0.23 |
| 1000 | 1280x720, 256px overdraw | 301 | 0.39 |
| 1000 | 1280x720, rotated 30 degrees | 283 | 0.36 |
| 1000 | 2560x1440, no margin | 419 | 0.52 |
| 10000 | 1280x720, no margin | 517 | 0.96 |
| 10000 | 1280x720, 256px overdraw | 1177 | 2.17 |
| 10000 | 1280x720, rotated 30 degrees | 850 | 1.44 |
| 10000 | 2560x1440, no margin | 1858 | 3.72 |
| 100000 | 1280x720, no margin | 617 | 1.76 |
| 100000 | 1280x720, 256px overdraw | 1444 | 3.91 |
| 100000 | 1280x720, rotated 30 degrees | 1205 | 2.20 |
| **100000** | **2560x1440, no margin** | **2395** | **5.60** |
| 500000 | 1280x720, no margin | 615 | 3.03 |
| 500000 | 1280x720, 256px overdraw | 1439 | 4.80 |
| 500000 | 1280x720, rotated 30 degrees | 1221 | 4.11 |
| 500000 | 2560x1440, no margin | 2386 | 7.11 |

The bolded row is #28's criterion: **2395 items culled from 100k in 5.6us, 178x under the 1ms
budget.** Time tracks the number of items returned far more than `n` -- 500k costs 1.3x what
100k does for the same answer size, and that residue is the deeper tree, not the scan.

### Tile cache

300 frames, 1280x720, 128px overdraw, 64 MiB budget.

| script | hit rate | tiles rendered | resident bytes |
|---|---:|---:|---:|
| pan + zoom | 98.0% | 209 | 54788096 |
| pan + zoom, 20 edits/frame in one selection | 92.9% | 729 | 46399488 |
| pan + zoom, 20 edits/frame scattered | 61.8% | 3944 | 43778048 |
| pan + zoom, 200 edits/frame scattered | 25.0% | 7757 | 43778048 |

The 209 tiles of the first row are the whole cost of 300 frames of navigation: about 35 tiles
are on screen at once, so all but the first six frames' worth are the leading edge scrolling in
and the two level boundaries the zoom crosses. Resident bytes stay under the 67108864-byte
budget in every run, which is the eviction policy doing its job rather than a coincidence of
this script.

### Things these numbers do not cover

- **No rendering.** A cache miss here allocates; in the browser it rasterises and uploads. The
  hit rate is the honest number; the frame time is #31's.
- **Uniform density and a uniform edit distribution.** A real selection is spatially clustered,
  which is the 92.9% row, but it is also usually *the same* selection frame after frame, which
  would do better than this script's fresh random cluster each frame.
- **One thread, one document.** No measurement of a second cache, or of the memory pressure of
  several documents open at once.

---

## The P3 gate: 100k objects at p99 under 16ms (#31)

```sh
cargo run --release --example gate_bench            # native

cargo build --release --target wasm32-unknown-unknown -p hane-wasm
mkdir -p web/public && cp target/wasm32-unknown-unknown/release/hane_wasm.wasm web/public/hane.wasm
cd web && npm ci && npm run build && cd ..
python3 scripts/gate-bench.py                       # Chrome and Firefox
```

**Verdict: the gate is not yet answerable, and everything that can be measured today passes
with room. At 100k the CPU side of a frame is 2.8 ms p99 in Chromium and 3.4 ms in Firefox,
leaving ~13 ms of the 16 ms budget for a renderer that does not exist yet. At 500k it is 14.3
and 15.3 ms, which spends the whole budget before a single pixel is filled.**

### Read this before the tables

**There is no GPU renderer.** P2 is unbuilt. Nothing in this repository can turn bins into
pixels on a canvas, so no number here is a frame time and none should be quoted as one. What is
measured is every part of a frame that exists:

| phase | what it is | in the real frame? |
|---|---|---|
| **cull** | advance the camera, `View::visible_bounds`, one `Quadtree::query`, `TileKey::visible`, one `TileCache::get` per tile | yes, on the CPU, every frame |
| **encode** | per missed tile: query the index for that tile's square, map to the tile's device pixels, `TileBinner::bin` | yes -- this is the CPU half of the GPU path (D-010) |
| **raster** | per missed tile: the same segments through `hane-raster` into real pixels | no. This is the *oracle* (D-002), correct rather than fast |

So the bolded **frame** column is `cull + encode`: **the CPU cost of a GPU frame with the GPU
removed.** It is a lower bound on the real frame. Missing from it: the texture upload of a
freshly rendered tile, the composite of the ~50 cached tiles that make up the screen, GL state
changes, and whatever the wasm/JS boundary costs under real `requestAnimationFrame` pacing with
a garbage collector running. Those are P2's to measure.

`raster` is reported because it is the only thing here that actually produces pixels, and
because it fills the cache, so the hit rates and the eviction the other two phases see are real.
Its *time* is not a renderer's -- 214 ms at 100k is D-002 working exactly as designed, and it is
the number P2 has to beat by three orders of magnitude.

### What was measured

- **Scene.** `n` ellipses, four cubics each, radii 2-20 units, uniform over a **fixed** 4096 x
  4096 document at every `n`. That is deliberately not what `spatial_bench` and `tile_bench` do
  -- they grow the document with `n` so density stays constant, which is right for measuring an
  index and would make this harness report that 1k and 500k cost the same. Here `n` is the
  number of objects actually competing for the screen: 46 visible at 1k, 4864 at 100k, 24075 at
  500k.
- **Script.** 300 frames, first 37 discarded. Zoom 1.0078 per frame, reversing every 90 -- the
  same gesture as `tile_bench`'s, but reversing so the run crosses a power-of-two tile level in
  both directions instead of magnifying 10x and then seeing nothing. Pan turns steadily so a
  fast drag stays inside the document. Purely a function of the frame counter, so a Chromium run
  and a Firefox run measure identical work, down to the tile: the `visible`, `tiles`,
  `miss/frame` and `hit rate` columns are byte-identical across all three rows of every table
  below, which is the check that they are.
- **Two pan speeds.** 4 px/frame is `tile_bench`'s script and reproduces its ~98% hit rate. 40
  px/frame is a flick -- 2400 px/s, an ordinary drag, and the speed the cache helps least with.
  A benchmark that ran only the slow one would report "the frame costs 30 microseconds", which
  is true of the gentlest gesture in the product and of nothing else.
- **Percentiles, not medians**, unlike the rest of this file. The gate is stated as a p99 and
  the p99 is the whole question. Note that p50 encode is *zero* at every size: the median frame
  has no cache miss and does no encode work at all.
- **Viewport** 1280x720, 128 px overdraw, 64 MiB tile budget -- the same as `tile_bench`.

Machine: AMD Ryzen 9 9950X3D, Chromium 150.0.7871.186 and Firefox 153.0, both headless on
Linux, both cross-origin isolated so `performance.now()` reports at 5 us and 20 us rather than
Firefox's default 1 ms clamp. The page publishes the resolution it measured, because a table of
quantised nonsense looks exactly like a real one.

All times in milliseconds. **frame** = `cull + encode`, taken as a per-frame sum before the
percentile -- p99 of a sum is not the sum of the p99s. **cold** is frame 0, where every tile on
screen misses: a document opening, or a jump to a zoom level never visited.

### Chromium 150, headless, Linux

| n | script | visible | miss/frame | hit rate | cull p99 | encode p99 | **frame p99** | raster p99 | cold |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | navigate | 46 | 0.43 | 98.8% | 0.010 | 0.025 | **0.030** | 2.0 | 0.48 |
| 1000 | flick | 33 | 1.77 | 95.1% | 0.010 | 0.035 | **0.035** | 4.0 | 0.14 |
| 10000 | navigate | 508 | 0.43 | 98.8% | 0.015 | 0.210 | **0.210** | 21.8 | 0.79 |
| 10000 | flick | 369 | 1.77 | 95.1% | 0.015 | 0.345 | **0.355** | 27.3 | 0.96 |
| 100000 | navigate | 4864 | 0.43 | 98.8% | 0.055 | 1.700 | **1.715** | 214.1 | 7.08 |
| 100000 | flick | 3591 | 1.77 | 95.1% | 0.045 | 2.760 | **2.785** | 329.3 | 8.68 |
| 500000 | navigate | 24075 | 0.43 | 98.8% | 0.265 | 7.675 | **7.775** | 882.3 | 37.63 |
| 500000 | flick | 18036 | 1.77 | 95.1% | 0.185 | 14.155 | **14.275** | 1516.6 | 45.61 |

### Firefox 153, headless, Linux

| n | script | visible | miss/frame | hit rate | cull p99 | encode p99 | **frame p99** | raster p99 | cold |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1000 | navigate | 46 | 0.43 | 98.8% | 0.020 | 0.040 | **0.040** | 2.6 | 0.16 |
| 1000 | flick | 33 | 1.77 | 95.1% | 0.020 | 0.040 | **0.040** | 4.4 | 0.14 |
| 10000 | navigate | 508 | 0.43 | 98.8% | 0.020 | 0.220 | **0.220** | 24.6 | 0.84 |
| 10000 | flick | 369 | 1.77 | 95.1% | 0.020 | 0.380 | **0.380** | 40.7 | 0.98 |
| 100000 | navigate | 4864 | 0.43 | 98.8% | 0.060 | 1.840 | **1.860** | 256.6 | 8.08 |
| 100000 | flick | 3591 | 1.77 | 95.1% | 0.060 | 3.380 | **3.420** | 428.2 | 9.38 |
| 500000 | navigate | 24075 | 0.43 | 98.8% | 0.260 | 9.400 | **9.580** | 1377.9 | 41.20 |
| 500000 | flick | 18036 | 1.77 | 95.1% | 0.200 | 15.140 | **15.280** | 2104.5 | 47.40 |

### Native, same harness, same script

`cargo run --release --example gate_bench`. The wasm tax, at 100k flick: Chromium 1.19x
native, Firefox 1.46x.

| n | script | cull p99 | encode p99 | **frame p99** | raster p99 | cold |
|---|---|---:|---:|---:|---:|---:|
| 1000 | navigate | 0.002 | 0.017 | **0.018** | 1.7 | 0.08 |
| 1000 | flick | 0.003 | 0.029 | **0.030** | 2.6 | 0.09 |
| 10000 | navigate | 0.008 | 0.158 | **0.160** | 15.0 | 0.71 |
| 10000 | flick | 0.006 | 0.417 | **0.421** | 25.0 | 0.79 |
| 100000 | navigate | 0.036 | 1.337 | **1.349** | 153.6 | 6.16 |
| 100000 | flick | 0.035 | 2.308 | **2.334** | 283.4 | 7.22 |
| 500000 | navigate | 0.188 | 6.947 | **7.040** | 829.2 | 29.87 |
| 500000 | flick | 0.138 | 11.640 | **11.747** | 1383.5 | 38.24 |

### What the numbers say

**The tile cache is the entire reason these numbers are small.** Encode runs only on tiles that
missed: 0.43 per frame while navigating, 1.77 while flicking, out of ~50 on screen. #18
measured a from-scratch bin at 2.85 ms for 10000 segments and 14.8 ms for 50000; the 100k
viewport here holds 4864 shapes, or 19456 segments, which extrapolates to ~5.6 ms **every
frame** without a cache against 2.8 ms in the worst frame with one. The cache converts a
constant per-frame cost into an occasional spike, and the gate rests on that trade.

**The spike is the zoom crossing a tile level.** The script crosses a power of two every ~90
frames, and the first crossing into a level asks for a whole screen of keys that have never been
rendered, so every tile misses at once. That frame costs about what the `cold` column costs --
7-9 ms at 100k, 38-47 ms at 500k -- and there are only one or two of them in 263 counted frames,
so they sit *above* the p99 rather than in it. **The worst frame in a run is the cold column,
not the p99 column.** At 100k that worst frame is still inside 16 ms with the GPU's share
unspent; at 500k it is a visible stall of three frames.

**The CPU oracle is not a fallback.** 214 ms p99 at 100k in Chromium, 1.5 s at 500k. That is
D-002 working as intended -- `hane-raster` is optimised for being obviously correct -- but it
settles the "could we just ship the CPU rasterizer" question: no, by three orders of magnitude,
and P2 is not optional.

**Firefox is consistently slower than Chromium on encode**, by 8% at 100k navigating and 23%
flicking, and it is the browser that decides the gate. Both are within 1.5x of native, so the
wasm tax is real but small; the flattener and the slab walk are not being penalised by the
sandbox.

**Editing is the untested risk, and the arithmetic is reassuring.** This script is pan and zoom
only, which is what #31 asks for. #30 measured the hit rate falling to 61.8% under 20 scattered
edits per frame -- about 21 misses per frame instead of 0.43. Frame 0 here encodes 54 misses in
7.1 ms at 100k, so 21 misses is roughly 2.8 ms of encode per frame, sustained rather than
occasional: tight, but inside budget. That is arithmetic on someone else's measurement, not a
measurement. P5 should make it one.

**500k is reported and does not hit 60fps.** Firefox spends 15.3 ms of the 16 ms budget on
CPU-only work in the flick script, before a pixel is filled, and its worst frame is 47 ms.
Nothing about a GPU renderer recovers that; the encode path itself would have to get cheaper --
by keeping the per-tile segment list across frames instead of re-querying and re-transforming
it, which is the obvious next lever and is not built.

### Things these numbers do not cover

- **The GPU, which is most of a real frame.** No upload of a freshly rendered tile, no composite
  of the ~50 cached tiles that make up the screen, no GL state, no shader. P2 owns all of it.
  Quote the **frame** column as a floor, never as a frame time.
- **No `requestAnimationFrame`.** Frames run back to back with a yield every eight, so there is
  no vsync, no compositor, and no chance for the browser to interleave a GC pause into a timed
  span. Real frame pacing will be worse.
- **Editing, rotation, and everything but a solid fill.** Pan and zoom only. Solid colours only:
  no strokes (P4 does not exist), no gradients, no clips, no text.
- **One machine, and a fast one.** A laptop at a third of this speed puts 100k flick at ~9 ms of
  CPU-only work, which changes the verdict from comfortable to tight. Re-run before trusting it
  on other hardware.
- **Headless, one tab, no other load.** No compositor, no other documents, no memory pressure.
- **Memory is not budgeted.** 500k shapes is ~128 MiB of control points in linear memory before
  the tile cache's 64 MiB, and nothing here measures what the browser does when a real document
  is that size.

---

## Which rasterizer the GPU should run (#19, D-003)

WebGL2 has no compute shaders, so D-003 left the shape of the rasterizer open: stencil-then-cover
or coverage computed in a fragment shader. This is the measurement that closed it, and it is not
a timing -- it is a per-pixel diff against the CPU oracle, because that is what the acceptance
criterion asks for and because a fast renderer that cannot match the oracle is not a candidate at
all.

### What was measured

`python3 scripts/gpu-diff.py` renders all 42 corpus fixtures through the real renderer in a
headless browser and POSTs the framebuffers back; `cargo test -p hane-gpu --test oracle_diff`
compares them against `hane-raster`'s committed goldens with `hane_raster::diff` -- the same
comparator the CPU harness uses, so one rule judges both sides (D-002).

Chromium runs on SwiftShader, Firefox on its software WebGL backend. Neither is a GPU. That is
fine and deliberate: both are conformant WebGL2 implementations and the thing under test is the
*arithmetic*, not the silicon. A driver difference would show up as a handful of edge pixels, and
the table below is how you would see it.

### The three candidates, before any code

| approach | worst case on this corpus |
|---|---|
| Signed-area accumulation, `min(abs(a), 1)` | integrates the *winding* over a pixel, not the coverage. Equal only where the winding is 0 or 1; at each of `pentagram`'s five crossing vertices a winding-2 sector meets a winding-0 one inside one pixel and the answer is out by up to a quarter of a pixel -- **~60 counts**. |
| Stencil-then-cover | exact about the winding, and has no anti-aliasing at all without MSAA. `glctx.rs` sets `antialias: false` on purpose, because a second differently-quantised AA puts the output permanently out of the oracle's reach. |
| Fragment-shader coverage running the oracle's own loop | sixteen sample lines per pixel, sorted crossings, analytic horizontal spans -- the same computation in `f32`. |

### The third one, measured

| | fixtures compared | bit-exact | worst max | worst mean |
|---|---:|---:|---:|---:|
| Chromium 150, `--use-gl=swiftshader` | 41 | 31 | 1 | 0.0060 |
| Firefox 153, software WebGL | 41 | 30 | 1 | 0.0114 |

One count out of 255 is the smallest difference a byte can hold. Every fixture the corpus exists
to catch a GPU on passes bit-exactly: `seam_shared_edge` (the conflation artifact, which needs one
draw per fill and not one accumulation buffer), `tile_boundary_rects`, `nested_triangles`
(winding 3), `pentagram` (winding 2), and both annuli -- identical geometry, opposite correct
answer.

`sliver_rows` is worth naming. #20 gave it max 24 on the reasoning that a GPU computing coverage
analytically would get the *right* answer where the oracle's sixteen sample lines get a quantised
one. The reasoning was sound; the premise turned out false, because this renderer samples the same
sixteen lines. It is bit-exact, and the tolerance came back down to the default.

### Things these numbers do not cover

- **Speed.** Nothing here is timed. The shader sorts crossings per pixel per sample line, which is
  more work per fragment than either alternative; the tile binning is what pays for it and neither
  has been measured against a frame budget. P2's own gate is correctness; the timing belongs with
  the first real frame loop.
- **A hardware driver.** Both browsers rasterized in software. A real GPU may reorder the
  floating-point contractions in the coverage loop (`fma`), which is exactly the kind of change
  that moves an edge pixel by one count -- inside the table's bound, but unverified.
- **`extreme_coords`.** Rendered and compared, it comes back at max 215, mean 38.5, and is
  excluded from the corpus for that reason. Its vertices are at 1e9, where an `f32` has a 64-pixel
  quantum (D-004). The fix is a view transform applied before the narrowing, and it belongs to
  whoever adds one.
- **Deep clip nesting and deep groups.** The corpus goes three clips deep and one group deep. The
  documented bounds are 8 and 4.

---

## The second backend: WebGPU against WebGL2 (#24, D-012)

```sh
cargo build --release --target wasm32-unknown-unknown -p hane-wasm
wasm-bindgen --target web --out-dir web/public --out-name hane \
    target/wasm32-unknown-unknown/release/hane_wasm.wasm
cd web && npm ci && npm run build && cd ..
python3 scripts/gpu-diff.py --browser chromium --backend webgpu --bench 3
cargo test -p hane-gpu --test oracle_diff
```

Two questions, in this order: does the second backend reproduce the oracle, and is it faster.

### Correctness first

Same corpus, same comparator, same tolerance table -- `oracle_diff.rs` cannot tell which backend
wrote the bytes it is reading, which is the only way "the second backend matches the first" means
anything.

| backend | browser, adapter | fixtures | bit-exact | worst max | worst mean |
|---|---|---:|---:|---:|---:|
| WebGL2 | Chromium 150, ANGLE on SwiftShader | 41 | 31 | 1 | 0.0060 |
| WebGL2 | Firefox 153, software WebGL | 41 | 29 | 1 | 0.0114 |
| **WebGPU** | Chromium 150, Dawn on SwiftShader | 41 | **33** | **1** | **0.0060** |
| **WebGPU** | Firefox 153, wgpu | 41 | **30** | **1** | **0.0114** |

Both WebGL2 rows were re-measured for this comparison rather than copied: Firefox comes back at
29 bit-exact where #19 recorded 30, one fixture having moved across a `1/255` boundary between
browser builds. Every other number is unchanged, and no fixture left the tolerance.

WebGPU is bit-exact on *two more* fixtures than WebGL2 in Chromium, which is
not luck: the GL path rounds a coverage composite to `floor(v + 0.5)`, divides by 255 and lets the
driver write it back through a unorm8 texture, while the compute path packs the byte itself and
stores it. One round trip removed is one place a difference cannot appear.

The eight Chromium fixtures that still differ by one count are the ones with an anti-aliased edge
whose coverage lands on a `1/255` boundary -- `figure_eight` and its even-odd twin worst, at a
mean of 0.006 over the whole image.

`extreme_coords` is excluded for the WebGPU backend for exactly the reason it is excluded for
WebGL2: `f32` has a 64-pixel quantum at 1e9 and no shading language changes that (D-004).

### Speed, and what the corpus can actually measure

One session, both backends, the same 42 fixtures, median of 3 passes:

| browser | WebGL2 | WebGPU |
|---|---:|---:|
| Chromium 150, SwiftShader | 2875 ms | **390 ms** |
| Firefox 153, software | **340 ms** | 4205 ms |

Those numbers disagree about which backend is faster, and both are right, because **neither is
measuring rasterization**. Timing four fixtures individually says why:

| fixture | Chromium WebGPU | Chromium WebGL2 | Firefox WebGPU | Firefox WebGL2 |
|---|---:|---:|---:|---:|
| `rect_pixel_aligned`, 64x64 | 2.4 ms | 64 ms | 100 ms | 11 ms |
| `tiny_canvas`, **1x1** | 2.4 ms | 65 ms | 99 ms | 4 ms |
| `blend_isolated_group` | 41 ms | 86 ms | 99 ms | 7 ms |

A **one-pixel** canvas costs Chromium's WebGL2 backend 65 ms and Firefox's WebGPU backend 100 ms.
Neither number can be pixels:

- **Chromium's WebGL2 cost is compilation.** `glrender.rs` builds its three GLSL programs per
  render, and translating them through ANGLE onto SwiftShader is most of a scene.
- **Firefox's WebGPU cost is a fixed round trip.** It is 100 ms for every scene, whatever the
  scene, and it does not move when the pipeline is cached -- the shape of a readback that waits
  for a device poll, not of work. This harness maps the framebuffer back to the CPU after every
  single render, which is the worst case for it and is not what a frame loop does.

WebGPU caches its pipeline across renders because nothing in it depends on the scene, and doing so
took the Chromium corpus pass from 7054 ms to 390 ms -- 95% of the original was compiling one
shader forty-two times. The GL backend still rebuilds its programs per render, so the two columns
above are not like for like, and the honest symmetric pair is the *uncached* one: 7054 ms WebGPU
against 2875 ms WebGL2, where Dawn's WGSL-to-SPIR-V is slower than ANGLE's GLSL-to-SPIR-V.

### Things these numbers do not cover

- **Steady-state throughput of either rasterizer.** This corpus is 42 independent scenes rendered
  once each and read back; it measures setup and readback, and both dominate. The comparison that
  matters -- one scene, many frames, nothing mapped -- belongs with the first real frame loop, and
  so does caching the GL programs the way WebGPU caches its pipeline.
- **A hardware adapter.** Chromium ran Dawn on Vulkan-SwiftShader and Firefox ran wgpu in
  software. Both are conformant implementations and the *arithmetic* is what the correctness table
  is about, but nothing here says what a discrete GPU does with a 256-invocation workgroup.
- **Large canvases.** Every fixture is at most 64x64. A tiled compute dispatch is exactly the
  shape that should pull ahead as the canvas grows, and that is untested.
- **Firefox on a machine without the pref.** `dom.webgpu.enabled` is set by `scripts/gpu-diff.py`
  in the throwaway profile. Firefox 153 on Linux does not enable WebGPU by default, which is why
  automatic selection has to fall back rather than assume.
