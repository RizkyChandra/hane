//! The undo log (D-007): a log of reversible commands, never snapshots.
//!
//! # Why a log and not snapshots
//!
//! A snapshot per undo step copies the whole document, so both the time and
//! the memory of one edit scale with the document, not with the edit. At the
//! 100k objects this engine targets that is dead on arrival. Here an entry is
//! one [`Command`] -- a few words describing *what changed* plus whatever it
//! must carry to put it back -- so undo cost scales with the edit and the log
//! never touches the objects an edit did not.
//!
//! Nothing in this module can copy a document even by accident: it never sees
//! one except as the `&mut` it hands to `apply` and `revert`, and it has no
//! `Clone` bound anywhere.

use std::collections::VecDeque;

/// A reversible, replayable edit to a document.
///
/// One implementation of a trait is normally banned by `AGENTS.md`, and this
/// crate deliberately provides none: the abstraction *is* the deliverable.
/// The scene layer owns the concrete edit operations and does not exist yet
/// (`hane-scene` is growing an arena under a parallel branch), so the log is
/// written against this seam instead of against a scene type it would have to
/// chase. `hane-edit` depends on no scene detail at all.
///
/// `UndoLog<C>` is generic rather than `dyn`: `C` is expected to be one enum
/// of every edit operation, which keeps an entry inline and allocation-free
/// and lets [`merge`](Command::merge) match on both sides at once.
///
/// # Contract
///
/// `revert` must exactly undo the immediately preceding `apply` of the *same*
/// value, and `apply` must be re-runnable after a `revert` -- that pair is
/// what undo and redo are. Both take `&mut self` so a command can stash what
/// it destroyed (a deleted object, a replaced style) inside itself and hand
/// it back on revert.
pub trait Command {
    /// The document type this command edits.
    type Doc;

    /// Applies the edit, storing whatever [`revert`](Command::revert) needs.
    fn apply(&mut self, doc: &mut Self::Doc);

    /// Undoes exactly what the matching [`apply`](Command::apply) did.
    fn revert(&mut self, doc: &mut Self::Doc);

    /// Absorbs `next` into `self` so that one `revert` undoes both, returning
    /// `Some(next)` when the two must stay separate undo steps.
    ///
    /// This is the whole of coalescing: a drag emits hundreds of small moves
    /// and the user expects one undo step, so a move absorbs a later move of
    /// the same target, keeping the position the group started from. `next`
    /// has already been applied to the document when this is called -- merging
    /// only rewrites history, never the document.
    ///
    /// Refusing is always correct, so the default never merges.
    fn merge(&mut self, next: Self) -> Option<Self>
    where
        Self: Sized,
    {
        Some(next)
    }
}

/// A bounded log of applied commands, with the undone ones kept for redo.
///
/// # Coalescing rule
///
/// A command merges into the newest entry when, and only when, both hold:
///
/// 1. the log is *open* -- no [`seal`](UndoLog::seal) since that entry was
///    pushed, and no undo or redo, either of which seals implicitly; and
/// 2. [`Command::merge`] accepts it.
///
/// Nothing here guesses from timing. The caller knows where a gesture ends --
/// pointer-up, tool change, selection change -- and says so by calling
/// `seal`, which is both cheaper and more predictable than an idle timeout
/// that fires mid-drag when a laptop stalls.
pub struct UndoLog<C> {
    /// Applied commands, oldest first. The newest is the next to be undone.
    done: VecDeque<C>,
    /// Undone commands, in the order they were undone. The last redoes first.
    undone: Vec<C>,
    /// Maximum number of entries in `done`; the oldest are dropped past it.
    capacity: usize,
    /// Whether the newest entry in `done` may still absorb a command.
    open: bool,
}

impl<C: Command> UndoLog<C> {
    // ponytail: bounded by step count, which bounds bytes only because a
    // command is small. One command that carries a big payload -- "delete
    // these 40k objects" -- breaks that; give `Command` a `weight` hook and
    // evict on a byte budget when such a command exists.
    /// An empty log holding at most `capacity` undoable steps.
    ///
    /// # Panics
    ///
    /// If `capacity` is zero, which would silently discard every edit.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "an undo log needs room for at least one step");
        Self {
            done: VecDeque::new(),
            undone: Vec::new(),
            capacity,
            open: false,
        }
    }

    /// Applies `cmd` and records it, coalescing it into the newest entry when
    /// the rule above allows.
    pub fn edit(&mut self, doc: &mut C::Doc, mut cmd: C) {
        cmd.apply(doc);
        // A new edit forks history: what was undone is now unreachable.
        self.undone.clear();
        if self.open
            && let Some(top) = self.done.back_mut()
        {
            match top.merge(cmd) {
                None => return,
                Some(rejected) => cmd = rejected,
            }
        }
        if self.done.len() == self.capacity {
            // Dropping the oldest entry drops what it was holding for revert,
            // so that step is gone for good -- the price of a bounded log.
            self.done.pop_front();
        }
        self.done.push_back(cmd);
        self.open = true;
    }

    /// Ends the current coalescing group, so the next edit starts a new undo
    /// step. Call it on pointer-up, tool change, or selection change.
    pub fn seal(&mut self) {
        self.open = false;
    }

    /// Reverts the newest entry, returning whether there was one.
    pub fn undo(&mut self, doc: &mut C::Doc) -> bool {
        match self.done.pop_back() {
            Some(mut cmd) => {
                cmd.revert(doc);
                self.undone.push(cmd);
                self.open = false;
                true
            }
            None => false,
        }
    }

    /// Re-applies the most recently undone entry, returning whether there was
    /// one.
    pub fn redo(&mut self, doc: &mut C::Doc) -> bool {
        match self.undone.pop() {
            Some(mut cmd) => {
                cmd.apply(doc);
                self.done.push_back(cmd);
                // A redone step is history, not the group being built now.
                self.open = false;
                true
            }
            None => false,
        }
    }

    /// Whether [`undo`](UndoLog::undo) would do anything.
    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    /// Whether [`redo`](UndoLog::redo) would do anything.
    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// The number of undoable steps, after coalescing.
    pub fn len(&self) -> usize {
        self.done.len()
    }

    /// Whether there are no undoable steps.
    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::Point;
    use hane_geom::fuzz::{Rng, check};

    /// A document standing in for the scene graph: objects in z-order, which
    /// makes position in the vector part of the state a revert has to
    /// restore, not just the coordinates.
    struct Doc {
        objects: Vec<(u32, Point)>,
    }

    impl Doc {
        fn index_of(&self, id: u32) -> Option<usize> {
            self.objects.iter().position(|&(i, _)| i == id)
        }

        /// Exact serialisation: coordinates as bit patterns, in z-order, so
        /// equality of two of these is equality of the documents.
        fn serialise(&self) -> String {
            let mut s = String::new();
            for &(id, p) in &self.objects {
                s.push_str(&format!(
                    "{id}:{:016x},{:016x};",
                    p.x.to_bits(),
                    p.y.to_bits()
                ));
            }
            s
        }
    }

    #[derive(Debug)]
    enum Op {
        /// A move is stated as a destination with the old position stashed on
        /// apply, not as a delta: `(p + d) - d` is not `p` in f64 once the
        /// magnitudes differ, so a delta command cannot restore state exactly
        /// and would fail the fuzz round-trip at large coordinates. Storing
        /// the old value also makes coalescing trivially exact.
        Move {
            id: u32,
            to: Point,
            from: Option<Point>,
        },
        Insert {
            id: u32,
            at: Point,
            /// Whether `apply` really inserted; the id may already be taken,
            /// and a revert that removed it anyway would delete an object it
            /// never created. The fuzz property caught exactly that.
            inserted: bool,
        },
        Delete {
            id: u32,
            /// Where it was and what it held, filled in by `apply`.
            removed: Option<(usize, Point)>,
        },
    }

    impl Command for Op {
        type Doc = Doc;

        fn apply(&mut self, doc: &mut Doc) {
            match self {
                Op::Move { id, to, from } => {
                    if let Some(i) = doc.index_of(*id) {
                        *from = Some(doc.objects[i].1);
                        doc.objects[i].1 = *to;
                    }
                }
                Op::Insert { id, at, inserted } => {
                    *inserted = doc.index_of(*id).is_none();
                    if *inserted {
                        doc.objects.push((*id, *at));
                    }
                }
                Op::Delete { id, removed } => {
                    *removed = doc.index_of(*id).map(|i| (i, doc.objects.remove(i).1));
                }
            }
        }

        fn revert(&mut self, doc: &mut Doc) {
            match self {
                Op::Move { id, from, .. } => {
                    if let (Some(i), Some(p)) = (doc.index_of(*id), from.take()) {
                        doc.objects[i].1 = p;
                    }
                }
                Op::Insert { id, inserted, .. } => {
                    if *inserted && let Some(i) = doc.index_of(*id) {
                        doc.objects.remove(i);
                    }
                }
                Op::Delete { id, removed } => {
                    if let Some((i, p)) = removed.take() {
                        doc.objects.insert(i, (*id, p));
                    }
                }
            }
        }

        fn merge(&mut self, next: Self) -> Option<Self> {
            match (self, next) {
                // The drag case: same object, so keep the position this group
                // started from and take the newest destination. Both sides
                // must have actually moved something, or the kept `from`
                // would not describe what the merged step has to restore.
                (
                    Op::Move {
                        id,
                        to,
                        from: Some(_),
                    },
                    Op::Move {
                        id: n,
                        to: t,
                        from: Some(_),
                    },
                ) if *id == n => {
                    *to = t;
                    None
                }
                (_, other) => Some(other),
            }
        }
    }

    fn move_to(id: u32, x: f64) -> Op {
        Op::Move {
            id,
            to: Point::new(x, 0.0),
            from: None,
        }
    }

    fn doc_with(ids: impl IntoIterator<Item = u32>) -> Doc {
        Doc {
            objects: ids.into_iter().map(|i| (i, Point::new(0.0, 0.0))).collect(),
        }
    }

    #[test]
    fn undo_then_redo_restores_serialisations_exactly() {
        let mut doc = doc_with([1, 2]);
        let start = doc.serialise();
        let mut log = UndoLog::new(16);

        log.edit(&mut doc, move_to(1, 3.0));
        log.seal();
        log.edit(
            &mut doc,
            Op::Insert {
                id: 9,
                at: Point::new(1.0, 2.0),
                inserted: false,
            },
        );
        log.seal();
        log.edit(
            &mut doc,
            Op::Delete {
                id: 1,
                removed: None,
            },
        );
        let edited = doc.serialise();

        while log.undo(&mut doc) {}
        assert_eq!(doc.serialise(), start);
        while log.redo(&mut doc) {}
        assert_eq!(doc.serialise(), edited);
    }

    #[test]
    fn delete_reverts_to_the_same_z_order_position() {
        let mut doc = doc_with([1, 2, 3]);
        let start = doc.serialise();
        let mut log = UndoLog::new(4);
        log.edit(
            &mut doc,
            Op::Delete {
                id: 2,
                removed: None,
            },
        );
        assert!(log.undo(&mut doc));
        // Appending it back instead of inserting at index 1 would pass a
        // set-comparison and fail this one.
        assert_eq!(doc.serialise(), start);
    }

    #[test]
    fn a_drag_coalesces_into_one_step() {
        let mut doc = doc_with([1]);
        let start = doc.serialise();
        let mut log = UndoLog::new(64);
        for _ in 0..200 {
            log.edit(&mut doc, move_to(1, 0.5));
        }
        assert_eq!(log.len(), 1);
        assert!(log.undo(&mut doc));
        assert_eq!(doc.serialise(), start);
        assert!(!log.can_undo());
    }

    #[test]
    fn seals_and_unlike_commands_break_coalescing() {
        let mut doc = doc_with([1, 2]);
        let mut log = UndoLog::new(64);
        log.edit(&mut doc, move_to(1, 1.0));
        log.edit(&mut doc, move_to(1, 1.0));
        assert_eq!(log.len(), 1);
        log.seal();
        log.edit(&mut doc, move_to(1, 1.0));
        assert_eq!(log.len(), 2, "a seal ends the group");
        log.edit(&mut doc, move_to(2, 1.0));
        assert_eq!(log.len(), 3, "a different object is a different step");
        log.edit(
            &mut doc,
            Op::Insert {
                id: 7,
                at: Point::new(0.0, 0.0),
                inserted: false,
            },
        );
        assert_eq!(log.len(), 4, "a different kind of edit is a different step");
    }

    #[test]
    fn undo_seals_so_a_redone_step_is_not_reopened() {
        let mut doc = doc_with([1]);
        let mut log = UndoLog::new(8);
        log.edit(&mut doc, move_to(1, 1.0));
        log.undo(&mut doc);
        log.redo(&mut doc);
        log.edit(&mut doc, move_to(1, 1.0));
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let mut doc = doc_with([1]);
        let mut log = UndoLog::new(8);
        log.edit(&mut doc, move_to(1, 1.0));
        log.undo(&mut doc);
        assert!(log.can_redo());
        log.edit(&mut doc, move_to(1, 2.0));
        assert!(!log.can_redo());
    }

    #[test]
    fn the_log_is_bounded_and_drops_the_oldest_steps() {
        let mut doc = doc_with([1]);
        let mut log = UndoLog::new(4);
        for i in 0..100 {
            log.edit(&mut doc, move_to(1, f64::from(i)));
            log.seal();
        }
        assert_eq!(log.len(), 4);
        for _ in 0..4 {
            assert!(log.undo(&mut doc));
        }
        assert!(!log.undo(&mut doc), "steps past the bound are gone");
    }

    #[test]
    fn a_hundred_thousand_objects_undo_without_copying_the_document() {
        let mut doc = doc_with(0..100_000);
        let start = doc.serialise();
        let mut log = UndoLog::new(4096);
        for i in 0..1000 {
            log.edit(&mut doc, move_to(i * 97 % 100_000, 1.5));
            log.seal();
        }
        let edited = doc.serialise();
        while log.undo(&mut doc) {}
        assert_eq!(doc.serialise(), start);
        while log.redo(&mut doc) {}
        assert_eq!(doc.serialise(), edited);
        // The point is the cost, not just the result: a snapshot log would
        // have copied 100k objects a thousand times over and this test would
        // not finish in a test suite's patience. It is the linear scan in
        // `Doc::index_of` that dominates here, not the log.
    }

    /// Random command sequences, fully undone, must return the document to
    /// the byte-identical state it started in -- with seals landing at random
    /// so coalesced and separate steps both get exercised.
    #[test]
    fn fuzz_full_undo_returns_to_the_start_state() {
        fn generate(r: &mut Rng) -> Vec<(u64, u32, Point, bool)> {
            (0..r.below(40))
                .map(|_| (r.below(3), r.below(6) as u32, r.point(), r.below(2) == 0))
                .collect()
        }

        check("undo returns to the start state", 2000, generate, |ops| {
            let mut doc = doc_with([0, 1, 2]);
            let start = doc.serialise();
            let mut log = UndoLog::new(64);
            for &(kind, id, p, seal) in ops {
                // Built here rather than in `generate` because a command is
                // consumed by `edit`, and `check` only lends its input.
                let op = match kind {
                    0 => Op::Move {
                        id,
                        to: p,
                        from: None,
                    },
                    1 => Op::Insert {
                        id,
                        at: p,
                        inserted: false,
                    },
                    _ => Op::Delete { id, removed: None },
                };
                log.edit(&mut doc, op);
                if seal {
                    log.seal();
                }
            }
            let edited = doc.serialise();
            while log.undo(&mut doc) {}
            if doc.serialise() != start {
                return false;
            }
            while log.redo(&mut doc) {}
            doc.serialise() == edited
        });
    }
}
