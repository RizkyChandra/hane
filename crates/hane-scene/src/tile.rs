//! A cache of rendered tiles, and the invalidation that keeps it honest (#30).
//!
//! This is the mechanism D-006 rests on: at 100k objects the rasterizer's inner
//! loop is not what decides the frame, never touching the 99% of the document
//! that did not change is. Culling handles "off screen"; this handles
//! "unchanged".
//!
//! # The tile grid is anchored in the document, not on the screen
//!
//! A tile's key is `(level, x, y)`, and it covers the document square
//! `[x * s, (x+1) * s)` by `[y * s, (y+1) * s)` where `s = TILE_PX / 2^level`.
//! Nothing in that depends on the pan or on the rotation, and it depends on the
//! zoom only through `level`, which is `round(log2(zoom))`. Three of the four
//! acceptance criteria fall straight out of that choice:
//!
//! - **Pan reuses tiles.** Panning does not appear in the key at all, so the
//!   tiles a pan brings into view are the ones it left behind a moment ago.
//! - **Zoom invalidates without discarding everything.** A zoom only changes
//!   the answer when it crosses a power of two. It then asks for a *different*
//!   set of keys rather than dirtying the old ones, so the level being left is
//!   still resident and zooming back is a hit.
//! - **Rotation costs nothing.** Tiles are axis-aligned in the document, so a
//!   rotated view draws the same tiles as turned quads. The alternative --
//!   screen-aligned tiles -- would have to throw the cache away on every frame
//!   of a rotate gesture, and would put the rotation in the key, which is
//!   continuous and therefore never hits twice.
//!
//! # What is in a tile is not this module's business
//!
//! [`TileCache`] stores opaque bytes. Per D-010 that is deliberate: tile
//! *management* -- which tile is live, which is stale, which gets evicted -- is
//! the part that contains the bugs, and keeping it free of any pixel format or
//! GL handle is what lets `cargo test` cover it without a browser. The caller
//! puts in whatever it rendered, which in `hane-wasm` is an uploaded texture's
//! backing bytes and in a test is whatever the oracle painted.

use std::collections::HashMap;

use hane_geom::Rect;

use crate::View;

/// A tile's side, in pixels at its own level's scale.
///
/// 256 is the usual compromise and there is no measurement here that argues
/// for anything else: smaller tiles make invalidation finer but multiply the
/// per-tile fixed costs (a draw call, a texture bind, a hash lookup), larger
/// ones mean a one-pixel edit re-renders more of the screen.
const TILE_PX: f64 = 256.0;

/// Ceiling on how many tiles [`TileKey::visible`] will enumerate.
///
/// A tile is 181 to 362 screen pixels on a side -- the level is rounded, so the
/// mismatch is at most sqrt(2) either way -- which puts a 4K viewport at a few
/// hundred tiles even with the rotation bound doubling it. Anything past 16384
/// is a nonsense viewport rectangle, and the point of the cap is that such a
/// rectangle yields no tiles instead of a loop that runs for an hour.
const MAX_VISIBLE: f64 = 16384.0;

/// Which square of the document a cached tile holds, and at what scale.
///
/// Ordered so a caller can sort a visible set into a stable draw order without
/// keeping a second key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    /// The zoom level: the tile is rendered at `2^level` pixels per document
    /// unit, so level 0 is 1:1 and level 3 is an eightfold magnification.
    pub level: i16,
    /// The tile's column, counting from the document origin.
    pub x: i32,
    /// The tile's row, counting from the document origin.
    pub y: i32,
}

impl TileKey {
    /// The side of a level's tile in document units.
    ///
    /// A power of two times 256, so it is exact in binary -- which is what
    /// makes `x * size` and `(x + 1) * size` land on the same value from either
    /// side and lets the grid tile the plane with no seam and no overlap.
    fn doc_size(level: i16) -> f64 {
        TILE_PX * 2f64.powi(-i32::from(level))
    }

    /// The document-space square this tile covers.
    #[must_use]
    pub fn doc_rect(self) -> Rect {
        let size = Self::doc_size(self.level);
        let (x, y) = (f64::from(self.x) * size, f64::from(self.y) * size);
        Rect::new(x, y, x + size, y + size)
    }

    /// Appends every tile of the current level overlapping the viewport.
    ///
    /// `screen` and `margin` mean what they do in
    /// [`View::visible_bounds`] -- the same overdraw band, so the tiles a
    /// scroll is about to need are already resident.
    ///
    /// `out` is not cleared, matching
    /// [`Quadtree::query`](crate::Quadtree::query), so a per-frame loop can
    /// reuse one allocation.
    pub fn visible(view: &View, screen: Rect, margin: f64, out: &mut Vec<Self>) {
        // Rounding rather than flooring the level: flooring would always
        // render below native resolution and every tile on screen would be
        // magnified and soft, where rounding is off by at most sqrt(2) and
        // half the time in the sharp direction.
        let level = view.zoom().log2().round();
        let size = TILE_PX * 2f64.powi(-(level as i32));
        let doc = view.visible_bounds(screen, margin);
        let (x0, y0) = ((doc.x0 / size).floor(), (doc.y0 / size).floor());
        // Inclusive of the tile containing the far edge. When that edge lands
        // exactly on a boundary this adds one row of tiles that touch the
        // viewport without overlapping it; one row of overdraw is a much
        // cheaper mistake than one row of holes.
        let (x1, y1) = ((doc.x1 / size).floor(), (doc.y1 / size).floor());
        let (cols, rows) = (x1 - x0 + 1.0, y1 - y0 + 1.0);
        // NaN and infinite bounds fail this too, since every comparison
        // against NaN is false. An empty viewport gives cols or rows <= 0.
        if !(cols >= 1.0 && rows >= 1.0 && cols * rows <= MAX_VISIBLE) {
            return;
        }
        // Saturating, which `as` is for float-to-int. A document coordinate
        // past 2^31 tiles is unreachable from any real viewport, and clamping
        // to the edge tile beats wrapping to the other side of the document.
        let (x0, y0, x1, y1) = (x0 as i32, y0 as i32, x1 as i32, y1 as i32);
        let level = level as i16;
        out.reserve((cols * rows) as usize);
        for y in y0..=y1 {
            for x in x0..=x1 {
                out.push(Self { level, x, y });
            }
        }
    }
}

/// One resident tile: its bytes, and when it was last wanted.
struct Tile {
    pixels: Vec<u8>,
    /// [`TileCache::clock`] at the last [`TileCache::get`]. The LRU order.
    stamp: u64,
}

/// A bounded store of rendered tiles, keyed by [`TileKey`].
///
/// The invariant that matters, and the one the tests are built around: **a tile
/// that is present is up to date.** Invalidation removes rather than flags, so
/// there is no state in which a stale tile is resident and something has to
/// remember not to draw it. A caller that asks for a tile either gets bytes
/// that match a fresh render or gets `None`.
pub struct TileCache {
    tiles: HashMap<TileKey, Tile>,
    /// Ceiling on the sum of every resident tile's byte length.
    budget: usize,
    /// The current sum, maintained rather than recomputed.
    bytes: usize,
    /// Monotonic tick, incremented on every access. Not a wall clock: an LRU
    /// only needs an order, and a counter cannot go backwards.
    clock: u64,
    hits: u64,
    misses: u64,
}

impl TileCache {
    /// A cache holding at most `budget` bytes of tile pixels.
    ///
    /// The budget is bytes rather than a tile count because that is the number
    /// the browser actually runs out of, and because a caller may hold tiles of
    /// different sizes.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            tiles: HashMap::new(),
            budget,
            bytes: 0,
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// The tile's pixels, or `None` if it must be rendered.
    ///
    /// Counts towards the hit rate and marks the tile as most recently used,
    /// which is why it takes `&mut self`.
    pub fn get(&mut self, key: TileKey) -> Option<&[u8]> {
        self.clock += 1;
        let now = self.clock;
        match self.tiles.get_mut(&key) {
            Some(tile) => {
                tile.stamp = now;
                self.hits += 1;
                Some(&tile.pixels)
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Stores a freshly rendered tile, evicting as needed to stay in budget.
    ///
    /// A tile larger than the whole budget is dropped rather than stored: it
    /// would otherwise evict every other tile and then still not fit, turning
    /// a too-small budget into an empty cache instead of a small one.
    pub fn insert(&mut self, key: TileKey, pixels: Vec<u8>) {
        let len = pixels.len();
        if len > self.budget {
            return;
        }
        self.clock += 1;
        let tile = Tile {
            pixels,
            stamp: self.clock,
        };
        if let Some(old) = self.tiles.insert(key, tile) {
            self.bytes -= old.pixels.len();
        }
        self.bytes += len;
        while self.bytes > self.budget && self.evict_one() {}
    }

    /// Discards every tile overlapping the box an object occupied **before** an
    /// edit or the box it occupies **after** it.
    ///
    /// Both boxes, in one call, because that is the bug this signature exists
    /// to make unwriteable. Dirtying only the new box leaves the tile at the
    /// old position holding a render that still contains the object -- a ghost,
    /// which reads as a rendering bug and gets debugged as one for a week
    /// before anybody suspects the cache. Requiring the caller to pass both
    /// means forgetting the old box is a missing argument rather than a missing
    /// line.
    ///
    /// [`Rect::EMPTY`] overlaps nothing, so it is the right `old` for an
    /// insertion and the right `new` for a deletion. An edit that does not move
    /// anything -- a colour change -- passes the same box twice.
    pub fn invalidate(&mut self, old: Rect, new: Rect) {
        let mut freed = 0;
        // ponytail: a scan of every resident tile per call. The budget holds a
        // few hundred tiles, so this is a few hundred `overlaps` -- cheaper
        // than the hash lookups a per-level tile-range walk would do for a
        // typical one-tile edit, and it cannot be made to loop over a range
        // the size of the document by a select-all drag. Index the resident
        // set by level if a profile ever shows invalidation on top.
        self.tiles.retain(|key, tile| {
            let rect = key.doc_rect();
            if rect.overlaps(old) || rect.overlaps(new) {
                freed += tile.pixels.len();
                false
            } else {
                true
            }
        });
        self.bytes -= freed;
    }

    /// Drops every tile. For a document swap, where nothing is worth keeping.
    pub fn clear(&mut self) {
        self.tiles.clear();
        self.bytes = 0;
    }

    /// Tiles served from the cache since it was created.
    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Tiles that had to be rendered since the cache was created.
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// The fraction of [`TileCache::get`] calls that were served, or zero
    /// before the first call.
    ///
    /// This is the number #31's benchmark page reports as its own counter: a
    /// frame time that looks good with a hit rate near zero is measuring an
    /// easy scene, not a working cache.
    #[must_use]
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    /// Bytes of tile pixels currently resident, never above the budget.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Drops the least recently used tile. False when there was none.
    ///
    /// ponytail: a linear scan for the oldest stamp, run only when an insert
    /// overflows the budget. At the few hundred tiles a sane budget holds that
    /// is a few hundred `u64` comparisons against a re-render costing
    /// milliseconds. An intrusive LRU list is the upgrade if the budget ever
    /// holds tens of thousands.
    fn evict_one(&mut self) -> bool {
        let Some((&key, _)) = self.tiles.iter().min_by_key(|(_, tile)| tile.stamp) else {
            return false;
        };
        if let Some(tile) = self.tiles.remove(&key) {
            self.bytes -= tile.pixels.len();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use hane_geom::fuzz::Rng;
    use hane_geom::{Point, Rect, Vec2};

    use super::{TILE_PX, TileCache, TileKey};
    use crate::View;

    const SCREEN: Rect = Rect::new(0.0, 0.0, 640.0, 480.0);

    /// A scene of axis-aligned boxes with a colour each.
    ///
    /// The oracle below paints these, standing in for `hane-raster` or for a
    /// GPU render. What is under test is cache coherence, not rasterisation,
    /// and a painter that runs in microseconds buys hundreds of edit steps
    /// where the real one would buy a handful.
    struct Scene {
        objects: Vec<(Rect, u8)>,
    }

    impl Scene {
        fn new(rng: &mut Rng, n: usize) -> Self {
            Self {
                objects: (0..n)
                    .map(|_| {
                        let x = rng.below(2000) as f64 - 1000.0;
                        let y = rng.below(2000) as f64 - 1000.0;
                        let (w, h) = (rng.below(200) as f64 + 4.0, rng.below(200) as f64 + 4.0);
                        (Rect::new(x, y, x + w, y + h), rng.below(255) as u8 + 1)
                    })
                    .collect(),
            }
        }

        /// Paints one tile from scratch: 64x64 bytes, one per pixel, painter's
        /// algorithm in object order. Every byte is a function of the tile's
        /// document square and the objects overlapping it and of nothing else,
        /// which is exactly the property a cache entry has to preserve.
        fn render(&self, key: TileKey) -> Vec<u8> {
            const SIDE: usize = 64;
            let rect = key.doc_rect();
            let scale = SIDE as f64 / rect.width();
            let mut pixels = vec![0u8; SIDE * SIDE];
            for &(bbox, colour) in &self.objects {
                if !bbox.overlaps(rect) {
                    continue;
                }
                let clip = bbox.intersect(rect);
                let px = |v: f64, lo: f64| (((v - lo) * scale).round() as usize).min(SIDE);
                for row in px(clip.y0, rect.y0)..px(clip.y1, rect.y0) {
                    for col in px(clip.x0, rect.x0)..px(clip.x1, rect.x0) {
                        pixels[row * SIDE + col] = colour;
                    }
                }
            }
            pixels
        }
    }

    fn visible(view: &View, margin: f64) -> Vec<TileKey> {
        let mut keys = Vec::new();
        TileKey::visible(view, SCREEN, margin, &mut keys);
        keys
    }

    /// Draws a frame: every visible tile is served or rendered and stored.
    /// Returns the keys that had to be rendered.
    fn draw(cache: &mut TileCache, scene: &Scene, view: &View) -> Vec<TileKey> {
        let mut rendered = Vec::new();
        for key in visible(view, 64.0) {
            if cache.get(key).is_none() {
                cache.insert(key, scene.render(key));
                rendered.push(key);
            }
        }
        rendered
    }

    #[test]
    fn the_grid_tiles_the_plane_exactly() {
        for level in [-4i16, -1, 0, 1, 7] {
            let a = TileKey { level, x: 3, y: -2 }.doc_rect();
            let b = TileKey { level, x: 4, y: -2 }.doc_rect();
            let c = TileKey { level, x: 3, y: -1 }.doc_rect();
            // Bit-exact, not close: the sizes are powers of two, so a shared
            // edge that is only nearly shared would mean a real bug rather
            // than rounding -- and a gap of one ulp is still a seam.
            assert_eq!(a.x1, b.x0, "level {level}");
            assert_eq!(a.y1, c.y0, "level {level}");
            assert!(!a.overlaps(b) && !a.overlaps(c));
            assert_eq!(a.width(), TileKey::doc_size(level));
        }
    }

    #[test]
    fn a_pan_reuses_the_tiles_it_scrolls_over() {
        let mut rng = Rng::new(4);
        let scene = Scene::new(&mut rng, 200);
        let mut cache = TileCache::new(64 << 20);
        let mut view = View::new();
        // The property, stated exactly: over a whole drag no tile is ever
        // rendered twice. A tile scrolling in at the leading edge is a genuine
        // miss; a tile scrolling back over ground already covered is not, and
        // that is what a screen-anchored grid would get wrong.
        let mut seen = std::collections::HashSet::new();
        let mut drag = |cache: &mut TileCache, view: &View| {
            for key in draw(cache, &scene, view) {
                assert!(seen.insert(key), "{key:?} was rendered twice");
            }
        };
        drag(&mut cache, &view);
        assert!(cache.misses() > 0);
        // 120 frames of a slow drag, out and back: 360 screen pixels of travel
        // over a 640x480 viewport, so the return leg re-crosses everything.
        for frame in 0..120 {
            let d = if frame < 60 { -3.0 } else { 3.0 };
            view.pan_by(Vec2::new(d, d * 0.7));
            drag(&mut cache, &view);
        }
        assert!(cache.hit_rate() > 0.95, "{}", cache.hit_rate());
    }

    #[test]
    fn zoom_changes_level_without_discarding_the_level_it_leaves() {
        let mut rng = Rng::new(5);
        let scene = Scene::new(&mut rng, 200);
        let mut cache = TileCache::new(64 << 20);
        let mut view = View::new();
        let centre = Point::new(320.0, 240.0);
        draw(&mut cache, &scene, &view);
        let at_level_0: Vec<TileKey> = visible(&view, 64.0);
        assert_eq!(at_level_0[0].level, 0);

        // Past sqrt(2), where the rounded level ticks over to 1.
        view.zoom_about(centre, 1.5);
        draw(&mut cache, &scene, &view);
        let at_level_1 = visible(&view, 64.0);
        assert_eq!(at_level_1[0].level, 1);
        // The old level is still resident -- a new level asks for new keys, it
        // does not dirty the old ones.
        for key in &at_level_0 {
            assert!(cache.get(*key).is_some(), "{key:?} was discarded by a zoom");
        }
        // So zooming back is free.
        let before = cache.misses();
        view.zoom_about(centre, 1.0 / 1.5);
        draw(&mut cache, &scene, &view);
        assert_eq!(cache.misses(), before, "zooming back re-rendered");

        // And a zoom that stays inside one level changes nothing at all.
        let before = cache.misses();
        for _ in 0..8 {
            view.zoom_about(centre, 1.02);
        }
        draw(&mut cache, &scene, &view);
        assert_eq!(cache.misses(), before, "a within-level zoom re-rendered");
    }

    #[test]
    fn an_edit_dirties_the_tiles_it_touches_before_and_after_and_no_others() {
        let mut rng = Rng::new(6);
        let scene = Scene::new(&mut rng, 200);
        let mut cache = TileCache::new(64 << 20);
        let view = View::new();
        draw(&mut cache, &scene, &view);
        let keys = visible(&view, 64.0);

        // A move far enough that the two boxes share no tile, which is what
        // makes "before" and "after" separately observable.
        let old = Rect::new(-40.0, -40.0, 40.0, 40.0);
        let new = Rect::new(700.0, 500.0, 780.0, 580.0);
        cache.invalidate(old, new);
        for &key in &keys {
            let rect = key.doc_rect();
            let touched = rect.overlaps(old) || rect.overlaps(new);
            assert_eq!(
                cache.get(key).is_none(),
                touched,
                "{key:?} {rect:?} touched={touched}"
            );
        }
        // Both boxes really did hit something, or the assertion above passes
        // by describing an edit that changed nothing.
        assert!(keys.iter().any(|k| k.doc_rect().overlaps(old)));
        assert!(keys.iter().any(|k| k.doc_rect().overlaps(new)));
    }

    /// The oracle for #30's last criterion, and the one that catches the ghost.
    ///
    /// `ghost` drops the "before" box from every invalidation, which is the
    /// classic bug. The test asserts the honest run is coherent *and* that the
    /// ghosting run is not -- because a coherence check that cannot fail is
    /// not a check, and this one would pass trivially if `render` ignored the
    /// scene or if no tile were ever reused.
    fn coherent(ghost: bool) -> bool {
        let mut rng = Rng::new(9);
        let mut scene = Scene::new(&mut rng, 120);
        let mut cache = TileCache::new(4 << 20);
        let mut view = View::new();
        let centre = Point::new(320.0, 240.0);
        for step in 0..120 {
            // Move one object, at a scale that lands it in different tiles.
            let k = rng.below(scene.objects.len() as u64) as usize;
            let old = scene.objects[k].0;
            let new = old.translate(Vec2::new(
                rng.below(600) as f64 - 300.0,
                rng.below(600) as f64 - 300.0,
            ));
            scene.objects[k].0 = new;
            cache.invalidate(if ghost { Rect::EMPTY } else { old }, new);

            // Interleave the view script, so the check covers tiles that were
            // cached at one view and served at another.
            match step % 4 {
                0 => view.pan_by(Vec2::new(11.0, -7.0)),
                1 => view.zoom_about(centre, 1.06),
                2 => view.rotate_about(centre, 0.05),
                _ => view.pan_by(Vec2::new(-5.0, 13.0)),
            }
            draw(&mut cache, &scene, &view);

            for key in visible(&view, 64.0) {
                let fresh = scene.render(key);
                if cache.get(key).is_some_and(|cached| cached != fresh) {
                    return false;
                }
            }
        }
        // A coherent run that never served a tile proves nothing.
        assert!(cache.hit_rate() > 0.5, "vacuous: {}", cache.hit_rate());
        true
    }

    #[test]
    fn a_cached_tile_always_equals_a_fresh_render() {
        assert!(coherent(false), "a stale tile was served");
        assert!(
            !coherent(true),
            "dropping the old box left no ghost to find"
        );
    }

    #[test]
    fn eviction_holds_the_budget_and_keeps_the_recently_used() {
        // Room for four tiles exactly.
        let mut cache = TileCache::new(4000);
        let key = |x| TileKey { level: 0, x, y: 0 };
        for x in 0..4 {
            cache.insert(key(x), vec![x as u8; 1000]);
        }
        assert_eq!(cache.bytes(), 4000);
        // Touch 0, 1 and 3, leaving 2 as the least recently used.
        for x in [0, 1, 3] {
            assert!(cache.get(key(x)).is_some());
        }
        cache.insert(key(9), vec![9; 1000]);
        assert_eq!(cache.bytes(), 4000);
        assert!(cache.get(key(2)).is_none(), "evicted the wrong tile");
        for x in [0, 1, 3, 9] {
            assert!(cache.get(key(x)).is_some(), "tile {x} should have survived");
        }

        // A run of inserts must never exceed the budget, whatever the sizes.
        let mut rng = Rng::new(2);
        for x in 0..500 {
            cache.insert(key(x), vec![0; rng.below(1500) as usize + 1]);
            assert!(cache.bytes() <= 4000, "budget broken at {x}");
        }
        // A tile that cannot fit at all is refused, not stored and not allowed
        // to empty the cache on its way out.
        let before = cache.bytes();
        cache.insert(key(1234), vec![0; 5000]);
        assert_eq!(cache.bytes(), before);
        assert!(cache.get(key(1234)).is_none());
    }

    #[test]
    fn invalidation_and_clearing_keep_the_byte_count_exact() {
        let mut cache = TileCache::new(1 << 20);
        for x in 0..8 {
            cache.insert(TileKey { level: 0, x, y: 0 }, vec![0; 100]);
        }
        assert_eq!(cache.bytes(), 800);
        // Tiles 0..8 cover document x in 0..2048; this box hits two of them.
        cache.invalidate(Rect::new(300.0, 10.0, 600.0, 20.0), Rect::EMPTY);
        assert_eq!(cache.bytes(), 600);
        // Rect::EMPTY on both sides is a no-op, not a flush.
        cache.invalidate(Rect::EMPTY, Rect::EMPTY);
        assert_eq!(cache.bytes(), 600);
        cache.clear();
        assert_eq!(cache.bytes(), 0);
    }

    #[test]
    fn a_nonsense_viewport_yields_no_tiles_rather_than_a_hang() {
        let view = View::new();
        for screen in [
            Rect::EMPTY,
            Rect::ZERO,
            Rect::new(f64::NAN, f64::NAN, f64::NAN, f64::NAN),
            // Wider than the cap allows at this level, by a wide margin.
            Rect::new(0.0, 0.0, 1e9, 1e9),
        ] {
            let mut keys = Vec::new();
            TileKey::visible(&view, screen, 0.0, &mut keys);
            assert!(keys.is_empty(), "{screen:?} produced {} tiles", keys.len());
        }
        // A real viewport at both ends of the zoom range stays sane.
        for factor in [View::MIN_ZOOM, View::MAX_ZOOM] {
            let mut v = View::new();
            v.zoom_about(Point::ORIGIN, factor);
            let keys = visible(&v, 256.0);
            assert!(!keys.is_empty() && keys.len() < 64, "{}", keys.len());
            assert_eq!(keys[0].level as f64, factor.log2().round());
            // Every visible tile must actually meet the viewport, or the
            // margin band is being computed at the wrong scale.
            let doc = v.visible_bounds(SCREEN, 256.0);
            assert!(keys.iter().all(|k| k.doc_rect().overlaps(doc)));
        }
    }

    #[test]
    fn every_tile_the_viewport_touches_is_enumerated() {
        // The gap-free half of `visible`: sample the document points the
        // viewport covers and check each one's tile is in the set.
        let mut rng = Rng::new(12);
        let mut view = View::new();
        for step in 0..40 {
            view.pan_by(Vec2::new(rng.below(200) as f64 - 100.0, 17.0));
            view.zoom_about(Point::new(320.0, 240.0), 1.09);
            if step % 3 == 0 {
                view.rotate_about(Point::new(320.0, 240.0), 0.4);
            }
            let keys = visible(&view, 0.0);
            let size = TileKey::doc_size(keys[0].level);
            for _ in 0..50 {
                let s = Point::new(rng.below(640) as f64, rng.below(480) as f64);
                let doc = view.to_document(s);
                let want = TileKey {
                    level: keys[0].level,
                    x: (doc.x / size).floor() as i32,
                    y: (doc.y / size).floor() as i32,
                };
                assert!(keys.contains(&want), "step {step}: {want:?} missing");
            }
        }
    }

    #[test]
    fn the_tile_side_is_the_documented_number_of_screen_pixels() {
        // The claim MAX_VISIBLE's comment rests on: rounding the level keeps a
        // tile between 181 and 362 screen pixels at every zoom in range.
        let mut rng = Rng::new(13);
        for _ in 0..2000 {
            let zoom = View::MIN_ZOOM * (View::MAX_ZOOM / View::MIN_ZOOM).powf(rng.unit());
            let mut view = View::new();
            view.zoom_about(Point::ORIGIN, zoom);
            let level = view.zoom().log2().round() as i16;
            let px = TileKey::doc_size(level) * view.zoom();
            assert!(
                px > TILE_PX / 1.4143 && px < TILE_PX * 1.4143,
                "zoom {zoom}: {px} px"
            );
        }
    }
}
