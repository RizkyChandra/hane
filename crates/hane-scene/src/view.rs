//! The screen <-> document mapping: pan, zoom and rotation.
//!
//! Ported from `sable/app/src/view_transform.hpp`, which is round-trip tested
//! for the same reason this is: a transform that silently drops the rotation on
//! one of the two paths still looks nearly right at small angles, and is only
//! ever caught by asserting that going there and back is the identity.
//!
//! Screen y grows downwards, so a positive rotation reads as clockwise. Pan is
//! the screen position of document point `(0, 0)`; zoom scales about that
//! origin and rotation turns about it. What the artist sees turning is the
//! whole mapping turning about the origin -- [`View::rotate_about`] and
//! [`View::zoom_about`] both re-anchor the pan afterwards so the point under
//! the cursor stays put.

use hane_geom::{Affine, Point, Vec2};

/// Full turn, in radians.
const TAU: f64 = core::f64::consts::TAU;

/// A viewport's pan, zoom and rotation.
///
/// The three fields are private because they are not independent: the zoom is
/// held inside [`View::MIN_ZOOM`]`..=`[`View::MAX_ZOOM`] and every field is
/// held finite, which is what keeps [`View::matrix`] invertible and stops a
/// NaN wheel delta from poisoning every later hit test.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pan: Vec2,
    zoom: f64,
    rotation: f64,
}

impl View {
    /// The furthest out the UI may zoom.
    ///
    /// Both limits are UI policy rather than a numeric bound -- f64 has decades
    /// of headroom past either -- but they are the guard that keeps the
    /// transform non-singular, so they are enforced, not advisory.
    pub const MIN_ZOOM: f64 = 1e-4;

    /// The closest in the UI may zoom.
    pub const MAX_ZOOM: f64 = 1e6;

    /// The identity view: document origin at the screen origin, 1:1, upright.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            rotation: 0.0,
        }
    }

    /// The screen position of document point `(0, 0)`.
    #[must_use]
    pub const fn pan(&self) -> Vec2 {
        self.pan
    }

    /// Screen pixels per document unit.
    #[must_use]
    pub const fn zoom(&self) -> f64 {
        self.zoom
    }

    /// The rotation in radians, folded to `[-pi, pi]`.
    #[must_use]
    pub const fn rotation(&self) -> f64 {
        self.rotation
    }

    /// The document-to-screen transform, for handing to the renderer.
    #[must_use]
    pub fn matrix(&self) -> Affine {
        let (c, s) = (self.rotation.cos(), self.rotation.sin());
        // translate(pan) * rotate(rotation) * scale(zoom), written out: the
        // composition is three multiplies of mostly zeros, and this way the
        // coefficients are visibly the ones `to_document` inverts by hand.
        Affine::new([
            c * self.zoom,
            s * self.zoom,
            -s * self.zoom,
            c * self.zoom,
            self.pan.x,
            self.pan.y,
        ])
    }

    /// Where document point `doc` lands on screen.
    #[must_use]
    pub fn to_screen(&self, doc: Point) -> Point {
        self.matrix() * doc
    }

    /// Which document point is under screen point `screen`.
    ///
    /// Infallible, unlike [`Affine::inverse`], and that is the whole job of the
    /// zoom clamp: a view zoomed to zero is reachable from the UI, and a hit
    /// test that quietly returns NaN coordinates is far harder to diagnose than
    /// one that cannot happen. Inverting by hand rather than through
    /// `matrix().inverse()` also keeps the round trip exact-ish -- it divides by
    /// the zoom instead of by a determinant that is the zoom squared.
    #[must_use]
    pub fn to_document(&self, screen: Point) -> Point {
        let (c, s) = (self.rotation.cos(), self.rotation.sin());
        let d = (screen - self.pan.to_point()) / self.zoom;
        Point::new(d.x * c + d.y * s, -d.x * s + d.y * c)
    }

    /// Slides the view by a screen-space displacement.
    pub fn pan_by(&mut self, delta: Vec2) {
        self.set_pan(self.pan + delta);
    }

    /// Moves the pan so document point `doc` lands on screen point `screen`.
    ///
    /// The primitive behind zoom and rotate about a cursor, and what a
    /// future fit-to-page would use.
    pub fn anchor_at(&mut self, doc: Point, screen: Point) {
        let unpanned = Self {
            pan: Vec2::ZERO,
            ..*self
        };
        self.set_pan(screen - unpanned.to_screen(doc));
    }

    /// Multiplies the zoom by `factor`, keeping the document point under
    /// `screen` under `screen`.
    ///
    /// The factor is clamped *before* anchoring, so at either limit the cursor
    /// point still holds still instead of drifting by the clamped-away part.
    pub fn zoom_about(&mut self, screen: Point, factor: f64) {
        // Rejected rather than clamped, because clamping would turn garbage
        // into a plausible-looking zoom: NaN passes through `clamp` untouched
        // and poisons every later `to_document`, and a negative factor lands on
        // MIN_ZOOM as if the user had asked to zoom all the way out.
        if factor <= 0.0 || !factor.is_finite() {
            return;
        }
        let zoom = (self.zoom * factor).clamp(Self::MIN_ZOOM, Self::MAX_ZOOM);
        if zoom == self.zoom {
            return;
        }
        let doc = self.to_document(screen);
        self.zoom = zoom;
        self.anchor_at(doc, screen);
    }

    /// Turns the view by `delta` radians about a screen point, keeping the
    /// document point under it in place. Passing `-view.rotation()` resets.
    pub fn rotate_about(&mut self, screen: Point, delta: f64) {
        if !delta.is_finite() {
            return;
        }
        let doc = self.to_document(screen);
        self.rotation = fold(self.rotation + delta);
        self.anchor_at(doc, screen);
    }

    /// The single place `pan` is written, so one guard covers every path into
    /// it: an infinite or NaN pan survives forever and takes the whole document
    /// with it, whereas dropping the offending gesture costs one frame.
    fn set_pan(&mut self, pan: Vec2) {
        if pan.is_finite() {
            self.pan = pan;
        }
    }
}

impl Default for View {
    fn default() -> Self {
        Self::new()
    }
}

/// Folds an angle to `[-pi, pi]`, so the status readout says -15 degrees after
/// one step left rather than 345, and a thousand turns cannot drift the
/// rotation off to 1e9 where its cosine is noise.
fn fold(angle: f64) -> f64 {
    angle - (angle / TAU).round() * TAU
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    /// A view within the UI's reach: a pan of a few thousand screen pixels, the
    /// full zoom range sampled geometrically, any rotation.
    ///
    /// Deliberately not `Rng::affine` -- the property under test is that these
    /// three parameters round-trip, and an arbitrary affine is not a view.
    fn view(rng: &mut Rng) -> View {
        let mut v = View::new();
        v.pan_by(Vec2::new(
            rng.unit() * 8192.0 - 4096.0,
            rng.unit() * 8192.0 - 4096.0,
        ));
        v.rotate_about(Point::ORIGIN, rng.unit() * TAU - TAU / 2.0);
        // Log-uniform over the whole zoom range, so 1e-4 and 1e6 are as likely
        // as 1 -- the extremes are where a round trip loses digits.
        let decade = 10f64.powi(i32::try_from(rng.below(11)).unwrap() - 4);
        v.zoom_about(Point::ORIGIN, decade * (1.0 + rng.unit() * 9.0));
        v
    }

    /// Relative, because a point 1e6 units out seen at 1e-4 zoom is 1e10 away
    /// from the pan in screen space and no inverse recovers it to 1e-9
    /// absolute -- that is below an ulp of the intermediate.
    fn close(a: Point, b: Point, scale: f64) -> bool {
        a.distance(b) <= 1e-9 * (1.0 + scale)
    }

    #[test]
    fn screen_to_document_round_trips() {
        check(
            "view round trip",
            2000,
            |rng| {
                let v = view(rng);
                let p = Point::new(rng.unit() * 2e6 - 1e6, rng.unit() * 2e6 - 1e6);
                (v, p)
            },
            |&(v, p)| {
                let back = v.to_document(v.to_screen(p));
                close(back, p, p.to_vec2().length() + v.pan().length() / v.zoom())
            },
        );
    }

    #[test]
    fn document_to_screen_round_trips() {
        check(
            "view inverse round trip",
            2000,
            |rng| {
                let v = view(rng);
                let s = Point::new(rng.unit() * 4096.0, rng.unit() * 4096.0);
                (v, s)
            },
            |&(v, s)| {
                let back = v.to_screen(v.to_document(s));
                close(back, s, s.to_vec2().length() + v.pan().length())
            },
        );
    }

    #[test]
    fn the_matrix_and_the_hand_inverse_agree() {
        check(
            "view matrix inverse",
            2000,
            |rng| {
                let v = view(rng);
                let s = Point::new(rng.unit() * 4096.0, rng.unit() * 4096.0);
                (v, s)
            },
            |&(v, s)| {
                // The clamp is what makes this `unwrap` safe; the point of the
                // test is that it never fires anywhere in the reachable range.
                let inv = v.matrix().inverse().expect("clamped view is invertible");
                close(inv * s, v.to_document(s), (inv * s).to_vec2().length())
            },
        );
    }

    #[test]
    fn zoom_keeps_the_cursor_point_fixed() {
        let cursor = Point::new(731.0, 419.0);
        let mut v = View::new();
        v.rotate_about(cursor, 0.7);
        for step in 0..40 {
            let doc = v.to_document(cursor);
            v.zoom_about(cursor, if step % 3 == 0 { 0.5 } else { 1.25 });
            assert!(close(v.to_screen(doc), cursor, 4096.0), "step {step}");
        }
    }

    #[test]
    fn rotation_about_the_viewport_centre_holds_that_point() {
        let centre = Point::new(640.0, 360.0);
        let mut v = View::new();
        v.zoom_about(centre, 3.0);
        let doc = v.to_document(centre);
        for _ in 0..16 {
            v.rotate_about(centre, TAU / 16.0);
            assert!(close(v.to_screen(doc), centre, 4096.0));
            assert!(v.rotation() >= -TAU / 2.0 && v.rotation() <= TAU / 2.0);
        }
        // Sixteen sixteenths is a full turn, back to upright to within the
        // accumulated rounding of sixteen additions.
        assert!(v.rotation().abs() < 1e-12, "{}", v.rotation());
    }

    #[test]
    fn zoom_limits_hold_and_the_transform_stays_invertible() {
        let cursor = Point::new(300.0, 200.0);
        for factor in [0.01, 100.0] {
            let mut v = View::new();
            for _ in 0..64 {
                v.zoom_about(cursor, factor);
                assert!(v.zoom() >= View::MIN_ZOOM && v.zoom() <= View::MAX_ZOOM);
                assert!(v.matrix().inverse().is_some());
            }
        }
    }

    #[test]
    fn a_non_finite_gesture_is_dropped_rather_than_stored() {
        let mut v = View::new();
        v.zoom_about(Point::ORIGIN, 2.0);
        let before = v;
        for bad in [f64::NAN, f64::INFINITY, -f64::INFINITY] {
            v.rotate_about(Point::new(10.0, 10.0), bad);
            v.pan_by(Vec2::new(bad, 0.0));
        }
        // A zero or negative zoom factor is garbage too, and the one that would
        // otherwise clamp to a legal-looking MIN_ZOOM.
        for bad in [f64::NAN, f64::INFINITY, -f64::INFINITY, -2.0, 0.0] {
            v.zoom_about(Point::new(10.0, 10.0), bad);
        }
        assert_eq!(v, before);
        assert!(v.matrix().is_finite());
    }
}
