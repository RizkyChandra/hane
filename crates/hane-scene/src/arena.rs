//! The scene graph store: an arena keyed by generational `u32` ids (D-004).
//!
//! The document lives in WASM linear memory and JS holds nothing but ids, which
//! it keeps across frames and, inevitably, across deletions. A plain index
//! would silently address whatever was inserted into the freed slot next --
//! "delete the layer, then rename it" would rename a stranger. So an id carries
//! the slot's generation as well as its index, and a removal bumps the
//! generation: the stale id then fails to match and [`Arena::get`] returns
//! `None`, which is a bug the caller can see rather than one the user reports
//! three weeks later.
//!
//! # Id layout
//!
//! One `u32`, so an id is a single JS number and a single wasm argument, and so
//! it can be handed straight to [`Quadtree`](crate::Quadtree) as an item id
//! without a side table: 20 low bits of index, 12 high bits of generation.
//!
//! ponytail: 1,048,576 live slots and 4096 reuses per slot. Both are far past
//! P3's 100k-object gate, but a stale id held across exactly 4096 reuses of one
//! slot aliases again and is accepted -- widen the id to `u64` (32/32, still
//! exact as a JS number) if that ever becomes reachable.

/// Bits of [`NodeId`] holding the slot index. The rest hold the generation.
const INDEX_BITS: u32 = 20;
/// Mask of the index field.
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;
/// Mask of the generation field, once shifted down.
const GENERATION_MASK: u32 = u32::MAX >> INDEX_BITS;

/// A handle to a node in an [`Arena`], valid only until that node is removed.
///
/// Opaque, `Copy`, and exactly one `u32` wide -- see the module docs for the
/// layout and for why it is not just an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    /// The id as a raw `u32`, for handing to JS or to the spatial index.
    #[inline]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// An id from a raw `u32`, as it comes back from JS.
    ///
    /// Every bit pattern is accepted: an id that was never issued is not
    /// distinguishable from one that has expired, and both are rejected the
    /// same way, by [`Arena::get`] returning `None`.
    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    #[inline]
    const fn new(index: u32, generation: u32) -> Self {
        Self((generation << INDEX_BITS) | index)
    }

    #[inline]
    const fn index(self) -> usize {
        (self.0 & INDEX_MASK) as usize
    }

    #[inline]
    const fn generation(self) -> u32 {
        self.0 >> INDEX_BITS
    }
}

/// One slot: its current generation, and its value while it is occupied.
struct Slot<T> {
    /// Incremented on every removal, so ids issued before it stop matching.
    generation: u32,
    value: Option<T>,
}

/// A store of `T` keyed by [`NodeId`], where removal invalidates only the ids
/// of the removed node.
///
/// Slots are recycled, so insertion is `O(1)` and the backing storage tracks
/// the high-water mark rather than growing forever.
pub struct Arena<T> {
    /// Indexed by [`NodeId::index`]. Never shrinks: a slot's generation has to
    /// outlive its value, or the ids it invalidated would come back to life.
    slots: Vec<Slot<T>>,
    /// Indices of the vacant slots, most recently freed first.
    free: Vec<u32>,
}

impl<T> Arena<T> {
    /// An empty arena.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }

    /// The number of live nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// True when no node is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Inserts `value` and returns its id.
    ///
    /// # Panics
    ///
    /// If the arena has more than `2^20` slots, which the id layout cannot
    /// address. Panicking beats truncating the index and handing back an id
    /// that points at someone else's node.
    pub fn insert(&mut self, value: T) -> NodeId {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            return NodeId::new(index, slot.generation);
        }
        let index = self.slots.len();
        assert!(
            index <= INDEX_MASK as usize,
            "scene arena is full at {} nodes",
            INDEX_MASK as usize + 1
        );
        self.slots.push(Slot {
            generation: 0,
            value: Some(value),
        });
        NodeId::new(index as u32, 0)
    }

    /// The node `id` refers to, or `None` if it has been removed or never
    /// existed.
    #[must_use]
    pub fn get(&self, id: NodeId) -> Option<&T> {
        let slot = self.slots.get(id.index())?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.value.as_ref()
    }

    /// Mutable access to the node `id` refers to.
    #[must_use]
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut T> {
        let slot = self.slots.get_mut(id.index())?;
        if slot.generation != id.generation() {
            return None;
        }
        slot.value.as_mut()
    }

    /// Removes the node `id` refers to and returns it, or `None` if `id` is
    /// already stale.
    ///
    /// Ids of other nodes keep working: nothing moves, only this slot changes.
    pub fn remove(&mut self, id: NodeId) -> Option<T> {
        let index = id.index();
        let slot = self.slots.get_mut(index)?;
        if slot.generation != id.generation() {
            return None;
        }
        let value = slot.value.take()?;
        // Wrap within the field rather than letting the counter carry into the
        // index bits, which would make every future id from this slot address a
        // different one.
        slot.generation = (slot.generation + 1) & GENERATION_MASK;
        self.free.push(index as u32);
        Some(value)
    }

    /// Every live node with its id, in slot order.
    ///
    /// Slot order is insertion order only until the first removal; nothing
    /// downstream may depend on it, and the quadtree is what gives spatial
    /// order.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &T)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            Some((
                NodeId::new(index as u32, slot.generation),
                slot.value.as_ref()?,
            ))
        })
    }
}

impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::Rng;

    #[test]
    fn insert_get_remove_iterate() {
        let mut arena = Arena::new();
        let a = arena.insert("a");
        let b = arena.insert("b");
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.get(a), Some(&"a"));
        *arena.get_mut(b).unwrap() = "B";

        let mut seen: Vec<_> = arena.iter().map(|(id, v)| (id, *v)).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![(a, "a"), (b, "B")]);

        assert_eq!(arena.remove(a), Some("a"));
        assert_eq!(arena.remove(a), None);
        assert!(arena.get(a).is_none());
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.iter().count(), 1);
    }

    #[test]
    fn a_stale_id_never_reaches_the_node_that_replaced_it() {
        let mut arena = Arena::new();
        let stale = arena.insert(1);
        assert_eq!(arena.remove(stale), Some(1));
        // Same slot, so an index-only id would hit `fresh` here.
        let fresh = arena.insert(2);
        assert_eq!(stale.to_bits() & INDEX_MASK, fresh.to_bits() & INDEX_MASK);
        assert_eq!(arena.get(stale), None);
        assert_eq!(arena.get_mut(stale), None);
        assert_eq!(arena.remove(stale), None);
        assert_eq!(arena.get(fresh), Some(&2));
    }

    #[test]
    fn removal_leaves_other_ids_alone() {
        let mut arena = Arena::new();
        let ids: Vec<_> = (0..64).map(|i| arena.insert(i)).collect();
        for (i, id) in ids.iter().enumerate().step_by(2) {
            assert_eq!(arena.remove(*id), Some(i32::try_from(i).unwrap()));
        }
        for (i, id) in ids.iter().enumerate() {
            let expected = i32::try_from(i).unwrap();
            assert_eq!(arena.get(*id), (i % 2 == 1).then_some(&expected));
        }
    }

    #[test]
    fn the_generation_counter_wraps_without_overflowing_into_the_index() {
        let mut arena = Arena::new();
        let mut id = arena.insert(0);
        // One full cycle of the generation field plus a few, on one slot.
        for i in 1..=GENERATION_MASK + 4 {
            assert!(arena.remove(id).is_some());
            id = arena.insert(i);
            assert_eq!(id.index(), 0, "generation carried into the index bits");
            assert_eq!(arena.get(id), Some(&i));
        }
        assert_eq!(arena.len(), 1);
    }

    #[test]
    fn ids_survive_a_round_trip_through_raw_bits() {
        let mut arena = Arena::new();
        arena.insert(0);
        let id = arena.insert(7);
        assert_eq!(arena.get(NodeId::from_bits(id.to_bits())), Some(&7));
        // Whatever else JS sends is rejected rather than believed: a slot that
        // does not exist, and a live slot with the wrong generation.
        assert_eq!(arena.get(NodeId::from_bits(u32::MAX)), None);
        assert_eq!(
            arena.get(NodeId::from_bits(id.to_bits() + (1 << INDEX_BITS))),
            None
        );
    }

    /// Random insert/remove/get against a shadow list of what should be live.
    ///
    /// The point is the dead half: every id ever handed out is re-checked every
    /// iteration, so a recycled slot that accepts an expired id fails here even
    /// if the accepting sequence is a hundred operations long.
    #[test]
    fn no_expired_id_is_ever_accepted() {
        let mut rng = Rng::new(0x5ce4e);
        let mut arena = Arena::new();
        let mut live: Vec<(NodeId, u64)> = Vec::new();
        let mut dead: Vec<NodeId> = Vec::new();

        for step in 0..4_000u64 {
            // Capped so the arena hovers around 64 nodes: a small slot pool is
            // what makes reuse, and therefore aliasing, likely.
            if live.is_empty() || (live.len() < 64 && rng.below(2) > 0) {
                live.push((arena.insert(step), step));
            } else {
                let i = rng.below(live.len() as u64) as usize;
                let (id, value) = live.swap_remove(i);
                assert_eq!(arena.remove(id), Some(value));
                dead.push(id);
            }
            assert_eq!(arena.len(), live.len());
            for (id, value) in &live {
                assert_eq!(arena.get(*id), Some(value));
            }
            for id in &dead {
                assert_eq!(arena.get(*id), None, "expired id accepted at step {step}");
            }
        }
    }
}
