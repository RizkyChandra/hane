//! What is selected, and the four things a pointer can do to it.
//!
//! A selection is a set of ids and nothing else. It holds no coordinates, no
//! bounding box and no copy of anything in the document, which is what lets it
//! sit outside the undo log: an edit that changes a shape's geometry cannot
//! invalidate a set of ids, so undo and redo of unrelated edits leave the
//! selection alone for free rather than by being replayed.
//!
//! Deleting a shape *does* invalidate its id -- the arena bumps the slot's
//! generation -- and a stale id is inert rather than dangerous, so it is swept
//! by [`Selection::retain_live`] at the point the caller cares, not eagerly.

use crate::document::Document;
use hane_geom::Rect;
use hane_scene::NodeId;
use std::collections::BTreeSet;

/// What a click or a marquee does to the existing selection.
///
/// The three modifier states of every editor: plain click replaces, shift adds,
/// ctrl/cmd toggles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectMode {
    /// Discard what was selected and select these.
    Replace,
    /// Add these to what was selected.
    Add,
    /// Deselect the ones already selected and select the rest.
    Toggle,
}

/// The set of selected shapes.
///
/// Ordered rather than hashed: the order is then a stable function of the ids
/// alone, so a marquee over 100k shapes reports the same order twice running
/// and a test can compare against a literal. Membership and insertion are
/// `O(log n)`, which a 100k-shape marquee needs and a linear `Vec` scan would
/// turn into `O(n^2)`.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    ids: BTreeSet<NodeId>,
}

impl Selection {
    /// An empty selection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ids: BTreeSet::new(),
        }
    }

    /// The number of selected shapes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether `id` is selected.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.ids.contains(&id)
    }

    /// The selected ids, in a stable order.
    pub fn iter(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.ids.iter().copied()
    }

    /// Deselects everything.
    pub fn clear(&mut self) {
        self.ids.clear();
    }

    /// Selects exactly `id`.
    pub fn select(&mut self, id: NodeId) {
        self.ids.clear();
        self.ids.insert(id);
    }

    /// Adds `id`, keeping what was already selected.
    pub fn add(&mut self, id: NodeId) {
        self.ids.insert(id);
    }

    /// Selects `id` if it was not selected, deselects it if it was.
    pub fn toggle(&mut self, id: NodeId) {
        if !self.ids.remove(&id) {
            self.ids.insert(id);
        }
    }

    /// Applies `mode` to `ids`, the shared path for a click and a marquee.
    ///
    /// A `Replace` of an empty `ids` clears the selection -- clicking empty
    /// canvas deselects -- which is why the mode is applied even when there is
    /// nothing to select.
    pub fn apply(&mut self, mode: SelectMode, ids: &[NodeId]) {
        match mode {
            SelectMode::Replace => {
                self.ids.clear();
                self.ids.extend(ids.iter().copied());
            }
            SelectMode::Add => self.ids.extend(ids.iter().copied()),
            SelectMode::Toggle => {
                for &id in ids {
                    self.toggle(id);
                }
            }
        }
    }

    /// Drops the ids of shapes that are no longer in `doc`.
    ///
    /// Call it after a deletion, or after an undo or redo that might have been
    /// one. A stale id is harmless until then: every read of it returns `None`.
    pub fn retain_live(&mut self, doc: &Document) {
        self.ids.retain(|&id| doc.get(id).is_some());
    }

    /// The document-space box around everything selected.
    ///
    /// [`Rect::EMPTY`] when nothing live is selected, which is the union
    /// identity and so folds correctly into a larger box rather than dragging
    /// it to the origin.
    #[must_use]
    pub fn bounds(&self, doc: &Document) -> Rect {
        self.ids
            .iter()
            .filter_map(|&id| doc.bounds(id))
            .fold(Rect::EMPTY, Rect::union)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::tests::{grid_document, rect_shape};
    use crate::transform::{Transform, gesture_start};
    use crate::undo::UndoLog;
    use hane_geom::{Affine, Vec2};

    fn document() -> (Document, Vec<NodeId>) {
        let mut doc = Document::new(Rect::new(-100.0, -100.0, 100.0, 100.0));
        let ids = (0..4)
            .map(|i| {
                let x = f64::from(i) * 20.0;
                doc.insert(rect_shape(Rect::new(x, 0.0, x + 10.0, 10.0)))
            })
            .collect();
        (doc, ids)
    }

    #[test]
    fn single_add_toggle_and_clear() {
        let (_doc, ids) = document();
        let mut sel = Selection::new();
        assert!(sel.is_empty());

        sel.select(ids[0]);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![ids[0]]);
        sel.select(ids[1]);
        assert_eq!(
            sel.iter().collect::<Vec<_>>(),
            vec![ids[1]],
            "single replaces"
        );

        sel.add(ids[2]);
        assert_eq!(sel.len(), 2);
        sel.add(ids[2]);
        assert_eq!(sel.len(), 2, "adding twice is once");

        sel.toggle(ids[2]);
        assert!(!sel.contains(ids[2]));
        sel.toggle(ids[2]);
        assert!(sel.contains(ids[2]));

        sel.clear();
        assert!(sel.is_empty());
    }

    #[test]
    fn the_three_modifier_modes() {
        let (_doc, ids) = document();
        let mut sel = Selection::new();
        sel.apply(SelectMode::Replace, &ids[0..2]);
        assert_eq!(sel.len(), 2);
        sel.apply(SelectMode::Add, &ids[2..4]);
        assert_eq!(sel.len(), 4);
        sel.apply(SelectMode::Toggle, &ids[1..3]);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![ids[0], ids[3]]);
        sel.apply(SelectMode::Replace, &[]);
        assert!(sel.is_empty(), "a plain click on empty canvas deselects");
    }

    #[test]
    fn the_bounds_of_a_multi_selection_span_all_of_it() {
        let (doc, ids) = document();
        let mut sel = Selection::new();
        assert!(
            sel.bounds(&doc).is_empty(),
            "nothing selected bounds nothing"
        );

        sel.select(ids[0]);
        assert_eq!(sel.bounds(&doc), Rect::new(0.0, 0.0, 10.0, 10.0));
        sel.add(ids[3]);
        // The gap between them is inside the union: this is the box, not the
        // shapes.
        assert_eq!(sel.bounds(&doc), Rect::new(0.0, 0.0, 70.0, 10.0));
    }

    #[test]
    fn selected_bounds_follow_the_shapes_they_name() {
        let (mut doc, ids) = document();
        let mut sel = Selection::new();
        sel.apply(SelectMode::Replace, &ids);
        let before = sel.bounds(&doc);
        for &id in &ids {
            doc.set_transform(id, Affine::translate(Vec2::new(0.0, 5.0)));
        }
        assert_eq!(sel.bounds(&doc), before.translate(Vec2::new(0.0, 5.0)));
    }

    #[test]
    fn deleted_ids_are_dropped() {
        let (mut doc, ids) = document();
        let mut sel = Selection::new();
        sel.apply(SelectMode::Replace, &ids);
        doc.remove(ids[1]);
        // Stale until swept, and inert meanwhile: it contributes no bounds.
        assert_eq!(sel.len(), 4);
        assert_eq!(sel.bounds(&doc), Rect::new(0.0, 0.0, 70.0, 10.0));
        sel.retain_live(&doc);
        assert_eq!(sel.iter().collect::<Vec<_>>(), vec![ids[0], ids[2], ids[3]]);
    }

    #[test]
    fn a_reused_slot_does_not_resurrect_a_selection() {
        let (mut doc, ids) = document();
        let mut sel = Selection::new();
        sel.select(ids[0]);
        doc.remove(ids[0]);
        // The arena hands this the slot `ids[0]` freed. A plain index would
        // make it selected; a generational id does not.
        let fresh = doc.insert(rect_shape(Rect::new(0.0, 0.0, 1.0, 1.0)));
        assert!(!sel.contains(fresh));
        sel.retain_live(&doc);
        assert!(sel.is_empty());
    }

    #[test]
    fn the_selection_survives_undo_and_redo_of_unrelated_edits() {
        let (mut doc, ids) = document();
        let mut sel = Selection::new();
        sel.apply(SelectMode::Replace, &ids[0..2]);
        let before = sel.bounds(&doc);

        let mut log = UndoLog::new(16);
        for &id in &ids[2..4] {
            let start = gesture_start(&doc, &{
                let mut s = Selection::new();
                s.select(id);
                s
            });
            log.edit(
                &mut doc,
                Transform::new(&start, Affine::translate(Vec2::new(3.0, 4.0))),
            );
            log.seal();
        }
        assert_eq!(sel.iter().collect::<Vec<_>>(), ids[0..2].to_vec());
        assert_eq!(sel.bounds(&doc), before);

        while log.undo(&mut doc) {}
        assert_eq!(sel.iter().collect::<Vec<_>>(), ids[0..2].to_vec());
        assert_eq!(sel.bounds(&doc), before);
        while log.redo(&mut doc) {}
        assert_eq!(sel.iter().collect::<Vec<_>>(), ids[0..2].to_vec());
        assert_eq!(sel.bounds(&doc), before);
    }

    #[test]
    fn a_hundred_thousand_selected_ids_stay_a_set() {
        let doc = grid_document(100_000);
        let mut sel = Selection::new();
        let ids: Vec<NodeId> = (0..100_000).map(NodeId::from_bits).collect();
        sel.apply(SelectMode::Replace, &ids);
        // Every id is live in a fresh arena, so this is the whole document --
        // and adding them twice must not double the set.
        sel.apply(SelectMode::Add, &ids);
        assert_eq!(sel.len(), 100_000);
        sel.retain_live(&doc);
        assert_eq!(sel.len(), 100_000);
        assert!(!sel.bounds(&doc).is_empty());
    }
}
