# Benchmarks

Numbers here are medians, produced by hand-rolled harnesses -- D-001 rules out `criterion`.
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
