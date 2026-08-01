//! SVG import: an element tree into the [`Scene`] graph.
//!
//! One walk, top down, carrying the computed [`Style`] of the parent. Shapes
//! become [`Path`]s, groups become groups, `use` is expanded into a copy of
//! what it points at, and everything else is recorded in
//! [`Scene::losses`](crate::scene::Scene::losses).
//!
//! # What is deliberately dropped
//!
//! Text, images, filters, masks, patterns, markers and animation, each with a
//! [`Loss`] naming the element and where it was. That list is the honest
//! statement of D-008: the scene graph is the document, so import keeps what
//! the scene graph can hold and *says* what it could not.
//!
//! Transforms are not baked into geometry. A group keeps its own matrix, so
//! export writes back the file's structure rather than a flattened copy of it,
//! and `gradientUnits="userSpaceOnUse"` still means what it meant.
//! [`Scene::flatten`](crate::scene::Scene::flatten) composes the chain when a
//! consumer wants world space.

use crate::scene::{Clip, Gradient, GradientKind, Loss, Node, NodeKind, Scene, Stop};
use crate::style::Style;
use crate::xml::{Element, Error, Node as XmlNode, parse};
use crate::{path_data, transform};
use hane_geom::{Affine, PathEl, Point, Rect, Vec2};
use hane_path::Path;
use hane_scene::{Arena, NodeId};
use std::collections::HashMap;

/// The SVG namespace. An element in any other namespace is not ours to draw.
const SVG_NS: &str = "http://www.w3.org/2000/svg";
/// The namespace `xlink:href` lives in, which is how SVG 1.1 spells `href`.
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// How deep `use` may expand before we stop.
///
/// A `use` chain is not bounded by the document's nesting the way elements are:
/// two elements that each `use` the other are finite text and infinite
/// expansion. The cycle check below catches exactly that, and this catches the
/// acyclic-but-exponential version, where a chain of ten elements each using
/// the previous one twice is a thousand copies.
const MAX_USE_DEPTH: u32 = 16;

/// Import an SVG document into the scene graph.
///
/// Everything the scene graph cannot represent is reported in
/// [`Scene::losses`] rather than dropped silently, and never stops the import:
/// a file with one `<text>` in it still opens, minus the text.
///
/// # Errors
///
/// Only two things fail outright: XML that does not parse, and a root element
/// that is not `<svg>`. Neither leaves anything to import.
pub fn import(source: &str) -> Result<Scene, Error> {
    let root = parse(source)?;
    if !is_svg(&root) || root.name.local != "svg" {
        return Err(Error {
            message: format!("root element is <{}>, not <svg>", root.name.local),
            line: root.line,
            column: root.column,
        });
    }

    let mut index = Index::default();
    index.collect(&root);

    let view_box = root.attribute("viewBox").and_then(parse_view_box);
    let width = root.attribute("width").map(str::to_string);
    let height = root.attribute("height").map(str::to_string);
    // What a percentage length is a percentage *of*. The viewBox wins because
    // it is what the document's own coordinates are expressed in; a `width` in
    // physical units is the fallback, and 0 is the honest answer when neither
    // is there, which makes `50%` come out as 0 rather than as a guess.
    let viewport = match view_box {
        Some(b) => (b.width(), b.height()),
        None => (
            width.as_deref().and_then(|w| length(w, 0.0)).unwrap_or(0.0),
            height
                .as_deref()
                .and_then(|h| length(h, 0.0))
                .unwrap_or(0.0),
        ),
    };

    let mut importer = Importer {
        index,
        viewport,
        stack: Vec::new(),
        scene: Scene {
            nodes: Arena::new(),
            roots: Vec::new(),
            gradients: Vec::new(),
            clips: Vec::new(),
            view_box,
            width,
            height,
            losses: Vec::new(),
        },
    };

    importer.definitions();
    let root_style = Style::initial().resolve(&root);
    let roots = importer.children(&root, &root_style, 0);
    importer.scene.roots = roots;
    Ok(importer.scene)
}

/// Everything reachable by id, plus the definition elements, found in one pass.
#[derive(Default)]
struct Index<'a> {
    /// Every element carrying an `id`, so `use` and `url(#...)` resolve. First
    /// wins, which is what a browser does with a duplicate id.
    by_id: HashMap<&'a str, &'a Element>,
    /// Gradient elements in document order, with their ids.
    gradients: Vec<&'a Element>,
    /// `clipPath` elements in document order.
    clips: Vec<&'a Element>,
    /// `<style>` elements, wherever they are.
    ///
    /// Found here rather than during the node walk because the walk does not
    /// enter `<defs>`, and a stylesheet in there still restyles the whole
    /// document -- silently, which is the one thing import may not do.
    styles: Vec<&'a Element>,
}

impl<'a> Index<'a> {
    fn collect(&mut self, el: &'a Element) {
        if let Some(id) = el.attribute("id")
            && !id.is_empty()
        {
            self.by_id.entry(id).or_insert(el);
        }
        if is_svg(el) {
            match el.name.local.as_str() {
                "linearGradient" | "radialGradient" => self.gradients.push(el),
                "clipPath" => self.clips.push(el),
                "style" => self.styles.push(el),
                _ => {}
            }
        }
        for child in elements(el) {
            self.collect(child);
        }
    }
}

struct Importer<'a> {
    index: Index<'a>,
    /// `(width, height)` that a percentage length resolves against.
    viewport: (f64, f64),
    /// The ids currently being expanded by `use`, innermost last, so a cycle is
    /// a membership test rather than a hang.
    stack: Vec<&'a str>,
    scene: Scene,
}

impl<'a> Importer<'a> {
    fn loss(&mut self, el: &Element, message: impl Into<String>) {
        self.scene.losses.push(Loss {
            message: message.into(),
            line: el.line,
            column: el.column,
        });
    }

    /// Import every gradient and clip definition, whether referenced or not.
    ///
    /// Unreferenced defs are kept because export writes them back: a round trip
    /// that quietly deleted the palette an author had not applied yet would be
    /// data loss the criteria do not ask for.
    fn definitions(&mut self) {
        for el in self.index.styles.clone() {
            self.loss(el, "CSS in a <style> element is not applied");
        }
        for el in self.index.gradients.clone() {
            if let Some(g) = self.gradient(el) {
                self.scene.gradients.push(g);
            }
        }
        for el in self.index.clips.clone() {
            let Some(id) = el.attribute("id").filter(|id| !id.is_empty()) else {
                continue;
            };
            if el.attribute("clipPathUnits") == Some("objectBoundingBox") {
                self.loss(el, "clipPathUnits=\"objectBoundingBox\" ignored");
            }
            let mut paths = Vec::new();
            self.clip_shapes(el, Affine::IDENTITY, &mut paths);
            self.scene.clips.push(Clip {
                id: id.to_string(),
                paths,
            });
        }
    }

    /// Shapes inside a `<clipPath>`, flattened into the clip's own space.
    ///
    /// Baking the transform is safe here in a way it would not be for a drawn
    /// node: clip geometry carries no paint, so there is no `userSpaceOnUse`
    /// gradient to shift out from under.
    fn clip_shapes(&mut self, parent: &Element, ctm: Affine, out: &mut Vec<Path>) {
        for el in elements(parent).collect::<Vec<_>>() {
            let ctm = ctm * self.transform_of(el);
            match el.name.local.as_str() {
                "g" if is_svg(el) => self.clip_shapes(el, ctm, out),
                name if is_shape(name) => {
                    if let Some(path) = self.shape(el) {
                        out.push(path.transform(ctm));
                    }
                }
                // ponytail: no `use` inside a clipPath -- it needs the cycle
                // guard the node walk has, for a construct that is rare in
                // real files. Route it through `node` and flatten if it stops
                // being rare.
                name => self.loss(el, format!("<{name}> in a clipPath ignored")),
            }
        }
    }

    fn gradient(&mut self, el: &'a Element) -> Option<Gradient> {
        let id = el.attribute("id").filter(|id| !id.is_empty())?.to_string();
        let radial = el.name.local == "radialGradient";
        let (vw, vh) = self.viewport;
        // A gradient's own coordinates are fractions of the object's bounding
        // box unless it says otherwise, and a fraction has no viewport to be a
        // percentage of -- so `50%` there means 0.5, not half the viewport.
        let user_space = self.inherited(el, "gradientUnits") == Some("userSpaceOnUse");
        let (px, py) = if user_space { (vw, vh) } else { (1.0, 1.0) };
        let diagonal = if user_space {
            ((vw * vw + vh * vh) / 2.0).sqrt()
        } else {
            1.0
        };
        let get = |name: &str, percent_of: f64, default: f64| {
            self.inherited(el, name)
                .and_then(|v| length(v, percent_of))
                .unwrap_or(default)
        };

        let kind = if radial {
            let center = Point::new(get("cx", px, 0.5 * px), get("cy", py, 0.5 * py));
            GradientKind::Radial {
                center,
                radius: get("r", diagonal, 0.5 * diagonal),
                focus: Point::new(get("fx", px, center.x), get("fy", py, center.y)),
            }
        } else {
            GradientKind::Linear {
                start: Point::new(get("x1", px, 0.0), get("y1", py, 0.0)),
                end: Point::new(get("x2", px, px), get("y2", py, 0.0)),
            }
        };

        let transform = match self.inherited(el, "gradientTransform") {
            Some(t) => match transform::parse_transform(t) {
                Ok(t) => t,
                Err(e) => {
                    self.loss(el, format!("gradientTransform ignored: {}", e.message));
                    Affine::IDENTITY
                }
            },
            None => Affine::IDENTITY,
        };
        let spread = self
            .inherited(el, "spreadMethod")
            .unwrap_or("pad")
            .to_string();

        Some(Gradient {
            id,
            kind,
            transform,
            user_space,
            spread,
            stops: self.stops(el),
        })
    }

    /// The stops of `el`, or of the nearest gradient in its `href` chain that
    /// has any.
    ///
    /// The chain is the normal case, not an edge one: Inkscape writes the stops
    /// once and every use of that gradient is a stopless element with an
    /// `xlink:href` and its own `gradientTransform`.
    fn stops(&mut self, el: &'a Element) -> Vec<Stop> {
        let mut source = el;
        for _ in 0..MAX_USE_DEPTH {
            if elements(source).any(|c| c.name.local == "stop") {
                break;
            }
            match self.href_target(source) {
                Some(next) => source = next,
                None => break,
            }
        }
        let parent = Style::initial().resolve(source);
        elements(source)
            .filter(|c| c.name.local == "stop")
            .map(|c| {
                let style = parent.resolve(c);
                Stop {
                    // A stop past the end of the gradient is clamped, not
                    // dropped: it still bounds the ramp before it.
                    offset: length(c.attribute("offset").unwrap_or("0"), 1.0)
                        .unwrap_or(0.0)
                        .clamp(0.0, 1.0),
                    color: style.get("stop-color").unwrap_or("black").to_string(),
                    opacity: style.get("stop-opacity").unwrap_or("1").to_string(),
                }
            })
            .collect()
    }

    /// An attribute of `el`, or of the first element in its `href` chain that
    /// has it.
    fn inherited(&self, el: &'a Element, name: &str) -> Option<&'a str> {
        let mut cur = el;
        for _ in 0..MAX_USE_DEPTH {
            if let Some(v) = cur.attribute(name) {
                return Some(v);
            }
            cur = self.href_target(cur)?;
        }
        None
    }

    /// The element `el`'s `href` or `xlink:href` points at, within this
    /// document.
    fn href_target(&self, el: &Element) -> Option<&'a Element> {
        let fragment = href(el)?.strip_prefix('#')?;
        self.index.by_id.get(fragment).copied()
    }

    /// The child nodes of `parent`, in paint order.
    fn children(&mut self, parent: &'a Element, style: &Style, depth: u32) -> Vec<NodeId> {
        elements(parent)
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|el| self.node(el, style, depth))
            .collect()
    }

    /// One element, and everything under it.
    ///
    /// `None` when the element draws nothing -- a definition, a zero-sized
    /// shape, an empty group, or something unsupported that has already been
    /// recorded as a loss.
    fn node(&mut self, el: &'a Element, parent: &Style, depth: u32) -> Option<NodeId> {
        if !is_svg(el) {
            // A foreign-namespace element inside SVG is somebody else's data
            // (Inkscape's `sodipodi:namedview`, Illustrator's metadata), not a
            // shape we failed to draw. Nothing renders it, so nothing is lost.
            return None;
        }
        let style = parent.resolve(el);
        let name = el.name.local.as_str();
        let kind = match name {
            // `symbol` and a nested `svg` reach here only as the target of a
            // `use`; both group their children.
            "g" | "a" | "symbol" | "svg" => {
                if name != "g" && el.attribute("viewBox").is_some() {
                    self.loss(el, format!("viewBox on <{name}> ignored"));
                }
                let children = self.children(el, &style, depth);
                if children.is_empty() {
                    return None;
                }
                NodeKind::Group(children)
            }
            "switch" => {
                // ponytail: the first child, not the first whose
                // requiredFeatures/systemLanguage pass -- we support no
                // features and have no locale, so every test would fail and
                // the switch would render nothing at all.
                self.loss(el, "<switch> imported as its first branch");
                let child = elements(el).next()?;
                let children = self.node(child, &style, depth).into_iter().collect();
                NodeKind::Group(children)
            }
            "use" => NodeKind::Group(self.use_element(el, &style, depth)?),
            // Handled elsewhere or carrying nothing to draw. Silent, because a
            // loss list that reports `<title>` is a loss list nobody reads.
            // `style` is reported once from the index instead, which is the
            // only way a stylesheet inside `<defs>` gets reported at all.
            "defs" | "clipPath" | "linearGradient" | "radialGradient" | "title" | "desc"
            | "metadata" | "stop" | "style" => return None,
            _ if is_shape(name) => NodeKind::Path(self.shape(el)?),
            _ => {
                self.loss(el, format!("<{name}> is not supported and was dropped"));
                return None;
            }
        };

        let clip_path = el
            .attribute("clip-path")
            .and_then(url_fragment)
            .map(str::to_string);
        if let Some(id) = &clip_path
            && !self.scene.clips.iter().any(|c| &c.id == id)
        {
            self.loss(
                el,
                format!("clip-path references #{id}, which is not a clipPath"),
            );
        }
        // Effects we do not implement, reported where they are *applied*
        // rather than where they are defined: the definition may sit inside a
        // `<defs>` this walk never enters, and it is the reference that changes
        // what the user sees.
        for effect in ["mask", "filter", "marker-start", "marker-mid", "marker-end"] {
            if let Some(value) = el.attribute(effect)
                && value != "none"
            {
                self.loss(el, format!("{effect}=\"{value}\" ignored"));
            }
        }
        for property in ["fill", "stroke"] {
            if let Some(id) = style.get(property).and_then(url_fragment)
                && !self.scene.gradients.iter().any(|g| g.id == id)
            {
                let message = format!("{property} references #{id}, which is not a gradient");
                self.loss(el, message);
            }
        }

        let mut transform = self.transform_of(el);
        if name == "use" {
            // `use` places its copy at (x, y), which composes *after* its own
            // transform: the translation is in the referencing element's space.
            let (vw, vh) = self.viewport;
            let x = el.attribute("x").and_then(|v| length(v, vw)).unwrap_or(0.0);
            let y = el.attribute("y").and_then(|v| length(v, vh)).unwrap_or(0.0);
            transform = transform * Affine::translate(Vec2::new(x, y));
        }

        Some(self.scene.nodes.insert(Node {
            kind,
            transform,
            style,
            clip_path,
        }))
    }

    /// The children a `use` expands to: a copy of what it points at, inside a
    /// group carrying the `use`'s own `x`/`y` translation.
    fn use_element(&mut self, el: &'a Element, style: &Style, depth: u32) -> Option<Vec<NodeId>> {
        let Some(fragment) = href(el).and_then(|h| h.strip_prefix('#')) else {
            self.loss(el, "<use> without a same-document href");
            return None;
        };
        let Some(&target) = self.index.by_id.get(fragment) else {
            self.loss(
                el,
                format!("<use> references #{fragment}, which does not exist"),
            );
            return None;
        };
        if self.stack.contains(&fragment) || depth >= MAX_USE_DEPTH {
            self.loss(
                el,
                format!("<use> of #{fragment} recurses; expansion stopped"),
            );
            return None;
        }
        self.stack.push(fragment);
        let child = self.node(target, style, depth + 1);
        self.stack.pop();
        child.map(|child| vec![child])
    }

    /// The geometry of a basic shape, or `None` when the shape draws nothing.
    ///
    /// Only called for the names [`is_shape`] accepts, so `None` always means
    /// "empty", never "unsupported" -- keeping those apart is what stops a
    /// `width="0"` rect from being reported as a lost element.
    fn shape(&mut self, el: &Element) -> Option<Path> {
        let (vw, vh) = self.viewport;
        let get = |name: &str, percent_of: f64| {
            el.attribute(name)
                .and_then(|v| length(v, percent_of))
                .unwrap_or(0.0)
        };
        let els = match el.name.local.as_str() {
            "rect" => {
                let (w, h) = (get("width", vw), get("height", vh));
                // Zero or negative disables rendering, per 9.2 -- an error the
                // spec defines away, so not a loss.
                if w <= 0.0 || h <= 0.0 {
                    return None;
                }
                let origin = Point::new(get("x", vw), get("y", vh));
                // Either radius alone gives both, and each is capped at half
                // its side, so `rx="999"` on a small rect is a stadium rather
                // than a self-crossing tangle.
                let rx = el.attribute("rx").and_then(|v| length(v, vw));
                let ry = el.attribute("ry").and_then(|v| length(v, vh));
                let radii = Vec2::new(
                    rx.or(ry).unwrap_or(0.0).clamp(0.0, w / 2.0),
                    ry.or(rx).unwrap_or(0.0).clamp(0.0, h / 2.0),
                );
                rounded_rect(origin, Vec2::new(w, h), radii)
            }
            "circle" => {
                let r = get("r", ((vw * vw + vh * vh) / 2.0).sqrt());
                if r <= 0.0 {
                    return None;
                }
                ellipse(Point::new(get("cx", vw), get("cy", vh)), Vec2::new(r, r))
            }
            "ellipse" => {
                let radii = Vec2::new(get("rx", vw), get("ry", vh));
                if radii.x <= 0.0 || radii.y <= 0.0 {
                    return None;
                }
                ellipse(Point::new(get("cx", vw), get("cy", vh)), radii)
            }
            "line" => vec![
                PathEl::MoveTo(Point::new(get("x1", vw), get("y1", vh))),
                PathEl::LineTo(Point::new(get("x2", vw), get("y2", vh))),
            ],
            // `points` is a moveto followed by implicit linetos, which is
            // exactly what path data means by the same numbers -- including
            // that an odd count renders the valid prefix.
            "polyline" | "polygon" => {
                let points = el.attribute("points").unwrap_or("").trim();
                if points.is_empty() {
                    return None;
                }
                let closed = el.name.local == "polygon";
                let d = format!("M{points}{}", if closed { "Z" } else { "" });
                self.path_data(el, &d)
            }
            "path" => self.path_data(el, el.attribute("d").unwrap_or("")),
            _ => return None,
        };
        (!els.is_empty()).then(|| Path::from(els))
    }

    /// Path data, with a parse failure recorded and the valid prefix kept.
    fn path_data(&mut self, el: &Element, d: &str) -> Vec<PathEl> {
        let (els, err) = path_data::parse(d);
        if let Some(err) = err {
            self.loss(
                el,
                format!(
                    "path data truncated at offset {}: {}",
                    err.offset, err.message
                ),
            );
        }
        els
    }

    fn transform_of(&mut self, el: &Element) -> Affine {
        let Some(value) = el.attribute("transform") else {
            return Affine::IDENTITY;
        };
        match transform::parse_transform(value) {
            Ok(t) => t,
            Err(e) => {
                self.loss(el, format!("transform ignored: {}", e.message));
                Affine::IDENTITY
            }
        }
    }
}

/// A rectangle, with elliptical corners when either radius is non-zero.
fn rounded_rect(origin: Point, size: Vec2, radii: Vec2) -> Vec<PathEl> {
    let (x0, y0) = (origin.x, origin.y);
    let (x1, y1) = (x0 + size.x, y0 + size.y);
    if radii.x <= 0.0 || radii.y <= 0.0 {
        return vec![
            PathEl::MoveTo(Point::new(x0, y0)),
            PathEl::LineTo(Point::new(x1, y0)),
            PathEl::LineTo(Point::new(x1, y1)),
            PathEl::LineTo(Point::new(x0, y1)),
            PathEl::ClosePath,
        ];
    }
    let (rx, ry) = (radii.x, radii.y);
    let corners = [
        (Point::new(x1 - rx, y0), Point::new(x1, y0 + ry)),
        (Point::new(x1, y1 - ry), Point::new(x1 - rx, y1)),
        (Point::new(x0 + rx, y1), Point::new(x0, y1 - ry)),
        (Point::new(x0, y0 + ry), Point::new(x0 + rx, y0)),
    ];
    let mut els = vec![PathEl::MoveTo(Point::new(x0 + rx, y0))];
    for (before, after) in corners {
        els.push(PathEl::LineTo(before));
        els.extend(PathEl::arc(before, radii, 0.0, false, true, after));
    }
    els.push(PathEl::ClosePath);
    els
}

/// An ellipse, as two half-arcs.
///
/// Not one arc: an arc whose endpoints coincide draws nothing at all (F.6.2),
/// so a full turn has to be cut somewhere.
fn ellipse(center: Point, radii: Vec2) -> Vec<PathEl> {
    let right = Point::new(center.x + radii.x, center.y);
    let left = Point::new(center.x - radii.x, center.y);
    let mut els = vec![PathEl::MoveTo(right)];
    els.extend(PathEl::arc(right, radii, 0.0, false, true, left));
    els.extend(PathEl::arc(left, radii, 0.0, false, true, right));
    els.push(PathEl::ClosePath);
    els
}

/// The child elements of `el`, skipping text nodes.
fn elements(el: &Element) -> impl Iterator<Item = &Element> {
    el.children.iter().filter_map(|child| match child {
        XmlNode::Element(e) => Some(e),
        XmlNode::Text(_) => None,
    })
}

/// True for the names of the basic shapes, the elements [`Importer::shape`]
/// turns into geometry.
fn is_shape(name: &str) -> bool {
    matches!(
        name,
        "rect" | "circle" | "ellipse" | "line" | "polyline" | "polygon" | "path"
    )
}

/// True when `el` is in the SVG namespace, or in none at all.
///
/// A document with no `xmlns` is invalid and common: browsers refuse it, every
/// icon-set generator emits it, and a file that opens everywhere except here
/// would be our bug to explain.
fn is_svg(el: &Element) -> bool {
    el.name.namespace.as_deref().is_none_or(|ns| ns == SVG_NS)
}

/// The `href` or `xlink:href` of `el`.
fn href(el: &Element) -> Option<&str> {
    el.attributes
        .iter()
        .find(|a| {
            a.name.local == "href" && a.name.namespace.as_deref().is_none_or(|ns| ns == XLINK_NS)
        })
        .map(|a| a.value.as_str())
}

/// The `id` inside `url(#id)`, for a paint or clip reference.
fn url_fragment(value: &str) -> Option<&str> {
    let inner = value.trim().strip_prefix("url(")?.strip_suffix(')')?;
    let inner = inner.trim().trim_matches(['"', '\'']);
    inner.strip_prefix('#').filter(|id| !id.is_empty())
}

/// `viewBox="x y width height"` as the rectangle it denotes.
///
/// A non-positive width or height disables rendering of the whole document
/// (7.7), and a `viewBox` we cannot read is better absent than half-applied, so
/// both come back as `None`.
fn parse_view_box(value: &str) -> Option<Rect> {
    let mut it = numbers(value);
    let (x, y, w, h) = (it.next()?, it.next()?, it.next()?, it.next()?);
    (w > 0.0 && h > 0.0).then(|| Rect::new(x, y, x + w, y + h))
}

/// The numbers in a comma-or-whitespace separated list, stopping at the first
/// thing that is not one.
fn numbers(value: &str) -> impl Iterator<Item = f64> {
    value
        .split([' ', '\t', '\n', '\r', ','])
        .filter(|s| !s.is_empty())
        .map_while(|s| s.parse::<f64>().ok().filter(|v: &f64| v.is_finite()))
}

/// A `<length>` or `<coordinate>` in user units.
///
/// `percent_of` is what `100%` means in this position. Unitless is already user
/// units; the absolute units convert at the 96dpi CSS pixel the SVG 1.1
/// processing model fixes them to. `em` and `ex` need a font, so they are not
/// lengths this crate can resolve and come back as `None`.
fn length(value: &str, percent_of: f64) -> Option<f64> {
    const UNITS: [(&str, f64); 7] = [
        ("px", 1.0),
        ("pt", 96.0 / 72.0),
        ("pc", 16.0),
        ("mm", 96.0 / 25.4),
        ("cm", 96.0 / 2.54),
        ("in", 96.0),
        ("%", 0.0),
    ];
    let value = value.trim();
    let (number, factor) = UNITS
        .iter()
        .find_map(|&(unit, factor)| {
            let rest = value.strip_suffix(unit)?;
            Some((
                rest,
                if unit == "%" {
                    percent_of / 100.0
                } else {
                    factor
                },
            ))
        })
        .unwrap_or((value, 1.0));
    let parsed: f64 = number.trim().parse().ok()?;
    // `parse` accepts `inf` and `NaN`, which no SVG number grammar does, and
    // either one poisons every bounding box it ever reaches.
    (parsed.is_finite() && factor.is_finite()).then_some(parsed * factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::NodeKind;

    fn scene(source: &str) -> Scene {
        import(source).expect("well-formed")
    }

    /// The one-line document the shape tests vary.
    fn wrap(body: &str) -> String {
        format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 100 100\">{body}</svg>")
    }

    fn bounds(scene: &Scene) -> Rect {
        scene
            .flatten()
            .iter()
            .fold(Rect::EMPTY, |b, p| b.union(p.bounding_box()))
    }

    #[test]
    fn every_basic_shape_imports() {
        for body in [
            "<rect x='10' y='20' width='30' height='30'/>",
            "<circle cx='25' cy='35' r='15'/>",
            "<ellipse cx='25' cy='35' rx='15' ry='15'/>",
            "<line x1='10' y1='20' x2='40' y2='50'/>",
            "<polyline points='10,20 40,20 40,50'/>",
            "<polygon points='10,20 40,20 40,50'/>",
            "<path d='M10 20 L40 20 L40 50 Z'/>",
        ] {
            let scene = scene(&wrap(&body.replace('\'', "\"")));
            assert_eq!(scene.roots.len(), 1, "{body}");
            assert!(scene.losses.is_empty(), "{body}: {:?}", scene.losses);
            let b = bounds(&scene);
            // Every shape above spans the same box, which is only true if the
            // attribute-to-geometry mapping of each is right.
            for (got, want) in [(b.x0, 10.0), (b.y0, 20.0), (b.x1, 40.0), (b.y1, 50.0)] {
                assert!((got - want).abs() < 1e-9, "{body}: {b:?}");
            }
        }
    }

    #[test]
    fn a_rects_corner_radii_are_capped_at_half_the_side() {
        // rx alone gives ry, and 999 is clamped to a stadium rather than
        // producing a self-crossing corner.
        let scene = scene(&wrap("<rect width=\"40\" height=\"20\" rx=\"999\"/>"));
        let b = bounds(&scene);
        assert!(
            (b.x1 - 40.0).abs() < 1e-9 && (b.y1 - 20.0).abs() < 1e-9,
            "{b:?}"
        );
    }

    #[test]
    fn a_degenerate_shape_draws_nothing_and_is_not_a_loss() {
        for body in [
            "<rect width=\"0\" height=\"10\"/>",
            "<circle r=\"0\"/>",
            "<polyline points=\"\"/>",
        ] {
            let scene = scene(&wrap(body));
            assert!(scene.roots.is_empty(), "{body}");
            assert!(scene.losses.is_empty(), "{body}: {:?}", scene.losses);
        }
    }

    #[test]
    fn nested_group_transforms_compose_outermost_last() {
        let scene = scene(&wrap(
            "<g transform=\"translate(10 0)\"><g transform=\"scale(2)\">\
             <rect width=\"1\" height=\"1\"/></g></g>",
        ));
        // scale first, then translate: (0,0)-(1,1) -> (10,0)-(12,2). The other
        // order would give (20,0)-(22,2).
        let b = bounds(&scene);
        assert!(
            (b.x0 - 10.0).abs() < 1e-9 && (b.x1 - 12.0).abs() < 1e-9,
            "{b:?}"
        );
    }

    #[test]
    fn presentation_style_inherits_through_groups() {
        let scene = scene(&wrap(
            "<g fill=\"red\" style=\"stroke: blue\"><path d=\"M0 0L1 1\" stroke=\"green\"/></g>",
        ));
        let group = scene.nodes.get(scene.roots[0]).unwrap();
        let NodeKind::Group(children) = &group.kind else {
            panic!("expected a group");
        };
        let leaf = scene.nodes.get(children[0]).unwrap();
        assert_eq!(leaf.style.get("fill"), Some("red"));
        // The child's own presentation attribute beats the inherited value.
        assert_eq!(leaf.style.get("stroke"), Some("green"));
        assert_eq!(group.style.get("stroke"), Some("blue"));
    }

    #[test]
    fn use_resolves_with_its_own_offset() {
        let scene = scene(&wrap(
            "<defs><rect id=\"r\" width=\"10\" height=\"10\"/></defs>\
             <use href=\"#r\" x=\"5\" y=\"7\"/>",
        ));
        assert!(scene.losses.is_empty(), "{:?}", scene.losses);
        let b = bounds(&scene);
        assert!(
            (b.x0 - 5.0).abs() < 1e-9 && (b.y0 - 7.0).abs() < 1e-9,
            "{b:?}"
        );
        // The definition itself is not drawn a second time.
        assert_eq!(scene.flatten().len(), 1);
    }

    #[test]
    fn a_circular_use_is_reported_rather_than_expanded_forever() {
        let scene = scene(&wrap(
            "<g id=\"a\"><use href=\"#b\"/></g><g id=\"b\"><use href=\"#a\"/></g>",
        ));
        assert!(
            scene.losses.iter().any(|l| l.message.contains("recurses")),
            "{:?}",
            scene.losses
        );
    }

    #[test]
    fn unsupported_elements_are_reported_with_their_position() {
        let scene = import(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n  <text>hi</text>\n  <image/>\n</svg>",
        )
        .unwrap();
        assert_eq!(scene.losses.len(), 2, "{:?}", scene.losses);
        assert!(scene.losses[0].message.contains("<text>"));
        assert_eq!((scene.losses[0].line, scene.losses[0].column), (2, 3));
        assert!(scene.losses[1].message.contains("<image>"));
    }

    #[test]
    fn malformed_path_data_keeps_its_valid_prefix_and_reports_the_rest() {
        let scene = scene(&wrap("<path d=\"M0 0 L10 10 L20 banana\"/>"));
        assert_eq!(scene.flatten()[0].elements().len(), 2);
        assert!(scene.losses[0].message.contains("truncated"));
    }

    #[test]
    fn gradients_resolve_their_href_chain() {
        let scene = scene(&wrap(
            "<defs>\
               <linearGradient id=\"base\"><stop offset=\"0\" stop-color=\"red\"/>\
               <stop offset=\"1\" stop-color=\"blue\" stop-opacity=\"0.5\"/></linearGradient>\
               <linearGradient id=\"used\" xlink:href=\"#base\" x1=\"0.25\" \
                gradientTransform=\"translate(3)\" \
                xmlns:xlink=\"http://www.w3.org/1999/xlink\"/>\
               <radialGradient id=\"r\" cx=\"2\" r=\"3\" gradientUnits=\"userSpaceOnUse\"/>\
             </defs>\
             <path d=\"M0 0L1 1\" fill=\"url(#used)\"/>",
        ));
        let used = scene.gradients.iter().find(|g| g.id == "used").unwrap();
        // Stops came from the chain, not from the empty element itself.
        assert_eq!(used.stops.len(), 2);
        assert_eq!(used.stops[1].color, "blue");
        assert_eq!(used.stops[1].opacity, "0.5");
        assert_eq!(used.transform.as_coeffs()[4], 3.0);
        assert!(!used.user_space);
        let GradientKind::Linear { start, end } = used.kind else {
            panic!("expected a linear gradient");
        };
        assert_eq!((start.x, end.x), (0.25, 1.0));
        let r = scene.gradients.iter().find(|g| g.id == "r").unwrap();
        assert!(r.user_space);
        assert!(matches!(r.kind, GradientKind::Radial { radius, .. } if radius == 3.0));
        assert!(scene.losses.is_empty(), "{:?}", scene.losses);
    }

    #[test]
    fn a_paint_reference_to_something_that_is_not_a_gradient_is_reported() {
        let scene = scene(&wrap(
            "<pattern id=\"p\"/><path d=\"M0 0L1 1\" fill=\"url(#p)\"/>",
        ));
        assert!(
            scene
                .losses
                .iter()
                .any(|l| l.message.contains("fill references #p")),
            "{:?}",
            scene.losses
        );
    }

    #[test]
    fn clip_paths_flatten_their_own_transforms() {
        let scene = scene(&wrap(
            "<clipPath id=\"c\"><g transform=\"translate(5 0)\">\
             <rect width=\"10\" height=\"10\"/></g></clipPath>\
             <path d=\"M0 0L20 20\" clip-path=\"url(#c)\"/>",
        ));
        assert_eq!(scene.clips.len(), 1);
        let b = scene.clips[0].paths[0].bounding_box();
        assert!(
            (b.x0 - 5.0).abs() < 1e-9 && (b.x1 - 15.0).abs() < 1e-9,
            "{b:?}"
        );
        let node = scene.nodes.get(scene.roots[0]).unwrap();
        assert_eq!(node.clip_path.as_deref(), Some("c"));
        assert!(scene.losses.is_empty(), "{:?}", scene.losses);
    }

    #[test]
    fn effects_we_cannot_render_are_reported_where_they_are_applied() {
        // Both definitions sit inside `<defs>`, which the node walk never
        // enters -- so the reference is the only place the loss can surface.
        let scene = scene(&wrap(
            "<defs><style>* { fill: red }</style><filter id=\"f\"/></defs>\
             <path d=\"M0 0L1 1\" filter=\"url(#f)\" mask=\"url(#m)\"/>",
        ));
        let messages: Vec<&str> = scene.losses.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].contains("<style>"));
        assert!(messages.iter().any(|m| m.starts_with("filter=")));
        assert!(messages.iter().any(|m| m.starts_with("mask=")));
    }

    #[test]
    fn lengths_carry_units_and_percentages() {
        assert_eq!(length("12", 0.0), Some(12.0));
        assert_eq!(length(" -3.5px ", 0.0), Some(-3.5));
        assert_eq!(length("1in", 0.0), Some(96.0));
        assert_eq!(length("50%", 200.0), Some(100.0));
        // No font, so no em; and nothing that is not a number.
        assert_eq!(length("2em", 10.0), None);
        assert_eq!(length("inf", 0.0), None);
        assert_eq!(length("", 0.0), None);
    }

    #[test]
    fn a_root_that_is_not_svg_fails_and_malformed_xml_fails() {
        assert!(import("<html/>").is_err());
        assert!(import("<svg").is_err());
    }
}
