//! Groups and paint order: a tree over the document's shapes, and the
//! undoable command that reorders them.
//!
//! # A group holds no transform
//!
//! The usual design gives a group a matrix and composes it onto each child at
//! render time. This one does not, and transforming a group instead writes the
//! gesture into each child's own transform -- exactly what a multi-selection
//! already does through [`Transform`](crate::transform::Transform).
//!
//! That is what makes "group and ungroup preserve visual appearance exactly"
//! true in the strong sense rather than the approximate one. Grouping changes
//! no coordinate at all, so the flattened geometry afterwards is the same
//! bits. With a matrix on the group it could not be: ungrouping has to fold
//! that matrix into the children, and `T * (C * p)` is not `(T * C) * p` in
//! f64 -- appearance would shift by a rounding step every time a group was
//! opened and closed.
//!
//! The cost is that a child's stored coordinates move when its group is
//! transformed. Nothing above this reads them expecting otherwise.
//!
//! # Paint order stays in the document
//!
//! [`Document`] already gives every shape a `z`, and the hit test resolves
//! overlaps with it. Groups do not own a second ordering that could disagree;
//! they only decide *which shapes move together* when one is reordered. A
//! group whose members are interleaved with other shapes is pulled together by
//! the first reorder, which is what every editor does.

use crate::document::Document;
use crate::undo::Command;
use hane_scene::NodeId;
use std::collections::BTreeMap;

/// A handle to a group.
///
/// Slots are never reused, so an id is unique for the life of the tree even
/// after the group is dissolved. That is what lets an undo of an ungroup put
/// the *same* group back, rather than a new one that every other reference has
/// to be rewritten to point at -- the problem a generational arena id has when
/// a deleted node is restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(u32);

impl GroupId {
    /// The id as a raw `u32`, for handing to JS.
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }
}

/// What a group can contain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Member {
    /// A shape in the document.
    Shape(NodeId),
    /// A nested group.
    Group(GroupId),
}

/// The grouping tree over a document's shapes.
///
/// Only grouped shapes appear here: an ungrouped shape is its own root, so an
/// empty tree is a document with no groups and costs nothing.
#[derive(Debug, Default)]
pub struct Groups {
    /// Members of each group by slot, `None` once it is dissolved. Slots are
    /// never reused -- see [`GroupId`].
    groups: Vec<Option<Vec<Member>>>,
    /// The group each member sits in.
    parents: BTreeMap<Member, GroupId>,
}

impl Groups {
    /// A tree with no groups.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts `members` in a new group and returns it.
    ///
    /// The group takes the place of its members inside whatever group the
    /// first of them was in, so grouping inside a group nests rather than
    /// escaping. Members are removed from wherever they were, which is what
    /// grouping across two groups has to mean.
    ///
    /// Nothing about the document changes, so nothing about the drawing does.
    pub fn group(&mut self, members: &[Member]) -> GroupId {
        let id = GroupId(self.groups.len() as u32);
        self.groups.push(Some(Vec::new()));
        self.fill(id, members);
        id
    }

    /// Puts `members` back into the dissolved group `id`, the exact inverse of
    /// [`ungroup`](Groups::ungroup) including the id.
    ///
    /// Returns false if `id` is live or was never issued.
    pub fn regroup(&mut self, id: GroupId, members: &[Member]) -> bool {
        match self.groups.get_mut(id.0 as usize) {
            Some(slot @ None) => *slot = Some(Vec::new()),
            _ => return false,
        }
        self.fill(id, members);
        true
    }

    /// Moves `members` into the live group `id`, taking its parent from the
    /// first member that has one.
    fn fill(&mut self, id: GroupId, members: &[Member]) {
        if let Some(&parent) = members.iter().find_map(|m| self.parents.get(m)) {
            self.detach(Member::Group(id));
            self.parents.insert(Member::Group(id), parent);
            if let Some(Some(list)) = self.groups.get_mut(parent.0 as usize) {
                list.push(Member::Group(id));
            }
        }
        for &m in members {
            // A group cannot contain itself, and a member already inside `id`
            // must not be listed twice.
            if m == Member::Group(id) || self.parents.get(&m) == Some(&id) {
                continue;
            }
            self.detach(m);
            self.parents.insert(m, id);
            if let Some(Some(list)) = self.groups.get_mut(id.0 as usize) {
                list.push(m);
            }
        }
    }

    /// Dissolves `id`, returning what was in it. Its members inherit its
    /// parent, so a nested group's contents stay in the enclosing group.
    pub fn ungroup(&mut self, id: GroupId) -> Vec<Member> {
        let Some(slot) = self.groups.get_mut(id.0 as usize) else {
            return Vec::new();
        };
        let Some(members) = slot.take() else {
            return Vec::new();
        };
        let parent = self.parents.remove(&Member::Group(id));
        if let Some(parent) = parent
            && let Some(Some(list)) = self.groups.get_mut(parent.0 as usize)
        {
            let at = list.iter().position(|&m| m == Member::Group(id));
            if let Some(at) = at {
                list.splice(at..=at, members.iter().copied());
            }
        }
        for &m in &members {
            match parent {
                Some(parent) => self.parents.insert(m, parent),
                None => self.parents.remove(&m),
            };
        }
        members
    }

    /// What is directly inside `id`, or nothing if it has been dissolved.
    #[must_use]
    pub fn members(&self, id: GroupId) -> &[Member] {
        self.groups
            .get(id.0 as usize)
            .and_then(Option::as_ref)
            .map_or(&[], Vec::as_slice)
    }

    /// The group `member` sits directly in.
    #[must_use]
    pub fn parent(&self, member: Member) -> Option<GroupId> {
        self.parents.get(&member).copied()
    }

    /// The outermost group containing `member`, or `member` itself when it is
    /// in none -- what a click on a shape should select.
    #[must_use]
    pub fn root(&self, member: Member) -> Member {
        let mut current = member;
        // Bounded by the number of groups: a cycle would otherwise hang the
        // editor, and `fill` refuses the only way to make one.
        for _ in 0..=self.groups.len() {
            match self.parents.get(&current) {
                Some(&parent) => current = Member::Group(parent),
                None => return current,
            }
        }
        current
    }

    /// Every shape under `member`, nested groups included, in tree order.
    ///
    /// This is what a group transform applies to and what a reorder moves as
    /// one block.
    #[must_use]
    pub fn shapes(&self, member: Member) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect(member, &mut out);
        out
    }

    fn collect(&self, member: Member, out: &mut Vec<NodeId>) {
        match member {
            Member::Shape(id) => out.push(id),
            Member::Group(id) => {
                for &m in self.members(id) {
                    self.collect(m, out);
                }
            }
        }
    }

    /// Removes `member` from its parent's list.
    fn detach(&mut self, member: Member) {
        if let Some(parent) = self.parents.remove(&member)
            && let Some(Some(list)) = self.groups.get_mut(parent.0 as usize)
        {
            list.retain(|&m| m != member);
        }
    }
}

/// An undoable change of paint order.
///
/// Like every command here it names the state to install rather than a
/// movement, and stores what each shape had so a revert is a copy back.
pub struct Reorder {
    /// The paint order to install on each shape.
    targets: Vec<(NodeId, u32)>,
    /// What each had before, captured by the first `apply`. `None` for a shape
    /// deleted meanwhile.
    before: Option<Vec<Option<u32>>>,
}

impl Reorder {
    /// Moves `member` one step towards the viewer.
    ///
    /// One step is past the whole of the next thing above it, so raising a
    /// shape past a group clears the group rather than landing inside it.
    #[must_use]
    pub fn raise(doc: &Document, groups: &Groups, member: Member) -> Self {
        Self::step(doc, groups, member, true)
    }

    /// Moves `member` one step away from the viewer.
    #[must_use]
    pub fn lower(doc: &Document, groups: &Groups, member: Member) -> Self {
        Self::step(doc, groups, member, false)
    }

    /// Moves `member` above everything else.
    #[must_use]
    pub fn to_front(doc: &Document, groups: &Groups, member: Member) -> Self {
        let (order, moving) = plan(doc, groups, member);
        let rest = without(&order, &moving);
        Self::at(doc, &rest, &moving, rest.len())
    }

    /// Moves `member` below everything else.
    #[must_use]
    pub fn to_back(doc: &Document, groups: &Groups, member: Member) -> Self {
        let (order, moving) = plan(doc, groups, member);
        let rest = without(&order, &moving);
        Self::at(doc, &rest, &moving, 0)
    }

    /// The ids this command reorders.
    pub fn targets(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.targets.iter().map(|&(id, _)| id)
    }

    fn step(doc: &Document, groups: &Groups, member: Member, up: bool) -> Self {
        let (order, moving) = plan(doc, groups, member);
        let rest = without(&order, &moving);
        // The neighbour is the first shape past the *end* of the block, not
        // past its start: a block with something interleaved in it steps over
        // what is outside it, never over its own members.
        let neighbour = if up {
            let top = order.iter().rposition(|id| moving.contains(id));
            top.and_then(|t| order[t + 1..].iter().find(|id| !moving.contains(id)))
        } else {
            let bottom = order.iter().position(|id| moving.contains(id));
            bottom.and_then(|b| order[..b].iter().rev().find(|id| !moving.contains(id)))
        };
        // Its whole root block is stepped over, so a raise never lands inside
        // a group or splits one in half.
        let index = neighbour.and_then(|&id| {
            let block = groups.shapes(groups.root(Member::Shape(id)));
            let at = rest.iter().enumerate().filter(|(_, r)| block.contains(*r));
            if up {
                at.map(|(i, _)| i + 1).next_back()
            } else {
                at.map(|(i, _)| i).next()
            }
        });
        // Nothing above (or below): it is already as far as it goes, and
        // landing back at the same end is the no-op that says so.
        let fallback = if up { rest.len() } else { 0 };
        Self::at(doc, &rest, &moving, index.unwrap_or(fallback))
    }

    /// The command putting `moving` into `rest` at index `at`.
    fn at(doc: &Document, rest: &[NodeId], moving: &[NodeId], at: usize) -> Self {
        let mut order = Vec::with_capacity(rest.len() + moving.len());
        order.extend_from_slice(&rest[..at]);
        order.extend_from_slice(moving);
        order.extend_from_slice(&rest[at..]);
        Self {
            // Only what actually moves: a raise of one shape is two entries,
            // not a copy of the document's whole ordering.
            targets: order
                .into_iter()
                .enumerate()
                .map(|(i, id)| (id, i as u32))
                .filter(|&(id, z)| doc.z(id) != z)
                .collect(),
            before: None,
        }
    }
}

/// The document's paint order, and the shapes `member` moves as one block.
///
/// ponytail: the membership tests below are linear scans of the block, so a
/// reorder is `O(shapes * block)`. It is a keystroke, not a pointer move, and
/// a block is normally a handful of shapes -- put the block in a `BTreeSet`
/// when someone raises a group of tens of thousands.
fn plan(doc: &Document, groups: &Groups, member: Member) -> (Vec<NodeId>, Vec<NodeId>) {
    let order = doc.z_order();
    let shapes = groups.shapes(member);
    // In paint order, not tree order: the block keeps the relative order it is
    // drawn in, so moving it cannot reshuffle its own members.
    let moving: Vec<NodeId> = order
        .iter()
        .copied()
        .filter(|id| shapes.contains(id))
        .collect();
    (order, moving)
}

/// `order` without the members of `moving`.
fn without(order: &[NodeId], moving: &[NodeId]) -> Vec<NodeId> {
    order
        .iter()
        .copied()
        .filter(|id| !moving.contains(id))
        .collect()
}

impl Command for Reorder {
    type Doc = Document;

    fn apply(&mut self, doc: &mut Document) {
        self.before = Some(
            self.targets
                .iter()
                .map(|&(id, z)| doc.set_z(id, z))
                .collect(),
        );
    }

    fn revert(&mut self, doc: &mut Document) {
        let Some(before) = self.before.take() else {
            return;
        };
        for (&(id, _), previous) in self.targets.iter().zip(before) {
            if let Some(previous) = previous {
                doc.set_z(id, previous);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Shape;
    use crate::document::tests::rect_shape;
    use crate::select::Selection;
    use crate::transform::{Transform, gesture_start};
    use crate::undo::UndoLog;
    use hane_geom::{Affine, PathEl, Point, Rect, Vec2};

    /// Four squares, bottom to top.
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

    /// Exactly what would be drawn: every shape in paint order, with its
    /// control points already placed in the document, as raw bits. Two of
    /// these being equal is the two documents being pixel-identical.
    fn rendered(doc: &Document) -> Vec<u64> {
        let mut out = Vec::new();
        for id in doc.z_order() {
            let shape: &Shape = doc.get(id).unwrap();
            out.push(u64::from(id.to_bits()));
            for &el in shape.path.transform(shape.transform).elements() {
                let points: [Point; 3] = match el {
                    PathEl::MoveTo(a) | PathEl::LineTo(a) => [a, a, a],
                    PathEl::QuadTo(a, b) => [a, b, b],
                    PathEl::CurveTo(a, b, c) => [a, b, c],
                    PathEl::ClosePath => [Point::ORIGIN; 3],
                };
                for q in points {
                    out.push(q.x.to_bits());
                    out.push(q.y.to_bits());
                }
            }
        }
        out
    }

    fn shapes(ids: &[NodeId]) -> Vec<Member> {
        ids.iter().copied().map(Member::Shape).collect()
    }

    /// Builds a reorder and applies it. Two steps because the command reads
    /// the order it is about to change.
    fn run(
        doc: &mut Document,
        log: &mut UndoLog<Reorder>,
        build: fn(&Document, &Groups, Member) -> Reorder,
        groups: &Groups,
        member: Member,
    ) {
        let cmd = build(doc, groups, member);
        log.edit(doc, cmd);
    }

    #[test]
    fn grouping_and_ungrouping_change_nothing_that_is_drawn() {
        let (doc, ids) = document();
        let before = rendered(&doc);
        let mut groups = Groups::new();
        let g = groups.group(&shapes(&ids[0..2]));
        let inner = groups.group(&shapes(&ids[2..4]));
        let outer = groups.group(&[Member::Group(g), Member::Group(inner)]);
        assert_eq!(rendered(&doc), before, "grouping moved something");

        groups.ungroup(outer);
        groups.ungroup(g);
        groups.ungroup(inner);
        assert_eq!(rendered(&doc), before, "ungrouping moved something");
        assert!(
            ids.iter()
                .all(|&id| groups.parent(Member::Shape(id)).is_none())
        );
    }

    #[test]
    fn nested_groups_flatten_and_resolve_to_one_root() {
        let (_doc, ids) = document();
        let mut groups = Groups::new();
        let inner = groups.group(&shapes(&ids[0..2]));
        let outer = groups.group(&[Member::Group(inner), Member::Shape(ids[2])]);

        assert_eq!(groups.shapes(Member::Group(outer)), ids[0..3].to_vec());
        assert_eq!(groups.root(Member::Shape(ids[0])), Member::Group(outer));
        assert_eq!(groups.root(Member::Shape(ids[3])), Member::Shape(ids[3]));
        assert_eq!(groups.parent(Member::Shape(ids[0])), Some(inner));
        assert_eq!(groups.parent(Member::Group(inner)), Some(outer));

        // Grouping inside a group nests: the new group joins the one its
        // members were in rather than escaping to the top.
        let deeper = groups.group(&shapes(&ids[0..1]));
        assert_eq!(groups.parent(Member::Group(deeper)), Some(inner));
        assert_eq!(groups.root(Member::Shape(ids[0])), Member::Group(outer));

        // Dissolving the middle group leaves its contents where they were.
        groups.ungroup(inner);
        assert_eq!(groups.parent(Member::Group(deeper)), Some(outer));
        assert_eq!(groups.shapes(Member::Group(outer)).len(), 3);
    }

    #[test]
    fn an_ungrouped_group_can_be_put_back_under_the_same_id() {
        let (_doc, ids) = document();
        let mut groups = Groups::new();
        let g = groups.group(&shapes(&ids[0..2]));
        let members = groups.ungroup(g);
        assert!(groups.members(g).is_empty());
        // The arena's generational ids cannot do this -- a restored node comes
        // back with a fresh id and every reference to it is stale. A group id
        // is never recycled, so undoing an ungroup restores the id too.
        assert!(groups.regroup(g, &members));
        assert_eq!(groups.members(g), &shapes(&ids[0..2])[..]);
        assert!(!groups.regroup(g, &members), "already live");
    }

    #[test]
    fn raise_lower_front_and_back_move_one_shape() {
        let (mut doc, ids) = document();
        let groups = Groups::new();
        let mut log = UndoLog::new(16);
        let order = |doc: &Document| doc.z_order();
        assert_eq!(order(&doc), ids);

        let one = Member::Shape(ids[0]);
        run(&mut doc, &mut log, Reorder::raise, &groups, one);
        assert_eq!(order(&doc), vec![ids[1], ids[0], ids[2], ids[3]]);
        log.seal();
        run(&mut doc, &mut log, Reorder::lower, &groups, one);
        assert_eq!(order(&doc), ids);
        log.seal();
        run(&mut doc, &mut log, Reorder::to_front, &groups, one);
        assert_eq!(order(&doc), vec![ids[1], ids[2], ids[3], ids[0]]);
        log.seal();
        run(&mut doc, &mut log, Reorder::to_back, &groups, one);
        assert_eq!(order(&doc), ids);
        log.seal();

        // Already at the bottom: a no-op, not a wrap round to the top.
        run(&mut doc, &mut log, Reorder::lower, &groups, one);
        assert_eq!(order(&doc), ids);
        run(&mut doc, &mut log, Reorder::to_back, &groups, one);
        assert_eq!(order(&doc), ids);

        while log.undo(&mut doc) {}
        assert_eq!(order(&doc), ids, "undo restores the order exactly");
    }

    #[test]
    fn a_group_reorders_as_one_block() {
        let (mut doc, ids) = document();
        let mut groups = Groups::new();
        // The two ends of the z range, so the group is not contiguous to begin
        // with and the reorder has to gather it.
        let g = groups.group(&[Member::Shape(ids[0]), Member::Shape(ids[3])]);
        let mut log = UndoLog::new(8);

        run(
            &mut doc,
            &mut log,
            Reorder::to_front,
            &groups,
            Member::Group(g),
        );
        assert_eq!(doc.z_order(), vec![ids[1], ids[2], ids[0], ids[3]]);
        log.seal();
        // Lowering steps over the whole of what is below, which here is two
        // ungrouped shapes taken one at a time.
        run(
            &mut doc,
            &mut log,
            Reorder::lower,
            &groups,
            Member::Group(g),
        );
        assert_eq!(doc.z_order(), vec![ids[1], ids[0], ids[3], ids[2]]);
        log.seal();

        // And a shape raised past a group clears the whole group.
        run(
            &mut doc,
            &mut log,
            Reorder::raise,
            &groups,
            Member::Shape(ids[1]),
        );
        assert_eq!(doc.z_order(), vec![ids[0], ids[3], ids[1], ids[2]]);

        while log.undo(&mut doc) {}
        assert_eq!(doc.z_order(), ids);
    }

    #[test]
    fn z_order_is_total_and_survives_a_round_trip() {
        let (mut doc, ids) = document();
        let groups = Groups::new();
        let mut log = UndoLog::new(8);
        let (up, down) = (Member::Shape(ids[1]), Member::Shape(ids[2]));
        run(&mut doc, &mut log, Reorder::to_front, &groups, up);
        run(&mut doc, &mut log, Reorder::to_back, &groups, down);
        let order = doc.z_order();

        // Serialisation is writing the shapes out in paint order and reading
        // them back: insertion order is paint order, so the order survives if
        // and only if it is a total order of the shapes.
        let mut reloaded = Document::new(Rect::new(-100.0, -100.0, 100.0, 100.0));
        let fresh: Vec<NodeId> = order
            .iter()
            .map(|&id| reloaded.insert(doc.get(id).unwrap().clone()))
            .collect();
        assert_eq!(reloaded.z_order(), fresh);
        assert_eq!(rendered(&reloaded).len(), rendered(&doc).len());
        // Removing a shape leaves a gap in the z values; the order that
        // remains must still be the one it was.
        doc.remove(order[1]);
        assert_eq!(doc.z_order(), vec![order[0], order[2], order[3]]);
    }

    #[test]
    fn transforming_a_group_transforms_its_children() {
        let (mut doc, ids) = document();
        let mut groups = Groups::new();
        let inner = groups.group(&shapes(&ids[0..2]));
        let outer = groups.group(&[Member::Group(inner), Member::Shape(ids[2])]);

        let mut selection = Selection::new();
        for id in groups.shapes(Member::Group(outer)) {
            selection.add(id);
        }
        let start = gesture_start(&doc, &selection);
        let before = selection.bounds(&doc);
        let untouched = doc.bounds(ids[3]);

        let mut log = UndoLog::new(8);
        let gesture = Affine::translate(Vec2::new(7.0, -3.0));
        log.edit(&mut doc, Transform::new(&start, gesture));
        assert_eq!(
            selection.bounds(&doc),
            before.translate(Vec2::new(7.0, -3.0))
        );
        assert_eq!(doc.bounds(ids[3]), untouched, "and nothing else moved");

        assert!(log.undo(&mut doc));
        assert_eq!(selection.bounds(&doc), before);
    }
}
