//! The editor: one session's state, and the exports the DOM shell drives it
//! through (D-004, D-005).
//!
//! Everything a document is lives here, in linear memory. The shell holds no
//! scene state at all: it forwards pointer, wheel and key events as numbers and
//! strings, asks for a frame, and reads back one small JSON view-model for the
//! panels. Nothing that scales with the document crosses the boundary per
//! frame.
//!
//! # What `hane-edit` does not model, and why this file does
//!
//! [`Shape`] is geometry plus *whether* it is filled and stroked -- which is
//! all hit testing needs and all P5 was asked for. It carries no colour. Paint
//! lives here, in [`Slot`], beside the geometry rather than inside it, rather
//! than widening a crate whose shape a hundred tests pin for the sake of two
//! `Option<Color>`s.
//!
//! # Stable slots
//!
//! Undoing an insert removes the shape, and redoing it inserts a *different*
//! [`NodeId`] -- the arena's generation has moved on. A redo stack whose later
//! entries named the old id would then silently do nothing. So every object
//! also gets a slot index that is never reused, undo entries name slots, and
//! the id is looked up at the moment a command runs.

use std::cell::RefCell;
use std::collections::HashMap;

use hane_edit::{
    Command, Document, Handle, MarqueeMode, Pen, SelectMode, Selection, Shape, TransformBox,
    UndoLog, hit_test, marquee,
};
use hane_geom::{Affine, PathEl, Point, Rect, Vec2};
use hane_gpu::DrawData;
use hane_path::{Path, StrokeStyle};
use hane_raster::{Color, Scene};
use hane_scene::{Arena, NodeId, View};
use hane_svg::scene::{Node as SvgNode, NodeKind, Scene as SvgScene};
use hane_svg::xml::{Attribute, Element, Name};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::glctx;
use crate::glrender::{Gpu, group_depth};

/// Pointer slop for hit testing, in CSS pixels.
const HIT_PX: f64 = 4.0;

/// Full width of a transform handle, in CSS pixels.
const HANDLE_PX: f64 = 9.0;

/// How near the first node a pen click must land to close the path.
const CLOSE_PX: f64 = 8.0;

/// How far the pointer must travel before a press becomes a drag.
///
/// Without it, every click that twitches by a pixel creates a zero-sized shape
/// or nudges the selection -- and a trackpad twitches on every click.
const DRAG_PX: f64 = 3.0;

/// Cubic control-point offset for a quarter ellipse: `4/3 * (sqrt(2) - 1)`.
const KAPPA: f64 = 0.552_284_749_830_793_4;

/// The default page: A4 at 96 dpi, in document units.
const PAGE: Rect = Rect {
    x0: 0.0,
    y0: 0.0,
    x1: 794.0,
    y1: 1123.0,
};

/// How many undo steps are kept.
const UNDO_DEPTH: usize = 200;

/// The selection blue, used for every piece of editor chrome on the canvas.
const CHROME: Color = Color {
    r: 43,
    g: 127,
    b: 255,
    a: 255,
};

/// The page colour.
const PAPER: Color = Color {
    r: 255,
    g: 255,
    b: 255,
    a: 255,
};

/// Black: the default ink, and what an unparseable colour imports as.
const BLACK: Color = Color {
    r: 0,
    g: 0,
    b: 0,
    a: 255,
};

// ---------------------------------------------------------------------------
// paint
// ---------------------------------------------------------------------------

/// What a shape is painted with: the half of a style `hane-edit` does not hold.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Style {
    /// The interior colour, or `None` for an unfilled shape.
    fill: Option<Color>,
    /// The outline colour, or `None` for an unstroked one.
    stroke: Option<Color>,
    /// Stroke width in document units.
    width: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fill: Some(Color {
                r: 90,
                g: 105,
                b: 130,
                a: 255,
            }),
            stroke: None,
            width: 1.0,
        }
    }
}

impl Style {
    /// Copies this style's flags onto a shape, so that hit testing agrees with
    /// what is drawn: an unfilled shape is grabbable only by its outline.
    fn stamp(self, shape: &mut Shape) {
        shape.filled = self.fill.is_some();
        shape.stroke_width = self.stroke.map(|_| self.width);
    }
}

/// Parses `#rgb`, `#rrggbb`, `#rrggbbaa`, `none`/`transparent`, or one of the
/// sixteen HTML colour names.
///
/// ponytail: no `rgb()` notation, no `url(#gradient)`, and the name table is
/// the HTML 4 sixteen rather than the CSS 147. Anything else imports as black,
/// which is also what a browser does with an invalid `fill`. Widen it when a
/// real file needs it.
fn parse_color(text: &str) -> Option<Color> {
    let t = text.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("none") || t.eq_ignore_ascii_case("transparent") {
        return None;
    }
    if let Some(hex) = t.strip_prefix('#') {
        let d: Vec<u8> = hex
            .bytes()
            .filter_map(|b| (b as char).to_digit(16).map(|v| v as u8))
            .collect();
        let byte = |hi: u8, lo: u8| hi * 16 + lo;
        return Some(match d.len() {
            // `#rgb` doubles each digit: `#f00` is `#ff0000`, not `#f00000`.
            3 => Color {
                r: byte(d[0], d[0]),
                g: byte(d[1], d[1]),
                b: byte(d[2], d[2]),
                a: 255,
            },
            6 => Color {
                r: byte(d[0], d[1]),
                g: byte(d[2], d[3]),
                b: byte(d[4], d[5]),
                a: 255,
            },
            8 => Color {
                r: byte(d[0], d[1]),
                g: byte(d[2], d[3]),
                b: byte(d[4], d[5]),
                a: byte(d[6], d[7]),
            },
            _ => BLACK,
        });
    }
    /// The HTML 4 named colours, as `0xrrggbb`.
    const NAMED: &[(&str, u32)] = &[
        ("black", 0x0000_0000),
        ("silver", 0x00c0_c0c0),
        ("gray", 0x0080_8080),
        ("grey", 0x0080_8080),
        ("white", 0x00ff_ffff),
        ("maroon", 0x0080_0000),
        ("red", 0x00ff_0000),
        ("purple", 0x0080_0080),
        ("fuchsia", 0x00ff_00ff),
        ("green", 0x0000_8000),
        ("lime", 0x0000_ff00),
        ("olive", 0x0080_8000),
        ("yellow", 0x00ff_ff00),
        ("navy", 0x0000_0080),
        ("blue", 0x0000_00ff),
        ("teal", 0x0000_8080),
        ("aqua", 0x0000_ffff),
    ];
    let rgb = NAMED
        .iter()
        .find(|(name, _)| t.eq_ignore_ascii_case(name))
        .map_or(0, |&(_, rgb)| rgb);
    Some(Color {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
        a: 255,
    })
}

/// A colour as `#rrggbb`. The alpha is reported separately: the shell's picker
/// is `<input type="color">`, which has no alpha channel.
fn hex(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Parses an SVG length that is a plain number, ignoring any unit.
///
/// ponytail: `1.5mm` reads as 1.5. Real unit conversion belongs with the
/// viewport resolution `hane-svg` does not do either.
fn parse_len(text: &str) -> Option<f64> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e'))
        .unwrap_or(text.len());
    text[..end].parse().ok()
}

// ---------------------------------------------------------------------------
// the document
// ---------------------------------------------------------------------------

/// One object's slot: what outlives the shape being undone away.
struct Slot {
    /// The live id, or `None` while the object is deleted or undone.
    node: Option<NodeId>,
    style: Style,
    /// What the layer panel calls it.
    name: String,
}

/// The shapes, their paint, and the mapping undo entries are written against.
struct Doc {
    doc: Document,
    slots: Vec<Slot>,
    /// Which slot a live id belongs to. Rewritten on every insert and remove,
    /// which is where the id changes.
    by_node: HashMap<NodeId, usize>,
    /// Slots the edits since the last [`Doc::forget`] touched.
    ///
    /// Undo and redo are the callers: what a step put back is what should end
    /// up selected, and by then the ids involved are new ones no selection
    /// could have been holding.
    touched: Vec<usize>,
}

impl Doc {
    fn new() -> Self {
        Self {
            // Generous: shapes outside the index still work, they are just not
            // accelerated, and a user who pans off the page expects to draw.
            doc: Document::new(PAGE.inflate(PAGE.width() * 4.0)),
            slots: Vec::new(),
            by_node: HashMap::new(),
            touched: Vec::new(),
        }
    }

    /// Puts `shape` into the document under an existing slot.
    ///
    /// ponytail: it lands on top, because `Document::insert` hands out the next
    /// paint order and `set_z` is `pub(crate)`. So undoing a delete raises the
    /// shape. Fixing it properly means `hane-edit` exposing paint order, which
    /// is the same change the layer panel's reordering needs.
    fn add(&mut self, slot: usize, mut shape: Shape) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        entry.style.stamp(&mut shape);
        let id = self.doc.insert(shape);
        entry.node = Some(id);
        self.by_node.insert(id, slot);
        self.touched.push(slot);
    }

    /// Takes the shape of `slot` back out, leaving the slot and its style.
    fn take(&mut self, slot: usize) -> Option<Shape> {
        self.touched.push(slot);
        let id = self.slots.get_mut(slot)?.node.take()?;
        self.by_node.remove(&id);
        self.doc.remove(id)
    }

    /// A new slot holding `shape`, and its index.
    fn insert(&mut self, shape: Shape, style: Style, name: &str) -> usize {
        let slot = self.slots.len();
        self.slots.push(Slot {
            node: None,
            style,
            name: name.to_string(),
        });
        self.add(slot, shape);
        slot
    }

    fn slot_of(&self, id: NodeId) -> Option<usize> {
        self.by_node.get(&id).copied()
    }

    fn node_of(&self, slot: usize) -> Option<NodeId> {
        self.slots.get(slot).and_then(|s| s.node)
    }

    fn style_at(&self, slot: usize) -> Style {
        self.slots
            .get(slot)
            .map_or_else(Style::default, |s| s.style)
    }

    fn style_of(&self, id: NodeId) -> Style {
        self.slot_of(id)
            .map_or_else(Style::default, |s| self.style_at(s))
    }

    /// Repaints one slot.
    ///
    /// ponytail: the shape's own `filled` and `stroke_width` are left at what
    /// they were when it was created, because changing them means reinserting
    /// -- and reinserting would raise the shape to the front, since paint order
    /// is `Document`'s to hand out. The visible cost is that clearing a fill
    /// leaves the interior clickable, and that a stroke widened after the fact
    /// is culled by a box that does not know about it.
    fn restyle(&mut self, slot: usize, style: Style) {
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.style = style;
            self.touched.push(slot);
        }
    }

    /// Starts a fresh record of touched slots.
    fn forget(&mut self) {
        self.touched.clear();
    }
}

// ---------------------------------------------------------------------------
// the undo log
// ---------------------------------------------------------------------------

/// Every reversible edit the editor makes (D-007).
///
/// Each names slots rather than ids, so an entry survives the re-insertion that
/// undoing and redoing an `Add` performs -- see the module docs.
enum Edit {
    /// Objects created. Holds their geometry while they are undone away.
    Add(Vec<(usize, Option<Shape>)>),
    /// Objects deleted. Holds their geometry while they are deleted.
    Remove(Vec<(usize, Option<Shape>)>),
    /// A finished drag: where each slot's transform began and ended.
    Move {
        slots: Vec<usize>,
        before: Vec<Affine>,
        after: Vec<Affine>,
    },
    /// A paint change.
    Restyle {
        slots: Vec<usize>,
        before: Vec<Style>,
        after: Style,
    },
}

/// Moves the held shapes into the document.
fn swap_in(doc: &mut Doc, held: &mut [(usize, Option<Shape>)]) {
    for (slot, shape) in held {
        if let Some(shape) = shape.take() {
            doc.add(*slot, shape);
        }
    }
}

/// The inverse of [`swap_in`]: takes them back out and holds them.
fn swap_out(doc: &mut Doc, held: &mut [(usize, Option<Shape>)]) {
    for (slot, shape) in held {
        *shape = doc.take(*slot);
    }
}

/// Sets each slot's transform, skipping slots whose shape is not live.
fn set_transforms(doc: &mut Doc, slots: &[usize], to: &[Affine]) {
    for (&slot, &t) in slots.iter().zip(to) {
        if let Some(id) = doc.node_of(slot) {
            doc.doc.set_transform(id, t);
            doc.touched.push(slot);
        }
    }
}

impl Command for Edit {
    type Doc = Doc;

    fn apply(&mut self, doc: &mut Doc) {
        match self {
            Self::Add(held) => swap_in(doc, held),
            Self::Remove(held) => swap_out(doc, held),
            Self::Move { slots, after, .. } => set_transforms(doc, slots, after),
            Self::Restyle {
                slots,
                before,
                after,
            } => {
                // Recorded on apply rather than at construction, so that a redo
                // puts back what was there the second time too.
                before.clear();
                for &slot in slots.iter() {
                    before.push(doc.style_at(slot));
                    doc.restyle(slot, *after);
                }
            }
        }
    }

    fn revert(&mut self, doc: &mut Doc) {
        match self {
            Self::Add(held) => swap_out(doc, held),
            Self::Remove(held) => swap_in(doc, held),
            Self::Move { slots, before, .. } => set_transforms(doc, slots, before),
            Self::Restyle { slots, before, .. } => {
                for (&slot, &style) in slots.iter().zip(before.iter()) {
                    doc.restyle(slot, style);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tools and gestures
// ---------------------------------------------------------------------------

/// What a pointer press means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    /// Click to select, drag to move, handles to scale.
    Select,
    /// Drag to turn the selection about its centre.
    Rotate,
    /// Drag out a rectangle.
    Rect,
    /// Drag out an ellipse.
    Ellipse,
    /// Click for corners, drag for curves.
    Pen,
    /// Drag the canvas.
    Pan,
}

impl Tool {
    fn parse(name: &str) -> Self {
        match name {
            "rotate" => Self::Rotate,
            "rect" => Self::Rect,
            "ellipse" => Self::Ellipse,
            "pen" => Self::Pen,
            "pan" => Self::Pan,
            _ => Self::Select,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Rotate => "rotate",
            Self::Rect => "rect",
            Self::Ellipse => "ellipse",
            Self::Pen => "pen",
            Self::Pan => "pan",
        }
    }
}

/// The gesture in progress. The pen is absent on purpose: it owns its own
/// multi-click state and a `Drag` cannot describe it.
enum Drag {
    /// Rubber band from a screen point.
    Marquee { start: Point, mode: SelectMode },
    /// Moving, scaling or turning the selection. `handle` is `None` for a plain
    /// move and [`Handle::Pivot`] for the rotate tool.
    Gesture {
        handle: Option<Handle>,
        start: Point,
        slots: Vec<usize>,
        before: Vec<Affine>,
        moved: bool,
    },
    /// Dragging out a new rectangle or ellipse.
    Create {
        start: Point,
        slot: Option<usize>,
        ellipse: bool,
    },
    /// Panning the view.
    Pan { last: Point },
}

// ---------------------------------------------------------------------------
// the app
// ---------------------------------------------------------------------------

/// One editing session.
struct App {
    doc: Doc,
    view: View,
    selection: Selection,
    log: UndoLog<Edit>,
    tool: Tool,
    /// The style new shapes get, and what the style panel edits.
    style: Style,
    pen: Pen,
    handles: Option<TransformBox>,
    drag: Option<Drag>,
    /// Where the pointer last was, in device pixels. The rubber band needs it
    /// at draw time, and a frame is not drawn from inside the event.
    pointer: Option<Point>,
    /// Canvas size in device pixels.
    size: (u32, u32),
    /// Device pixels per CSS pixel. Every screen constant here is in CSS
    /// pixels; every coordinate that arrives is in device pixels.
    dpr: f64,
    page: Rect,
}

impl App {
    fn new(width: u32, height: u32, dpr: f64) -> Self {
        let mut app = Self {
            doc: Doc::new(),
            view: View::new(),
            selection: Selection::new(),
            log: UndoLog::new(UNDO_DEPTH),
            tool: Tool::Select,
            style: Style::default(),
            pen: Pen::new(),
            handles: None,
            drag: None,
            pointer: None,
            size: (width.max(1), height.max(1)),
            dpr: if dpr > 0.0 { dpr } else { 1.0 },
            page: PAGE,
        };
        app.zoom_to_fit();
        app
    }

    fn resize(&mut self, width: u32, height: u32, dpr: f64) {
        let old = self.screen().center();
        self.size = (width.max(1), height.max(1));
        self.dpr = if dpr > 0.0 { dpr } else { 1.0 };
        // Keep whatever was in the middle of the window in the middle of it,
        // so a resize does not slide the document out of view.
        let doc_center = self.view.to_document(old);
        self.view.anchor_at(doc_center, self.screen().center());
    }

    fn screen(&self) -> Rect {
        Rect::new(0.0, 0.0, f64::from(self.size.0), f64::from(self.size.1))
    }

    /// Device pixels for a length given in CSS pixels.
    fn px(&self, css: f64) -> f64 {
        css * self.dpr
    }

    fn zoom_to_fit(&mut self) {
        let screen = self.screen();
        let (pw, ph) = (self.page.width(), self.page.height());
        if pw <= 0.0 || ph <= 0.0 {
            return;
        }
        // 0.9 leaves a margin, so the page reads as a page rather than as the
        // window's background.
        let target = (screen.width() / pw).min(screen.height() / ph) * 0.9;
        self.view = View::new();
        self.view.zoom_about(screen.center(), target);
        self.view.anchor_at(self.page.center(), screen.center());
    }

    fn track_handles(&mut self) {
        self.handles = TransformBox::new(&self.doc.doc, &self.selection);
    }

    /// The slots of the current selection, with their transforms.
    fn selected(&self) -> (Vec<usize>, Vec<Affine>) {
        let mut slots = Vec::new();
        let mut transforms = Vec::new();
        for id in self.selection.iter() {
            if let (Some(slot), Some(shape)) = (self.doc.slot_of(id), self.doc.doc.get(id)) {
                slots.push(slot);
                transforms.push(shape.transform);
            }
        }
        (slots, transforms)
    }

    /// Reselects by slot.
    ///
    /// Undo and redo reinsert shapes under fresh ids, so a selection held as
    /// ids does not survive them; held as slots it does.
    fn select_slots(&mut self, slots: &[usize]) {
        let ids: Vec<NodeId> = slots.iter().filter_map(|&s| self.doc.node_of(s)).collect();
        self.selection.apply(SelectMode::Replace, &ids);
        self.track_handles();
    }

    // -- input -------------------------------------------------------------

    fn press(&mut self, p: Point, shift: bool, alt: bool, middle: bool) {
        self.pointer = Some(p);
        if middle || self.tool == Tool::Pan {
            self.drag = Some(Drag::Pan { last: p });
            return;
        }
        match self.tool {
            Tool::Pen => {
                if let Some(path) = self.pen.press(&self.view, p, self.px(CLOSE_PX)) {
                    self.commit_path(path);
                }
            }
            Tool::Rect | Tool::Ellipse => {
                self.drag = Some(Drag::Create {
                    start: p,
                    slot: None,
                    ellipse: self.tool == Tool::Ellipse,
                });
            }
            Tool::Rotate => {
                let (slots, before) = self.selected();
                if !slots.is_empty() {
                    self.drag = Some(Drag::Gesture {
                        handle: Some(Handle::Pivot),
                        start: p,
                        slots,
                        before,
                        moved: false,
                    });
                }
            }
            Tool::Select | Tool::Pan => self.press_select(p, shift, alt),
        }
    }

    fn press_select(&mut self, p: Point, shift: bool, _alt: bool) {
        // A handle wins over whatever is under it: it is drawn on top, and is
        // the only thing at that pixel the user can mean.
        // The pivot is not filtered out for nothing: it sits at the centre of
        // the box, which is exactly where a user grabs a shape to move it. The
        // rotate tool is what turns things here, so the pivot is not a handle
        // this tool offers.
        let handle = self
            .handles
            .and_then(|b| b.handle_at(&self.view, p, self.px(HANDLE_PX)))
            .filter(|h| *h != Handle::Pivot);
        if handle.is_some() {
            let (slots, before) = self.selected();
            self.drag = Some(Drag::Gesture {
                handle,
                start: p,
                slots,
                before,
                moved: false,
            });
            return;
        }
        let mode = if shift {
            SelectMode::Add
        } else {
            SelectMode::Replace
        };
        match hit_test(&self.doc.doc, &self.view, p, self.px(HIT_PX)) {
            Some(id) => {
                // Clicking one of several selected shapes drags the whole
                // selection; that is what every editor does, and re-selecting
                // on press would make a multi-shape drag impossible.
                if !self.selection.contains(id) {
                    self.selection.apply(mode, &[id]);
                    self.track_handles();
                }
                let (slots, before) = self.selected();
                self.drag = Some(Drag::Gesture {
                    handle: None,
                    start: p,
                    slots,
                    before,
                    moved: false,
                });
            }
            None => self.drag = Some(Drag::Marquee { start: p, mode }),
        }
    }

    fn moved(&mut self, p: Point, shift: bool, alt: bool) {
        self.pointer = Some(p);
        match self.drag.take() {
            Some(Drag::Pan { last }) => {
                self.view.pan_by(p - last);
                self.drag = Some(Drag::Pan { last: p });
            }
            Some(Drag::Marquee { start, mode }) => {
                self.drag = Some(Drag::Marquee { start, mode });
                if start.distance(p) >= self.px(DRAG_PX) {
                    let hits = marquee(&self.doc.doc, &self.view, start, p, MarqueeMode::Intersect);
                    self.selection.apply(mode, &hits);
                }
            }
            Some(Drag::Create {
                start,
                slot,
                ellipse,
            }) => {
                let slot = self.grow(start, p, slot, ellipse, shift);
                self.drag = Some(Drag::Create {
                    start,
                    slot,
                    ellipse,
                });
            }
            Some(Drag::Gesture {
                handle,
                start,
                slots,
                before,
                moved,
            }) => {
                let moved = moved || start.distance(p) >= self.px(DRAG_PX);
                if moved {
                    self.apply_gesture(handle, start, p, &slots, &before, shift, alt);
                }
                self.drag = Some(Drag::Gesture {
                    handle,
                    start,
                    slots,
                    before,
                    moved,
                });
            }
            // No drag: the pen is the only tool that tracks a bare move, and
            // `drag` is a no-op unless its button is actually down.
            None => {
                self.pen.drag(&self.view, p, alt);
                self.pen.hover(&self.view, p);
            }
        }
    }

    fn release(&mut self, p: Point) {
        self.pointer = Some(p);
        match self.drag.take() {
            Some(Drag::Marquee { start, mode }) => {
                // A click on empty canvas deselects. A band one pixel wide is
                // that click, not a selection of nothing.
                if start.distance(p) < self.px(DRAG_PX) && mode == SelectMode::Replace {
                    self.selection.clear();
                }
                self.track_handles();
            }
            Some(Drag::Create { slot, .. }) => {
                if let Some(slot) = slot {
                    // Taken back out and reinserted through the log, so the one
                    // entry that exists is the one that owns the shape.
                    let shape = self.doc.take(slot);
                    self.log.edit(&mut self.doc, Edit::Add(vec![(slot, shape)]));
                    self.select_slots(&[slot]);
                }
            }
            Some(Drag::Gesture {
                slots,
                before,
                moved,
                ..
            }) => {
                if moved {
                    let after = slots
                        .iter()
                        .map(|&slot| {
                            self.doc
                                .node_of(slot)
                                .and_then(|id| self.doc.doc.get(id))
                                .map_or(Affine::IDENTITY, |s| s.transform)
                        })
                        .collect();
                    self.log.edit(
                        &mut self.doc,
                        Edit::Move {
                            slots,
                            before,
                            after,
                        },
                    );
                }
                self.track_handles();
            }
            Some(Drag::Pan { .. }) | None => {}
        }
        self.pen.release();
        // The caller knows where a gesture ends, and this is it: one pointer
        // stroke is one undo step.
        self.log.seal();
    }

    /// Updates the shape being dragged out, creating it on the first real move.
    fn grow(
        &mut self,
        start: Point,
        p: Point,
        slot: Option<usize>,
        ellipse: bool,
        square: bool,
    ) -> Option<usize> {
        if start.distance(p) < self.px(DRAG_PX) {
            return slot;
        }
        let a = self.view.to_document(start);
        let mut b = self.view.to_document(p);
        if square {
            let side = (b.x - a.x).abs().max((b.y - a.y).abs());
            b = Point::new(
                a.x + side.copysign(b.x - a.x),
                a.y + side.copysign(b.y - a.y),
            );
        }
        let rect = Rect::from_points(a, b);
        let path = if ellipse {
            ellipse_path(rect)
        } else {
            Path::from(rect_els(rect))
        };
        match slot {
            Some(slot) => {
                if let Some(id) = self.doc.node_of(slot) {
                    self.doc.doc.set_path(id, path);
                }
                Some(slot)
            }
            None => {
                let name = if ellipse { "Ellipse" } else { "Rectangle" };
                Some(self.doc.insert(Shape::new(path), self.style, name))
            }
        }
    }

    #[expect(clippy::too_many_arguments, reason = "a gesture is its modifiers")]
    fn apply_gesture(
        &mut self,
        handle: Option<Handle>,
        start: Point,
        p: Point,
        slots: &[usize],
        before: &[Affine],
        shift: bool,
        alt: bool,
    ) {
        let Some(mut boxed) = self.handles else {
            return;
        };
        let (from, to) = (self.view.to_document(start), self.view.to_document(p));
        let gesture = match handle {
            None => Affine::translate(to - from),
            // The rotate tool turns about the box centre, where a fresh
            // `TransformBox` puts its pivot.
            Some(Handle::Pivot) => boxed.rotate(from, to),
            Some(h) => boxed.scale(h, to, alt, shift),
        };
        if !gesture.is_finite() {
            return;
        }
        for (&slot, &start_transform) in slots.iter().zip(before) {
            if let Some(id) = self.doc.node_of(slot) {
                self.doc.doc.set_transform(id, gesture * start_transform);
            }
        }
        // The box follows the shapes -- except during a rotation, where the
        // bounds it would track are the *axis-aligned* box of a turning shape,
        // which grows and shrinks and would make the gesture chase itself.
        if handle != Some(Handle::Pivot) {
            boxed.track(&self.doc.doc, &self.selection);
            self.handles = Some(boxed);
        }
    }

    fn wheel(&mut self, p: Point, dx: f64, dy: f64, zoom: bool) {
        if zoom {
            // `exp` gives equal notches equal ratios, so zooming in and back
            // out returns to exactly where it started.
            self.view.zoom_about(p, (-dy * 0.002).exp());
        } else {
            self.view.pan_by(Vec2::new(-dx, -dy));
        }
    }

    // -- actions -----------------------------------------------------------

    fn action(&mut self, name: &str) -> bool {
        match name {
            "undo" => {
                self.doc.forget();
                let done = self.log.undo(&mut self.doc);
                self.after_history();
                done
            }
            "redo" => {
                self.doc.forget();
                let done = self.log.redo(&mut self.doc);
                self.after_history();
                done
            }
            "delete" => self.delete_selection(),
            "select-all" => {
                let ids = self.doc.doc.z_order();
                self.selection.apply(SelectMode::Replace, &ids);
                self.track_handles();
                true
            }
            "escape" => {
                if self.pen.is_drawing() {
                    self.pen.finish();
                } else {
                    self.selection.clear();
                    self.handles = None;
                }
                true
            }
            "finish" => match self.pen.finish() {
                Some(path) => {
                    self.commit_path(path);
                    true
                }
                None => false,
            },
            "zoom-fit" => {
                self.zoom_to_fit();
                true
            }
            "zoom-in" => {
                self.view.zoom_about(self.screen().center(), 1.25);
                true
            }
            "zoom-out" => {
                self.view.zoom_about(self.screen().center(), 0.8);
                true
            }
            "new" => {
                let (size, dpr) = (self.size, self.dpr);
                *self = Self::new(size.0, size.1, dpr);
                true
            }
            _ => false,
        }
    }

    /// Selects whatever the step just walked over, which is the shape the user
    /// is watching. A step that removed its shapes leaves nothing selected.
    fn after_history(&mut self) {
        self.selection.retain_live(&self.doc.doc);
        let touched = std::mem::take(&mut self.doc.touched);
        self.select_slots(&touched);
    }

    fn delete_selection(&mut self) -> bool {
        let (slots, _) = self.selected();
        if slots.is_empty() {
            return false;
        }
        let held = slots.into_iter().map(|slot| (slot, None)).collect();
        self.log.edit(&mut self.doc, Edit::Remove(held));
        self.log.seal();
        self.selection.clear();
        self.handles = None;
        true
    }

    /// Inserts a finished pen path as one undo step, and selects it.
    fn commit_path(&mut self, path: Path) {
        let slot = self.doc.insert(Shape::new(path), self.style, "Path");
        let shape = self.doc.take(slot);
        self.log.edit(&mut self.doc, Edit::Add(vec![(slot, shape)]));
        self.log.seal();
        self.select_slots(&[slot]);
    }

    /// Applies a style to the selection -- or, with nothing selected, makes it
    /// what the next shape drawn will use.
    fn set_style(&mut self, style: Style) {
        self.style = style;
        let (slots, _) = self.selected();
        if slots.is_empty() {
            return;
        }
        self.log.edit(
            &mut self.doc,
            Edit::Restyle {
                slots,
                before: Vec::new(),
                after: style,
            },
        );
        self.log.seal();
    }

    // -- rendering ---------------------------------------------------------

    /// One frame, as the scene both rasterizers read (D-002).
    fn scene(&self) -> Scene {
        let mut scene = Scene::new(self.size.0, self.size.1);
        let m = self.view.matrix();
        let screen = self.screen();
        scene.fill(rect_els(self.page.transform(m)), PAPER);

        for id in self.doc.doc.z_order() {
            let Some(shape) = self.doc.doc.get(id) else {
                continue;
            };
            // ponytail: linear over the document, so 100k objects walk 100k
            // boxes per frame. The quadtree query that replaces it is
            // `Document::query`, which is `pub(crate)` today; P3's tile cache
            // is the real answer.
            if !shape.bounds().transform(m).overlaps(screen) {
                continue;
            }
            let full = m * shape.transform;
            let style = self.doc.style_of(id);
            let path = shape.path.transform(full);
            if let Some(color) = style.fill {
                scene.fill(path.elements().to_vec(), color);
            }
            if let Some(color) = style.stroke {
                // ponytail: `max_scale` is exact for a rotation or a uniform
                // scale and overstates a skewed one, where SVG draws with an
                // elliptical pen. Stroking in shape space and transforming the
                // outline is the fix, at one expansion per frame.
                let outline = path.stroke(&pen_style(style.width * full.max_scale()));
                scene.fill(outline.elements().to_vec(), color);
            }
        }
        self.overlay(&mut scene, m);
        scene
    }

    /// Selection box, handles, rubber band and pen preview.
    ///
    /// Drawn into the scene rather than into DOM elements over the canvas: it
    /// is geometry, in the same space as everything else, and the renderer is
    /// right here. The panels stay DOM, which is what D-005 is about.
    fn overlay(&self, scene: &mut Scene, m: Affine) {
        let hair = pen_style(self.px(1.0));
        if let Some(boxed) = self.handles {
            let outline = boxed.bounds.transform(m);
            scene.fill(
                Path::from(rect_els(outline))
                    .stroke(&hair)
                    .elements()
                    .to_vec(),
                CHROME,
            );
            let size = self.px(HANDLE_PX);
            for corner in handle_points(outline) {
                let square = Rect::new(corner.x, corner.y, corner.x, corner.y).inflate(size * 0.5);
                scene.fill(rect_els(square), CHROME);
                scene.fill(rect_els(square.inflate(-self.px(2.0))), PAPER);
            }
        }
        if let (Some(Drag::Marquee { start, .. }), Some(now)) = (&self.drag, self.pointer) {
            // Only once it is a band: a press that has not moved would draw a
            // degenerate rectangle over the pixel under the cursor.
            if start.distance(now) >= self.px(DRAG_PX) {
                let band = Rect::from_points(*start, now);
                scene.fill(rect_els(band), Color { a: 40, ..CHROME });
                scene.fill(
                    Path::from(rect_els(band)).stroke(&hair).elements().to_vec(),
                    CHROME,
                );
            }
        }
        if self.pen.is_drawing() {
            let preview = self.pen.preview().transform(m);
            scene.fill(preview.stroke(&hair).elements().to_vec(), CHROME);
            for node in self.pen.nodes() {
                let p = m * node.point;
                scene.fill(
                    rect_els(Rect::new(p.x, p.y, p.x, p.y).inflate(self.px(3.0))),
                    CHROME,
                );
            }
        }
    }

    // -- the view-model ----------------------------------------------------

    /// The JSON the panels are built from.
    ///
    /// The tool, the zoom, the undo flags, the style, and one row per object.
    ///
    /// ponytail: every row, topmost first, so a 100k-object document sends 100k
    /// of them. D-004 wants the shell to ask for the ~40 rows it can show,
    /// which needs the panel to virtualise first.
    fn state_json(&self) -> String {
        let style = self.selection_style();
        let mut out = format!(
            "{{\"tool\":\"{}\",\"zoom\":{},\"shapes\":{},\"selected\":{},\
             \"canUndo\":{},\"canRedo\":{},\"drawing\":{},\
             \"fill\":\"{}\",\"fillOn\":{},\"stroke\":\"{}\",\"strokeOn\":{},\"width\":{},\
             \"layers\":[",
            self.tool.name(),
            json_num(self.view.zoom()),
            self.doc.doc.len(),
            self.selection.len(),
            self.log.can_undo(),
            self.log.can_redo(),
            self.pen.is_drawing(),
            hex(style.fill.unwrap_or(BLACK)),
            style.fill.is_some(),
            hex(style.stroke.unwrap_or(BLACK)),
            style.stroke.is_some(),
            json_num(style.width),
        );
        let mut first = true;
        for id in self.doc.doc.z_order().into_iter().rev() {
            let Some(slot) = self.doc.slot_of(id) else {
                continue;
            };
            if !first {
                out.push(',');
            }
            first = false;
            let name = self
                .doc
                .slots
                .get(slot)
                .map_or("Shape", |s| s.name.as_str());
            out.push_str(&format!(
                "{{\"slot\":{},\"name\":\"{}\",\"selected\":{}}}",
                slot,
                escape(name),
                self.selection.contains(id)
            ));
        }
        out.push_str("]}");
        out
    }

    /// The style the panel shows: the selection's when it agrees, and otherwise
    /// the one the next shape will get.
    fn selection_style(&self) -> Style {
        let mut found: Option<Style> = None;
        for id in self.selection.iter() {
            let style = self.doc.style_of(id);
            match found {
                None => found = Some(style),
                Some(first) if first == style => {}
                Some(_) => return self.style,
            }
        }
        found.unwrap_or(self.style)
    }

    /// Selects one object by slot: what the layer panel clicks.
    fn select_slot(&mut self, slot: usize, additive: bool) {
        let Some(id) = self.doc.node_of(slot) else {
            return;
        };
        let mode = if additive {
            SelectMode::Toggle
        } else {
            SelectMode::Replace
        };
        self.selection.apply(mode, &[id]);
        self.track_handles();
    }

    // -- SVG ---------------------------------------------------------------

    /// The document as an SVG 1.1 file.
    fn to_svg(&self) -> String {
        let mut nodes = Arena::new();
        let mut roots = Vec::new();
        for id in self.doc.doc.z_order() {
            let Some(shape) = self.doc.doc.get(id) else {
                continue;
            };
            roots.push(nodes.insert(SvgNode {
                kind: NodeKind::Path(shape.path.clone()),
                transform: shape.transform,
                style: svg_style(self.doc.style_of(id)),
                clip_path: None,
            }));
        }
        hane_svg::export::write(&SvgScene {
            nodes,
            roots,
            gradients: Vec::new(),
            clips: Vec::new(),
            view_box: Some(self.page),
            width: Some(json_num(self.page.width())),
            height: Some(json_num(self.page.height())),
            losses: Vec::new(),
        })
    }

    /// Replaces the document with an imported SVG, reporting what was lost.
    fn open_svg(&mut self, source: &str) -> String {
        let scene = match hane_svg::import::import(source) {
            Ok(scene) => scene,
            Err(e) => return format!("could not read that file: {e}"),
        };
        let (size, dpr) = (self.size, self.dpr);
        *self = Self::new(size.0, size.1, dpr);
        if let Some(view_box) = scene.view_box {
            self.page = view_box;
        } else if let (Some(w), Some(h)) = (
            scene.width.as_deref().and_then(parse_len),
            scene.height.as_deref().and_then(parse_len),
        ) {
            self.page = Rect::new(0.0, 0.0, w, h);
        }
        let mut leaves = Vec::new();
        for &root in &scene.roots {
            collect(&scene, root, Affine::IDENTITY, &mut leaves);
        }
        for (path, transform, style) in leaves {
            let slot = self.doc.insert(Shape::new(path), style, "Path");
            if let Some(id) = self.doc.node_of(slot) {
                self.doc.doc.set_transform(id, transform);
            }
        }
        self.zoom_to_fit();
        let mut report = String::new();
        if !scene.losses.is_empty() {
            report = format!(
                "opened, with {} thing(s) this build cannot represent yet: {}",
                scene.losses.len(),
                scene
                    .losses
                    .iter()
                    .take(3)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        report
    }
}

/// Every leaf path of an imported scene, with its accumulated transform and the
/// paint this editor can hold.
fn collect(scene: &SvgScene, id: NodeId, parent: Affine, out: &mut Vec<(Path, Affine, Style)>) {
    let Some(node) = scene.nodes.get(id) else {
        return;
    };
    // The parent's transform applies last, so it goes on the left.
    let ctm = parent * node.transform;
    match &node.kind {
        NodeKind::Group(children) => {
            for &child in children {
                collect(scene, child, ctm, out);
            }
        }
        NodeKind::Path(path) => {
            let get = |name: &str| node.style.get(name).unwrap_or("");
            let opacity = |value: &str| parse_len(value).unwrap_or(1.0).clamp(0.0, 1.0);
            let with_alpha = |c: Option<Color>, a: f64| {
                c.map(|c| Color {
                    a: (f64::from(c.a) * a).round() as u8,
                    ..c
                })
            };
            let style = Style {
                fill: with_alpha(parse_color(get("fill")), opacity(get("fill-opacity"))),
                stroke: with_alpha(parse_color(get("stroke")), opacity(get("stroke-opacity"))),
                width: parse_len(get("stroke-width")).unwrap_or(1.0),
            };
            out.push((path.clone(), ctm, style));
        }
    }
}

/// A style as the presentation attributes SVG export writes back out.
fn svg_style(style: Style) -> hane_svg::style::Style {
    let attr = |local: &str, value: String| Attribute {
        name: Name {
            namespace: None,
            local: local.to_string(),
        },
        value,
    };
    let alpha = |c: Color| json_num(f64::from(c.a) / 255.0);
    let mut attributes = vec![
        attr("fill", style.fill.map_or_else(|| "none".into(), hex)),
        attr("stroke", style.stroke.map_or_else(|| "none".into(), hex)),
    ];
    if let Some(c) = style.fill {
        attributes.push(attr("fill-opacity", alpha(c)));
    }
    if let Some(c) = style.stroke {
        attributes.push(attr("stroke-opacity", alpha(c)));
        attributes.push(attr("stroke-width", json_num(style.width)));
    }
    hane_svg::style::Style::initial().resolve(&Element {
        name: Name {
            namespace: None,
            local: "path".to_string(),
        },
        attributes,
        children: Vec::new(),
        line: 0,
        column: 0,
    })
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

/// A number JSON and SVG both accept: never `NaN`, never `inf`, never `1e300`
/// in a place that wants a length.
fn json_num(v: f64) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".into()
    }
}

/// JSON string escaping, for the one field that carries user text.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `r` as a closed rectangle.
fn rect_els(r: Rect) -> Vec<PathEl> {
    vec![
        PathEl::MoveTo(Point::new(r.x0, r.y0)),
        PathEl::LineTo(Point::new(r.x1, r.y0)),
        PathEl::LineTo(Point::new(r.x1, r.y1)),
        PathEl::LineTo(Point::new(r.x0, r.y1)),
        PathEl::ClosePath,
    ]
}

/// The eight scale handles of a box, where `TransformBox` puts them.
fn handle_points(r: Rect) -> [Point; 8] {
    let c = r.center();
    [
        Point::new(r.x0, r.y0),
        Point::new(c.x, r.y0),
        Point::new(r.x1, r.y0),
        Point::new(r.x1, c.y),
        Point::new(r.x1, r.y1),
        Point::new(c.x, r.y1),
        Point::new(r.x0, r.y1),
        Point::new(r.x0, c.y),
    ]
}

/// The ellipse inscribed in `r`, as four cubics.
fn ellipse_path(r: Rect) -> Path {
    let c = r.center();
    let (rx, ry) = (r.width() * 0.5, r.height() * 0.5);
    let (kx, ky) = (rx * KAPPA, ry * KAPPA);
    let p = |x: f64, y: f64| Point::new(c.x + x, c.y + y);
    Path::from(vec![
        PathEl::MoveTo(p(rx, 0.0)),
        PathEl::CurveTo(p(rx, ky), p(kx, ry), p(0.0, ry)),
        PathEl::CurveTo(p(-kx, ry), p(-rx, ky), p(-rx, 0.0)),
        PathEl::CurveTo(p(-rx, -ky), p(-kx, -ry), p(0.0, -ry)),
        PathEl::CurveTo(p(kx, -ry), p(rx, -ky), p(rx, 0.0)),
        PathEl::ClosePath,
    ])
}

/// A stroke `width` units wide, flattened well inside a pixel.
fn pen_style(width: f64) -> StrokeStyle {
    StrokeStyle {
        width,
        tolerance: 0.05,
        ..StrokeStyle::default()
    }
}

// ---------------------------------------------------------------------------
// the exports
// ---------------------------------------------------------------------------

thread_local! {
    /// The one session. A tab edits one document.
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// The renderer, kept across frames: its three shader programs cost more to
    /// compile than a frame costs to draw.
    static GPU: RefCell<Option<Gpu>> = const { RefCell::new(None) };
}

fn with<R: Default>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with_borrow_mut(|slot| slot.as_mut().map_or_else(R::default, f))
}

/// Starts a session on a `width` by `height` device-pixel canvas.
///
/// `dpr` is `devicePixelRatio`: the shell sends device pixels, and this is what
/// turns a handle's size in CSS pixels into the same size on screen.
#[wasm_bindgen]
pub fn hane_app_init(width: u32, height: u32, dpr: f64) {
    APP.with_borrow_mut(|slot| *slot = Some(App::new(width, height, dpr)));
}

/// Tells the session the canvas changed size.
#[wasm_bindgen]
pub fn hane_app_resize(width: u32, height: u32, dpr: f64) {
    with(|app| app.resize(width, height, dpr));
}

/// Selects a tool: `select`, `rotate`, `rect`, `ellipse`, `pen` or `pan`.
#[wasm_bindgen]
pub fn hane_app_tool(name: &str) {
    with(|app| {
        // Switching away from a half-drawn path finishes it rather than losing
        // it -- the nodes are already placed, and dropping them silently is the
        // one thing a user cannot undo.
        if app.pen.is_drawing()
            && Tool::parse(name) != Tool::Pen
            && let Some(path) = app.pen.finish()
        {
            app.commit_path(path);
        }
        app.tool = Tool::parse(name);
        app.drag = None;
    });
}

/// One pointer event. `phase` is `down`, `move` or `up`; the coordinates are
/// device pixels relative to the canvas.
#[wasm_bindgen]
pub fn hane_app_pointer(phase: &str, x: f64, y: f64, shift: bool, alt: bool, middle: bool) {
    with(|app| {
        let p = Point::new(x, y);
        match phase {
            "down" => app.press(p, shift, alt, middle),
            "move" => app.moved(p, shift, alt),
            "up" => app.release(p),
            _ => {}
        }
    });
}

/// One wheel event. `zoom` distinguishes a zoom gesture (ctrl, or a pinch) from
/// a two-axis scroll.
#[wasm_bindgen]
pub fn hane_app_wheel(x: f64, y: f64, dx: f64, dy: f64, zoom: bool) {
    with(|app| app.wheel(Point::new(x, y), dx, dy, zoom));
}

/// Runs a named action, returning whether it changed anything.
///
/// `undo`, `redo`, `delete`, `select-all`, `escape`, `finish`, `zoom-fit`,
/// `zoom-in`, `zoom-out`, `new`.
#[wasm_bindgen]
pub fn hane_app_action(name: &str) -> bool {
    with(|app| app.action(name))
}

/// Selects the object in a layer row. `additive` toggles instead of replacing.
#[wasm_bindgen]
pub fn hane_app_select(slot: usize, additive: bool) {
    with(|app| app.select_slot(slot, additive));
}

/// Sets the paint of the selection, or of the next shape when nothing is
/// selected. An empty colour string means no fill or no stroke.
#[wasm_bindgen]
pub fn hane_app_set_style(fill: &str, stroke: &str, width: f64) {
    with(|app| {
        app.set_style(Style {
            fill: parse_color(fill),
            stroke: parse_color(stroke),
            width: if width.is_finite() && width > 0.0 {
                width
            } else {
                1.0
            },
        });
    });
}

/// The view-model the panels render from, as JSON.
#[wasm_bindgen]
pub fn hane_app_state() -> String {
    with(|app| app.state_json())
}

/// The document as an SVG 1.1 file.
#[wasm_bindgen]
pub fn hane_app_svg() -> String {
    with(|app| app.to_svg())
}

/// Replaces the document with an imported SVG.
///
/// Returns an empty string when nothing was lost, and otherwise a line for the
/// user: a file that half-opened must say so before they save over it.
#[wasm_bindgen]
pub fn hane_app_open_svg(source: &str) -> String {
    with(|app| app.open_svg(source))
}

/// Draws one frame onto `canvas`.
///
/// The renderer is built once and reused; only the geometry is re-uploaded.
/// A canvas that changed size gets a new one, since the targets are allocated
/// at one size.
#[wasm_bindgen]
pub fn hane_app_render(canvas: &HtmlCanvasElement) -> Result<(), JsValue> {
    let scene = with(|app| Some(app.scene())).ok_or_else(|| JsValue::from_str("no session"))?;
    let gl = glctx::context(canvas)?;
    if gl.is_context_lost() {
        // Every GL object the cached renderer holds died with the context.
        // Dropping it here is what makes the frame after the restore rebuild
        // them, rather than draw with handles that name nothing.
        GPU.with_borrow_mut(|slot| *slot = None);
        return Err(JsValue::from_str("the WebGL context was lost"));
    }
    // The clip mask is a float target; without it nothing here can draw at all,
    // and the failure is worth naming rather than showing a blank artboard.
    if gl.get_extension("EXT_color_buffer_float")?.is_none() {
        return Err(JsValue::from_str(
            "EXT_color_buffer_float is required for the clip mask",
        ));
    }
    // ponytail: the whole scene is re-flattened and re-binned every frame. That
    // is P3's tile cache missing, not the renderer being slow -- and it is why
    // this is honest about not being the 100k-object path yet.
    let data = DrawData::build(&scene);
    let (w, h) = (data.width as i32, data.height as i32);
    let levels = group_depth(&data)? + 1;
    GPU.with_borrow_mut(|slot| {
        if !slot.as_ref().is_some_and(|gpu| gpu.fits(w, h, levels)) {
            *slot = Some(Gpu::new(&gl, w, h, levels)?);
        }
        let gpu = slot.as_ref().ok_or_else(|| JsValue::from_str("no gpu"))?;
        gpu.upload(&data)?;
        gpu.run(&data)?;
        gpu.present();
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with a canvas big enough that the page is a sane size on it.
    fn app() -> App {
        App::new(800, 600, 1.0)
    }

    /// Drags a rectangle out between two screen points.
    fn drag_rect(app: &mut App, a: Point, b: Point) {
        app.tool = Tool::Rect;
        app.press(a, false, false, false);
        app.moved(b, false, false);
        app.release(b);
    }

    #[test]
    fn drawing_a_rectangle_is_one_undo_step() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        assert_eq!(app.doc.doc.len(), 1);
        assert_eq!(app.selection.len(), 1, "a new shape is selected");

        assert!(app.action("undo"));
        assert_eq!(app.doc.doc.len(), 0);
        assert!(app.action("redo"));
        assert_eq!(app.doc.doc.len(), 1);
    }

    /// The reason slots exist: a redone insert lands under a *new* `NodeId`,
    /// and the move that follows it in the log must still find the shape.
    #[test]
    fn redo_replays_a_move_onto_the_reinserted_shape() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        app.tool = Tool::Select;
        let inside = Point::new(150.0, 140.0);
        app.press(inside, false, false, false);
        app.moved(inside + Vec2::new(60.0, 0.0), false, false);
        app.release(inside + Vec2::new(60.0, 0.0));

        let moved = app.selected().1[0];
        assert!(moved.translation().x > 0.0, "the drag moved it");

        assert!(app.action("undo"), "undo the move");
        assert!(app.action("undo"), "undo the draw");
        assert_eq!(app.doc.doc.len(), 0);
        assert!(app.action("redo"), "redo the draw");
        assert!(app.action("redo"), "redo the move");
        assert_eq!(app.doc.doc.len(), 1);
        let again = app
            .doc
            .node_of(0)
            .and_then(|id| app.doc.doc.get(id))
            .map(|s| s.transform)
            .expect("the shape is back");
        assert_eq!(again.as_coeffs(), moved.as_coeffs(), "the move replayed");
    }

    #[test]
    fn deleting_and_undoing_restores_the_shape() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        assert!(app.action("delete"));
        assert_eq!(app.doc.doc.len(), 0);
        assert!(app.action("undo"));
        assert_eq!(app.doc.doc.len(), 1);
        assert_eq!(app.selection.len(), 1, "and is selected again, by slot");
    }

    #[test]
    fn a_click_on_empty_canvas_deselects() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        app.tool = Tool::Select;
        let empty = Point::new(600.0, 500.0);
        app.press(empty, false, false, false);
        app.release(empty);
        assert_eq!(app.selection.len(), 0);
        assert!(app.handles.is_none());
    }

    #[test]
    fn restyling_the_selection_undoes() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        let before = app.doc.style_at(0);
        app.set_style(Style {
            fill: None,
            stroke: Some(BLACK),
            width: 3.0,
        });
        assert_eq!(app.doc.style_at(0).stroke, Some(BLACK));
        assert!(app.action("undo"));
        assert_eq!(app.doc.style_at(0), before);
    }

    /// Export and import are each other's inverse for what this editor holds:
    /// the geometry, the two colours and the width.
    #[test]
    fn svg_round_trips_geometry_and_paint() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        app.set_style(Style {
            fill: Some(Color {
                r: 18,
                g: 52,
                b: 86,
                a: 255,
            }),
            stroke: Some(BLACK),
            width: 2.5,
        });
        // The path's own box, not `Shape::bounds`: that one includes the
        // stroke, and this document's stroke was added after the fact -- see
        // the ponytail note on `Doc::restyle`.
        let before = app
            .doc
            .node_of(0)
            .and_then(|id| app.doc.doc.get(id))
            .map(|s| s.path.bounding_box().transform(s.transform))
            .expect("drawn");
        let svg = app.to_svg();
        assert!(svg.contains("#123456"), "{svg}");

        let report = app.open_svg(&svg);
        assert_eq!(report, "", "a file we wrote must open cleanly");
        assert_eq!(app.doc.doc.len(), 1);
        let style = app.doc.style_at(0);
        assert_eq!(style.fill.map(hex).as_deref(), Some("#123456"));
        assert_eq!(style.stroke, Some(BLACK));
        assert!((style.width - 2.5).abs() < 1e-12);
        let after = app
            .doc
            .node_of(0)
            .and_then(|id| app.doc.doc.get(id))
            .map(|s| s.path.bounding_box().transform(s.transform))
            .expect("imported");
        // The path is written at full `f64` precision, so this is exact and not
        // a tolerance.
        assert_eq!(
            (after.x0, after.y0, after.x1, after.y1),
            (before.x0, before.y0, before.x1, before.y1)
        );
    }

    #[test]
    fn colours_parse_the_way_a_browser_reads_them() {
        assert_eq!(parse_color("none"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#f00").map(hex).as_deref(), Some("#ff0000"));
        assert_eq!(parse_color("#123456").map(hex).as_deref(), Some("#123456"));
        assert_eq!(parse_color("#0000ff80").map(|c| c.a), Some(0x80));
        assert_eq!(parse_color("RED").map(hex).as_deref(), Some("#ff0000"));
        // Unreadable is black, as in a browser -- never invisible, which would
        // look like the shape failed to import.
        assert_eq!(parse_color("url(#grad)"), Some(BLACK));
    }

    /// The pen: three clicks and a close makes one closed shape, in one step.
    #[test]
    fn the_pen_closes_a_path_into_one_shape() {
        let mut app = app();
        app.tool = Tool::Pen;
        let first = Point::new(200.0, 200.0);
        for p in [first, Point::new(300.0, 200.0), Point::new(300.0, 300.0)] {
            app.press(p, false, false, false);
            app.release(p);
        }
        assert!(app.pen.is_drawing());
        app.press(first, false, false, false);
        app.release(first);
        assert!(!app.pen.is_drawing(), "the click on node 0 closed it");
        assert_eq!(app.doc.doc.len(), 1);
        assert!(app.action("undo"));
        assert_eq!(app.doc.doc.len(), 0);
    }

    /// The view-model is the contract the shell is written against.
    #[test]
    fn the_state_json_reports_what_the_panels_need() {
        let mut app = app();
        drag_rect(&mut app, Point::new(100.0, 100.0), Point::new(200.0, 180.0));
        let json = app.state_json();
        assert!(json.starts_with("{\"tool\":\"rect\""), "{json}");
        assert!(json.contains("\"shapes\":1"), "{json}");
        assert!(json.contains("\"canUndo\":true"), "{json}");
        assert!(json.contains("\"name\":\"Rectangle\""), "{json}");
        assert!(json.contains("\"selected\":1"), "{json}");
    }

    /// Zoom is a ratio, so equal notches in and out land back where they began.
    #[test]
    fn zooming_in_and_out_returns_to_the_same_view() {
        let mut app = app();
        let before = app.view.zoom();
        let at = Point::new(400.0, 300.0);
        app.wheel(at, 0.0, -120.0, true);
        assert!(app.view.zoom() > before);
        app.wheel(at, 0.0, 120.0, true);
        assert!((app.view.zoom() - before).abs() < 1e-9);
    }
}
