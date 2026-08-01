//! SVG export: the scene graph written back out as an SVG 1.1 document.
//!
//! The output is deliberately plain -- absolute path commands, presentation
//! attributes, no CSS, no `xlink` -- because the thing it has to do is open
//! everywhere, and every generator that got clever about this is the reason
//! some files only open in the tool that wrote them.
//!
//! # Precision
//!
//! [`write`] emits the shortest text that reads back to the same `f64`, which
//! is what makes import-export-import exact rather than merely close.
//! [`write_with_precision`] trades that for size: coordinates are rounded to a
//! fixed number of decimals *before* serialising, so the numbers stay short in
//! the file instead of being long numbers that happen to be near round ones.

use crate::path_data;
use crate::scene::{Clip, Gradient, GradientKind, NodeKind, Scene};
use crate::style::Style;
use hane_geom::{Affine, PathEl, Point};
use hane_path::Path;
use hane_scene::NodeId;

/// Write the scene as an SVG 1.1 document.
///
/// Coordinates keep full `f64` precision: a round trip through this and
/// [`import`](crate::import::import) reproduces geometry bit for bit, not to a
/// tolerance.
#[must_use]
pub fn write(scene: &Scene) -> String {
    write_inner(scene, None)
}

/// Write the scene with every coordinate rounded to `decimals` decimal places.
///
/// Six or so is plenty for a document whose coordinates are pixels; the cost is
/// that a round trip is then only accurate to that rounding, so
/// `10.000000001` comes back as `10`.
#[must_use]
pub fn write_with_precision(scene: &Scene, decimals: u8) -> String {
    write_inner(scene, Some(decimals))
}

fn write_inner(scene: &Scene, precision: Option<u8>) -> String {
    let mut w = Writer {
        out: String::new(),
        precision,
    };
    w.out
        .push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\" version=\"1.1\"");
    if let Some(width) = &scene.width {
        w.attribute("width", width);
    }
    if let Some(height) = &scene.height {
        w.attribute("height", height);
    }
    if let Some(b) = scene.view_box {
        let value = format!(
            "{} {} {} {}",
            w.num(b.x0),
            w.num(b.y0),
            w.num(b.width()),
            w.num(b.height())
        );
        w.attribute("viewBox", &value);
    }
    w.out.push_str(">\n");

    if !scene.gradients.is_empty() || !scene.clips.is_empty() {
        w.out.push_str("  <defs>\n");
        for gradient in &scene.gradients {
            w.gradient(gradient);
        }
        for clip in &scene.clips {
            w.clip(clip);
        }
        w.out.push_str("  </defs>\n");
    }

    let initial = Style::initial();
    for &root in &scene.roots {
        w.node(scene, root, &initial, 1);
    }
    w.out.push_str("</svg>\n");
    w.out
}

struct Writer {
    out: String,
    precision: Option<u8>,
}

impl Writer {
    fn num(&self, v: f64) -> String {
        let v = match self.precision {
            Some(decimals) => round(v, decimals),
            None => v,
        };
        // f64's Display is the shortest string that parses back to the same
        // bits, which is the entire round-trip guarantee.
        v.to_string()
    }

    fn attribute(&mut self, name: &str, value: &str) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        // Escaping is not optional even for values that came out of a parsed
        // document: a colour of `url(#a&b)` would otherwise close nothing and
        // produce a file that will not parse again.
        for c in value.chars() {
            match c {
                '&' => self.out.push_str("&amp;"),
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                '"' => self.out.push_str("&quot;"),
                _ => self.out.push(c),
            }
        }
        self.out.push('"');
    }

    fn indent(&mut self, depth: usize) {
        for _ in 0..depth {
            self.out.push_str("  ");
        }
    }

    fn node(&mut self, scene: &Scene, id: NodeId, parent: &Style, depth: usize) {
        let Some(node) = scene.nodes.get(id) else {
            return;
        };
        let name = match &node.kind {
            NodeKind::Group(_) => "g",
            NodeKind::Path(_) => "path",
        };
        self.indent(depth);
        self.out.push('<');
        self.out.push_str(name);
        if let NodeKind::Path(path) = &node.kind {
            let d = self.path_data(path);
            self.attribute("d", &d);
        }
        if node.transform != Affine::IDENTITY {
            let [a, b, c, d, e, f] = node.transform.as_coeffs();
            let value = format!(
                "matrix({} {} {} {} {} {})",
                self.num(a),
                self.num(b),
                self.num(c),
                self.num(d),
                self.num(e),
                self.num(f)
            );
            self.attribute("transform", &value);
        }
        if let Some(clip) = &node.clip_path
            // A `clip-path` pointing at nothing hides the element entirely in
            // every conforming renderer, so a dangling reference exported
            // verbatim would turn a lossy import into a blank file.
            && scene.clips.iter().any(|c| &c.id == clip)
        {
            let value = format!("url(#{clip})");
            self.attribute("clip-path", &value);
        }
        self.style(&node.style, parent);

        match &node.kind {
            NodeKind::Path(_) => self.out.push_str("/>\n"),
            NodeKind::Group(children) => {
                self.out.push_str(">\n");
                for &child in children {
                    self.node(scene, child, &node.style, depth + 1);
                }
                self.indent(depth);
                self.out.push_str("</g>\n");
            }
        }
    }

    /// The presentation attributes a reimport would not recompute by itself.
    ///
    /// An inheriting property only needs writing when it differs from the
    /// parent's computed value; one that does not inherit needs writing when it
    /// differs from its initial value, and writing it against the *parent*
    /// there would silently drop a child that repeats its parent's `opacity`.
    fn style(&mut self, style: &Style, parent: &Style) {
        let initial = Style::initial();
        for (name, inherits, value) in style.properties() {
            let implied = if inherits { parent } else { &initial };
            if implied.get(name) != Some(value) {
                self.attribute(name, value);
            }
        }
    }

    fn path_data(&self, path: &Path) -> String {
        match self.precision {
            None => path_data::write(path.elements()),
            Some(decimals) => {
                let els: Vec<PathEl> = path
                    .elements()
                    .iter()
                    .map(|el| round_el(*el, decimals))
                    .collect();
                path_data::write(&els)
            }
        }
    }

    fn gradient(&mut self, gradient: &Gradient) {
        self.out.push_str("    <");
        let name = match gradient.kind {
            GradientKind::Linear { .. } => "linearGradient",
            GradientKind::Radial { .. } => "radialGradient",
        };
        self.out.push_str(name);
        self.attribute("id", &gradient.id);
        match gradient.kind {
            GradientKind::Linear { start, end } => {
                for (name, v) in [
                    ("x1", start.x),
                    ("y1", start.y),
                    ("x2", end.x),
                    ("y2", end.y),
                ] {
                    let value = self.num(v);
                    self.attribute(name, &value);
                }
            }
            GradientKind::Radial {
                center,
                radius,
                focus,
            } => {
                for (name, v) in [
                    ("cx", center.x),
                    ("cy", center.y),
                    ("r", radius),
                    ("fx", focus.x),
                    ("fy", focus.y),
                ] {
                    let value = self.num(v);
                    self.attribute(name, &value);
                }
            }
        }
        if gradient.user_space {
            self.attribute("gradientUnits", "userSpaceOnUse");
        }
        if gradient.transform != Affine::IDENTITY {
            let [a, b, c, d, e, f] = gradient.transform.as_coeffs();
            let value = format!(
                "matrix({} {} {} {} {} {})",
                self.num(a),
                self.num(b),
                self.num(c),
                self.num(d),
                self.num(e),
                self.num(f)
            );
            self.attribute("gradientTransform", &value);
        }
        if gradient.spread != "pad" {
            self.attribute("spreadMethod", &gradient.spread);
        }
        self.out.push_str(">\n");
        for stop in &gradient.stops {
            self.out.push_str("      <stop");
            let offset = self.num(stop.offset);
            self.attribute("offset", &offset);
            self.attribute("stop-color", &stop.color);
            self.attribute("stop-opacity", &stop.opacity);
            self.out.push_str("/>\n");
        }
        self.out.push_str("    </");
        self.out.push_str(name);
        self.out.push_str(">\n");
    }

    fn clip(&mut self, clip: &Clip) {
        self.out.push_str("    <clipPath");
        self.attribute("id", &clip.id);
        self.out.push_str(">\n");
        for path in &clip.paths {
            self.out.push_str("      <path");
            let d = self.path_data(path);
            self.attribute("d", &d);
            self.out.push_str("/>\n");
        }
        self.out.push_str("    </clipPath>\n");
    }
}

/// `v` rounded to `decimals` decimal places.
fn round(v: f64, decimals: u8) -> f64 {
    let scale = 10f64.powi(i32::from(decimals));
    let scaled = v * scale;
    // A coordinate large enough to overflow the scaling has no decimals left to
    // round anyway, and rounding it would produce an infinity in the file.
    if scaled.is_finite() {
        scaled.round() / scale
    } else {
        v
    }
}

fn round_el(el: PathEl, decimals: u8) -> PathEl {
    let p = |p: Point| Point::new(round(p.x, decimals), round(p.y, decimals));
    match el {
        PathEl::MoveTo(a) => PathEl::MoveTo(p(a)),
        PathEl::LineTo(a) => PathEl::LineTo(p(a)),
        PathEl::QuadTo(a, b) => PathEl::QuadTo(p(a), p(b)),
        PathEl::CurveTo(a, b, c) => PathEl::CurveTo(p(a), p(b), p(c)),
        PathEl::ClosePath => PathEl::ClosePath,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::import;
    use crate::scene::GradientKind;

    /// One document exercising every feature import and export both claim:
    /// each shape, nested transforms, inherited and overridden style, a
    /// gradient with an href chain, a radial gradient, a clip and a `use`.
    const DOCUMENT: &str = r##"<svg xmlns="http://www.w3.org/2000/svg"
        xmlns:xlink="http://www.w3.org/1999/xlink"
        width="200" height="100" viewBox="0 0 200 100">
      <defs>
        <linearGradient id="base">
          <stop offset="0" stop-color="#ff0000"/>
          <stop offset="0.5" stop-color="rgb(0, 255, 0)" stop-opacity="0.25"/>
          <stop offset="1" stop-color="blue"/>
        </linearGradient>
        <linearGradient id="slanted" xlink:href="#base" x1="0.1" y1="0.2" x2="0.9"
          gradientTransform="rotate(30 1 2)"/>
        <radialGradient id="glow" cx="30" cy="40" r="25" fx="35" gradientUnits="userSpaceOnUse"
          spreadMethod="reflect">
          <stop offset="0" stop-color="white"/>
          <stop offset="1" stop-color="black"/>
        </radialGradient>
        <clipPath id="window">
          <rect x="5" y="5" width="90" height="90" rx="8"/>
        </clipPath>
        <g id="stamp"><circle cx="3" cy="3" r="2.5"/><path d="M0 0 Q 4 8 8 0 T 16 0"/></g>
      </defs>
      <g transform="translate(10 20) rotate(15)" fill="url(#slanted)" opacity="0.75">
        <rect x="1.5" y="2.25" width="30" height="17" rx="3" ry="6"/>
        <ellipse cx="40" cy="10" rx="9" ry="4"/>
        <g transform="scale(2 3) skewX(12)">
          <polygon points="0,0 10,0 10,10 0,10" fill="url(#glow)" opacity="0.75"/>
          <polyline points="0,0 3,4 9,-2" stroke="green" stroke-width="0.5"/>
          <line x1="-1e2" y1="0.001" x2="1234567.5" y2="-9"/>
        </g>
        <path d="M0 0 C 1 2 3 4 5 6 S 9 8 11 10 A 5 4 30 1 0 20 25 z" clip-path="url(#window)"/>
      </g>
      <use xlink:href="#stamp" x="150" y="40" fill="teal"/>
    </svg>"##;

    fn geometry(source: &str) -> Vec<Vec<PathEl>> {
        import(source)
            .expect("well-formed")
            .flatten()
            .iter()
            .map(|p| p.elements().to_vec())
            .collect()
    }

    /// Element-wise, because a bounding box would compare transformed points
    /// that were *summed* differently and drift by an ulp for reasons that have
    /// nothing to do with export.
    fn difference(a: &[Vec<PathEl>], b: &[Vec<PathEl>]) -> f64 {
        assert_eq!(a.len(), b.len(), "different number of paths");
        let mut worst: f64 = 0.0;
        for (pa, pb) in a.iter().zip(b) {
            assert_eq!(pa.len(), pb.len(), "different number of elements");
            for (ea, eb) in pa.iter().zip(pb) {
                let (xs, ys) = (points(*ea), points(*eb));
                assert_eq!(xs.len(), ys.len(), "different element kinds");
                for (p, q) in xs.iter().zip(&ys) {
                    worst = worst.max((p.x - q.x).abs()).max((p.y - q.y).abs());
                }
            }
        }
        worst
    }

    /// The largest coordinate in play, which is what a tolerance on this
    /// geometry has to be relative to: an absolute 1e-9 is below one ulp of a
    /// coordinate of 1e9, so no correct implementation could meet it.
    fn magnitude(paths: &[Vec<PathEl>]) -> f64 {
        paths
            .iter()
            .flatten()
            .flat_map(|el| points(*el))
            .fold(1.0f64, |m, p| m.max(p.x.abs()).max(p.y.abs()))
    }

    fn points(el: PathEl) -> Vec<Point> {
        match el {
            PathEl::MoveTo(a) | PathEl::LineTo(a) => vec![a],
            PathEl::QuadTo(a, b) => vec![a, b],
            PathEl::CurveTo(a, b, c) => vec![a, b, c],
            PathEl::ClosePath => Vec::new(),
        }
    }

    #[test]
    fn a_round_trip_reproduces_geometry_exactly() {
        let scene = import(DOCUMENT).unwrap();
        assert!(scene.losses.is_empty(), "{:?}", scene.losses);
        let once = write(&scene);
        let twice = write(&import(&once).unwrap());
        // Bit-exact, not to 1e-9: the default precision is the shortest text
        // that reads back to the same f64, so a second pass has nothing left to
        // lose. Comparing the documents catches style and defs drift too.
        assert_eq!(once, twice);
        assert_eq!(difference(&geometry(DOCUMENT), &geometry(&once)), 0.0);
    }

    #[test]
    fn a_rounded_round_trip_stays_within_its_precision() {
        let scene = import(DOCUMENT).unwrap();
        let rounded = write_with_precision(&scene, 9);
        // The criterion for #55, relative. Rounding is applied to a node's own
        // coordinates *and* to its group transforms, and a group transform
        // multiplies: half an ulp of the ninth decimal of a matrix coefficient
        // becomes 7e-4 of world space once a coordinate of 1.2e6 goes through
        // it. That is 6e-10 relative, and an absolute 1e-9 there is a bound
        // that no fixed-decimal writer can meet.
        let (before, after) = (geometry(DOCUMENT), geometry(&rounded));
        assert!(difference(&before, &after) <= 1e-9 * magnitude(&before));
        // Rounding twice is idempotent, so the file stops changing.
        assert_eq!(rounded, write_with_precision(&import(&rounded).unwrap(), 9));
        assert!(write_with_precision(&scene, 3).len() < rounded.len());
    }

    #[test]
    fn the_output_reparses_as_svg() {
        let out = write(&import(DOCUMENT).unwrap());
        assert!(out.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg "));
        assert!(out.contains("xmlns=\"http://www.w3.org/2000/svg\""));
        assert!(out.contains("version=\"1.1\""));
        assert!(out.contains("width=\"200\" height=\"100\" viewBox=\"0 0 200 100\""));
        // No element outside the SVG 1.1 subset we claim to write.
        let allowed = [
            "svg",
            "defs",
            "g",
            "path",
            "clipPath",
            "stop",
            "linearGradient",
            "radialGradient",
        ];
        for tag in out.split('<').skip(1).filter(|s| !s.starts_with('/')) {
            let name = tag.split([' ', '>', '/']).next().unwrap();
            assert!(
                allowed.contains(&name) || name.starts_with('?'),
                "wrote <{name}>"
            );
        }
    }

    #[test]
    fn gradients_and_clips_keep_their_references() {
        let scene = import(&write(&import(DOCUMENT).unwrap())).unwrap();
        assert_eq!(scene.gradients.len(), 3);
        assert_eq!(scene.clips.len(), 1);
        let glow = scene.gradients.iter().find(|g| g.id == "glow").unwrap();
        assert!(glow.user_space && glow.spread == "reflect");
        assert!(matches!(
            glow.kind,
            GradientKind::Radial { center, radius, focus }
                if center.x == 30.0 && radius == 25.0 && focus.x == 35.0 && focus.y == 40.0
        ));
        assert_eq!(glow.stops.len(), 2);
        // The href chain was resolved on the way in, so the second pass sees
        // the stops directly and the reference no longer has to exist.
        let slanted = scene.gradients.iter().find(|g| g.id == "slanted").unwrap();
        assert_eq!(slanted.stops.len(), 3);
        assert_eq!(slanted.stops[1].opacity, "0.25");
        assert!(scene.flatten().len() > 1);
        assert!(
            scene
                .nodes
                .iter()
                .any(|(_, n)| n.clip_path.as_deref() == Some("window"))
        );
    }

    #[test]
    fn a_property_repeating_its_parents_value_survives_the_trip() {
        // `opacity` does not inherit, so writing only the parent-differing
        // properties would drop the child's copy and change what it means.
        let source = "<svg xmlns=\"http://www.w3.org/2000/svg\"><g opacity=\"0.5\">\
                      <path d=\"M0 0L1 1\" opacity=\"0.5\" fill=\"red\"/></g></svg>";
        let out = write(&import(source).unwrap());
        let scene = import(&out).unwrap();
        let leaf = scene.flatten();
        assert_eq!(leaf.len(), 1);
        let node = scene
            .nodes
            .iter()
            .find(|(_, n)| matches!(n.kind, NodeKind::Path(_)))
            .unwrap()
            .1;
        assert_eq!(node.style.get("opacity"), Some("0.5"));
        assert_eq!(node.style.get("fill"), Some("red"));
        // Inherited and unchanged, so it is written once on the group.
        assert_eq!(out.matches("fill=\"red\"").count(), 1);
    }

    #[test]
    fn attribute_values_are_escaped() {
        let source = "<svg xmlns=\"http://www.w3.org/2000/svg\">\
                      <path d=\"M0 0L1 1\" fill=\"&quot;a&amp;b&lt;c&quot;\"/></svg>";
        let out = write(&import(source).unwrap());
        assert!(out.contains("fill=\"&quot;a&amp;b&lt;c&quot;\""), "{out}");
        assert_eq!(
            import(&out)
                .unwrap()
                .nodes
                .iter()
                .next()
                .unwrap()
                .1
                .style
                .get("fill"),
            Some("\"a&b<c\"")
        );
    }

    #[test]
    fn a_dangling_clip_reference_is_not_written_back() {
        // A clip-path pointing at nothing hides the element in a conforming
        // renderer, so exporting it verbatim would turn a reported loss into a
        // blank file.
        let source = "<svg xmlns=\"http://www.w3.org/2000/svg\">\
                      <path d=\"M0 0L1 1\" clip-path=\"url(#gone)\"/></svg>";
        let scene = import(source).unwrap();
        assert_eq!(scene.losses.len(), 1);
        assert!(!write(&scene).contains("clip-path"));
    }
}
