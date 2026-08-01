//! The `d` attribute: the SVG path data grammar.
//!
//! [`parse`] turns path data into `Vec<PathEl>`; [`write`] turns it back into a
//! string that parses to the same elements.
//!
//! # Why parsing does not fail
//!
//! SVG 1.1 F.4 and SVG 2 both require a path with malformed data to render
//! *everything up to the error* rather than nothing, and files in the wild
//! depend on it -- a truncated export or one bad arc flag still draws. So
//! [`parse`] returns the elements it committed alongside the error that stopped
//! it, and the caller decides whether to report the error at all. Only whole
//! commands are committed: a command whose argument list runs out halfway
//! contributes nothing, which is what the spec means by "the path up to the
//! point of the error".
//!
//! # Compact notation is the normal case
//!
//! Every separator in the grammar is optional wherever the result stays
//! unambiguous, and every real generator exploits that. `M0 0L1 1` runs a
//! command letter straight onto the previous number, `.5.5` is two numbers with
//! nothing between them, `1-2` uses the sign as the separator, and `1e3` may
//! end a number that `-1e-3` continues. The number scanner therefore stops at
//! the first byte that cannot extend the number it is already reading, rather
//! than at a delimiter.
//!
//! # Arcs
//!
//! `A`/`a` goes through [`PathEl::arc`], so nothing downstream ever sees a
//! fourth segment kind. The `x-axis-rotation` argument is in degrees here and
//! radians there, which is the only unit conversion in this module.

use core::fmt;
use hane_geom::{PathEl, Point, Vec2};

/// What stopped the parse, and where.
///
/// The position is a byte offset into the `d` string rather than the line and
/// column [`xml::Error`](crate::xml::Error) carries: attribute-value
/// normalisation has already turned every newline in `d` into a space by the
/// time it reaches here, so there is only ever one line to point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// A description of the failure, without position.
    pub message: String,
    /// Byte offset into the path data where the parser stopped.
    pub offset: usize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "offset {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for Error {}

/// Parse path data into path elements.
///
/// Returns the elements that parsed and, when the data is malformed, the error
/// that stopped it -- both, because the spec requires the prefix to render. An
/// empty or all-whitespace `d` is not an error and yields no elements.
///
/// Malformed input never panics.
pub fn parse(d: &str) -> (Vec<PathEl>, Option<Error>) {
    let mut parser = Parser {
        src: d,
        pos: 0,
        els: Vec::new(),
        cur: Point::ORIGIN,
        start: Point::ORIGIN,
        prev_cubic: None,
        prev_quad: None,
    };
    let err = parser.run().err();
    (parser.els, err)
}

/// Serialise path elements back to path data.
///
/// Every command is absolute and every number is Rust's shortest round-tripping
/// form, so [`parse`] on the result reproduces the input exactly -- bit for
/// bit, not to a tolerance.
///
/// ponytail: no minification -- absolute commands, a space before every number.
/// Relative commands and dropped separators cut real files by a third or so;
/// add it when export size matters, not before, because it is exactly where
/// serialisers introduce round-off.
pub fn write(els: &[PathEl]) -> String {
    let mut out = String::new();
    for el in els {
        match *el {
            PathEl::MoveTo(p) => push(&mut out, 'M', &[p]),
            PathEl::LineTo(p) => push(&mut out, 'L', &[p]),
            PathEl::QuadTo(c, p) => push(&mut out, 'Q', &[c, p]),
            PathEl::CurveTo(c0, c1, p) => push(&mut out, 'C', &[c0, c1, p]),
            PathEl::ClosePath => out.push('Z'),
        }
    }
    out
}

fn push(out: &mut String, cmd: char, points: &[Point]) {
    out.push(cmd);
    for p in points {
        for v in [p.x, p.y] {
            out.push(' ');
            // f64's Display is the shortest string that parses back to the same
            // bits, which is the whole round-trip guarantee. Formatting to a
            // fixed number of digits would not round-trip.
            out.push_str(&v.to_string());
        }
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    els: Vec<PathEl>,
    /// The current point.
    cur: Point,
    /// Where the current subpath began, which `Z` returns to.
    start: Point,
    /// The second control point of the previous command, when it was a cubic,
    /// and `None` otherwise -- `S` reflects it and falls back to the current
    /// point exactly when it is `None`.
    prev_cubic: Option<Point>,
    /// The same for `T` and the previous quadratic's control point.
    prev_quad: Option<Point>,
}

impl Parser<'_> {
    fn run(&mut self) -> Result<(), Error> {
        self.skip_wsp();
        if self.pos == self.src.len() {
            return Ok(());
        }
        // F.4: data that does not start with a moveto is in error from the
        // first character, so nothing renders.
        if !matches!(self.peek(), Some(b'M' | b'm')) {
            return Err(self.error("path data must begin with a moveto"));
        }
        loop {
            self.skip_wsp();
            let Some(cmd) = self.peek() else {
                return Ok(());
            };
            if !cmd.is_ascii_alphabetic() {
                return Err(self.error("expected a command letter"));
            }
            self.pos += 1;
            self.command(cmd)?;
        }
    }

    fn command(&mut self, cmd: u8) -> Result<(), Error> {
        // Case selects the frame: uppercase absolute, lowercase relative.
        let relative = cmd.is_ascii_lowercase();
        let (mut cubic, mut quad) = (None, None);
        let mut first = true;
        loop {
            match cmd.to_ascii_uppercase() {
                b'Z' => {
                    self.els.push(PathEl::ClosePath);
                    // The subpath start becomes current, so a following
                    // relative command measures from there and not from the
                    // last point drawn.
                    self.cur = self.start;
                }
                b'M' => {
                    let p = self.point(relative)?;
                    // Implicit repetition after a moveto is a *lineto*, not
                    // more movetos -- the one place repetition changes command.
                    if first {
                        self.els.push(PathEl::MoveTo(p));
                        self.start = p;
                    } else {
                        self.els.push(PathEl::LineTo(p));
                    }
                    self.cur = p;
                }
                b'L' => {
                    let p = self.point(relative)?;
                    self.els.push(PathEl::LineTo(p));
                    self.cur = p;
                }
                b'H' => {
                    let x = self.coord(relative, self.cur.x)?;
                    let p = Point::new(x, self.cur.y);
                    self.els.push(PathEl::LineTo(p));
                    self.cur = p;
                }
                b'V' => {
                    let y = self.coord(relative, self.cur.y)?;
                    let p = Point::new(self.cur.x, y);
                    self.els.push(PathEl::LineTo(p));
                    self.cur = p;
                }
                b'C' => {
                    let c0 = self.point(relative)?;
                    let c1 = self.point(relative)?;
                    let p = self.point(relative)?;
                    self.els.push(PathEl::CurveTo(c0, c1, p));
                    (self.cur, cubic) = (p, Some(c1));
                }
                b'S' => {
                    let c0 = reflect(self.cur, self.prev_cubic);
                    let c1 = self.point(relative)?;
                    let p = self.point(relative)?;
                    self.els.push(PathEl::CurveTo(c0, c1, p));
                    (self.cur, cubic) = (p, Some(c1));
                }
                b'Q' => {
                    let c = self.point(relative)?;
                    let p = self.point(relative)?;
                    self.els.push(PathEl::QuadTo(c, p));
                    (self.cur, quad) = (p, Some(c));
                }
                b'T' => {
                    let c = reflect(self.cur, self.prev_quad);
                    let p = self.point(relative)?;
                    self.els.push(PathEl::QuadTo(c, p));
                    (self.cur, quad) = (p, Some(c));
                }
                b'A' => {
                    let radii = Vec2::new(self.number()?, self.number()?);
                    let rotation = self.number()?;
                    let large_arc = self.flag()?;
                    let sweep = self.flag()?;
                    let p = self.point(relative)?;
                    self.els.extend(PathEl::arc(
                        self.cur,
                        radii,
                        rotation.to_radians(),
                        large_arc,
                        sweep,
                        p,
                    ));
                    self.cur = p;
                }
                _ => return Err(self.err_at(self.pos - 1, "unknown command")),
            }
            // Reflection looks one command back, so the state is replaced
            // wholesale after every command -- an `S` following an `L` must
            // reflect nothing even though a `C` came before the `L`.
            (self.prev_cubic, self.prev_quad) = (cubic, quad);
            first = false;
            // `Z` takes no arguments, so it never repeats.
            if cmd.eq_ignore_ascii_case(&b'Z') || !self.more_args() {
                return Ok(());
            }
        }
    }

    /// True when another argument group follows, leaving the position untouched
    /// when it does not -- a separator consumed here would hide a stray comma
    /// before the next command letter.
    fn more_args(&mut self) -> bool {
        let save = self.pos;
        self.skip_sep();
        let more = matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'+' | b'-'));
        if !more {
            self.pos = save;
        }
        more
    }

    fn point(&mut self, relative: bool) -> Result<Point, Error> {
        let x = self.coord(relative, self.cur.x)?;
        let y = self.coord(relative, self.cur.y)?;
        Ok(Point::new(x, y))
    }

    fn coord(&mut self, relative: bool, origin: f64) -> Result<f64, Error> {
        let v = self.number()?;
        Ok(if relative { origin + v } else { v })
    }

    /// A flag argument, which is a *single* character: `a1 1 0 011 10` is seven
    /// valid arguments, and treating flags as ordinary numbers would read `011`
    /// as one.
    fn flag(&mut self) -> Result<bool, Error> {
        self.skip_sep();
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                Ok(false)
            }
            Some(b'1') => {
                self.pos += 1;
                Ok(true)
            }
            _ => Err(self.error("arc flag must be 0 or 1")),
        }
    }

    fn number(&mut self) -> Result<f64, Error> {
        self.skip_sep();
        let start = self.pos;
        self.eat_sign();
        let int_digits = self.eat_digits();
        let frac_digits = if self.peek() == Some(b'.') {
            self.pos += 1;
            self.eat_digits()
        } else {
            0
        };
        if int_digits == 0 && frac_digits == 0 {
            self.pos = start;
            return Err(self.error("expected a number"));
        }
        // An exponent is taken only when it is complete, so the `e` of `1exp`
        // is left for the caller to reject rather than swallowed into a number
        // that then fails to parse.
        let mantissa_end = self.pos;
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            self.eat_sign();
            if self.eat_digits() == 0 {
                self.pos = mantissa_end;
            }
        }
        // Every byte consumed above is ASCII, so the range is on char
        // boundaries; `get` rather than indexing keeps that a fact this
        // function does not have to be trusted about.
        let text = self.src.get(start..self.pos).unwrap_or("");
        let value: f64 = text
            .parse()
            .map_err(|_| self.err_at(start, "expected a number"))?;
        // `1e400` parses to infinity, which would poison every bounding box and
        // transform downstream. Treated as the malformedness it is.
        if !value.is_finite() {
            self.pos = start;
            return Err(self.error("number out of range"));
        }
        Ok(value)
    }

    fn eat_sign(&mut self) {
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
    }

    fn eat_digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn skip_wsp(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')) {
            self.pos += 1;
        }
    }

    /// comma-wsp: whitespace around at most one comma, all of it optional.
    fn skip_sep(&mut self) {
        self.skip_wsp();
        if self.peek() == Some(b',') {
            self.pos += 1;
            self.skip_wsp();
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn error(&self, message: &str) -> Error {
        self.err_at(self.pos, message)
    }

    fn err_at(&self, offset: usize, message: &str) -> Error {
        Error {
            message: message.to_string(),
            offset,
        }
    }
}

/// The control point `S` and `T` infer: the previous one mirrored through the
/// current point, or the current point itself when the previous command was not
/// the matching kind.
fn reflect(cur: Point, prev: Option<Point>) -> Point {
    match prev {
        Some(p) => cur + (cur - p),
        None => cur,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    fn ok(d: &str) -> Vec<PathEl> {
        let (els, err) = parse(d);
        assert_eq!(err, None, "unexpected error parsing {d:?}");
        els
    }

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    #[test]
    fn every_command_absolute() {
        let els = ok("M1 1 L2 2 H3 V4 C5 5 6 6 7 7 S8 8 9 9 Q10 10 11 11 T12 12 Z");
        assert_eq!(
            els,
            vec![
                PathEl::MoveTo(p(1.0, 1.0)),
                PathEl::LineTo(p(2.0, 2.0)),
                PathEl::LineTo(p(3.0, 2.0)),
                PathEl::LineTo(p(3.0, 4.0)),
                PathEl::CurveTo(p(5.0, 5.0), p(6.0, 6.0), p(7.0, 7.0)),
                PathEl::CurveTo(p(8.0, 8.0), p(8.0, 8.0), p(9.0, 9.0)),
                PathEl::QuadTo(p(10.0, 10.0), p(11.0, 11.0)),
                PathEl::QuadTo(p(12.0, 12.0), p(12.0, 12.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn relative_commands_match_their_absolute_twins() {
        let absolute = ok("M1 1 L3 4 H5 V6 C7 7 8 8 9 9 S10 10 11 11 Q12 12 13 13 T14 14 Z");
        let relative = ok("m1 1 l2 3 h2 v2 c2 1 3 2 4 3 s1 1 2 2 q1 1 2 2 t1 1 z");
        assert_eq!(absolute, relative);
    }

    #[test]
    fn relative_after_close_starts_from_the_subpath_start() {
        // `z` moves the current point back to (1,1), so `m` lands at (3,3) --
        // not at (12,12) where the line ended.
        let els = ok("M1 1 L10 10 z m2 2");
        assert_eq!(els[3], PathEl::MoveTo(p(3.0, 3.0)));
    }

    #[test]
    fn implicit_repetition() {
        // Repeated moveto pairs are linetos; every other command just repeats.
        assert_eq!(
            ok("M1 1 2 2 3 3"),
            vec![
                PathEl::MoveTo(p(1.0, 1.0)),
                PathEl::LineTo(p(2.0, 2.0)),
                PathEl::LineTo(p(3.0, 3.0)),
            ]
        );
        assert_eq!(ok("M0 0 L1 1 2 2 3 3").len(), 4);
        assert_eq!(ok("M0 0 C1 1 2 2 3 3 4 4 5 5 6 6").len(), 3);
    }

    #[test]
    fn smooth_curves_reflect_the_previous_control_point() {
        // C ends at (3,3) with second control (2,2); the reflection is (4,4).
        let els = ok("M0 0 C1 1 2 2 3 3 S5 5 6 6");
        assert_eq!(
            els[2],
            PathEl::CurveTo(p(4.0, 4.0), p(5.0, 5.0), p(6.0, 6.0))
        );
        // Repeated S reflects the previous S's own control point.
        let els = ok("M0 0 C1 1 2 2 3 3 S5 5 6 6 S9 9 10 10");
        assert_eq!(
            els[3],
            PathEl::CurveTo(p(7.0, 7.0), p(9.0, 9.0), p(10.0, 10.0))
        );
        // T after Q likewise.
        let els = ok("M0 0 Q1 1 2 2 T4 4");
        assert_eq!(els[2], PathEl::QuadTo(p(3.0, 3.0), p(4.0, 4.0)));
    }

    #[test]
    fn smooth_after_a_foreign_command_uses_the_current_point() {
        // The line between them clears the reflection, so the first control
        // coincides with the current point (5,5).
        let els = ok("M0 0 C1 1 2 2 3 3 L5 5 S8 8 9 9");
        assert_eq!(
            els[3],
            PathEl::CurveTo(p(5.0, 5.0), p(8.0, 8.0), p(9.0, 9.0))
        );
        // S must not reflect a *quadratic* control point, nor T a cubic one.
        let els = ok("M0 0 Q1 1 2 2 S8 8 9 9");
        assert_eq!(
            els[2],
            PathEl::CurveTo(p(2.0, 2.0), p(8.0, 8.0), p(9.0, 9.0))
        );
        let els = ok("M0 0 C1 1 2 2 3 3 T9 9");
        assert_eq!(els[2], PathEl::QuadTo(p(3.0, 3.0), p(9.0, 9.0)));
    }

    #[test]
    fn arcs_go_through_the_geometry_routine() {
        let from = p(0.0, 0.0);
        let to = p(2.0, 0.0);
        let expected: Vec<PathEl> = PathEl::arc(
            from,
            Vec2::new(1.0, 1.0),
            30f64.to_radians(),
            true,
            false,
            to,
        )
        .collect();
        let els = ok("M0 0 A1 1 30 1 0 2 0");
        assert_eq!(els[0], PathEl::MoveTo(from));
        assert_eq!(&els[1..], &expected[..]);
        assert!(!expected.is_empty(), "the fixture must produce segments");
    }

    #[test]
    fn arc_flags_may_be_single_digits_without_separators() {
        // `0 1` written as `01`, the compact form Illustrator emits.
        assert_eq!(ok("M0 0 A1 1 0 011 10"), ok("M0 0 A1 1 0 0 1 1 10"));
    }

    #[test]
    fn compact_notation() {
        // A command letter needs no separator before it.
        assert_eq!(
            ok("M0 0L1 1"),
            vec![PathEl::MoveTo(p(0.0, 0.0)), PathEl::LineTo(p(1.0, 1.0))]
        );
        // `.5.5` is two numbers: the second `.` cannot extend the first.
        assert_eq!(ok("M.5.5")[0], PathEl::MoveTo(p(0.5, 0.5)));
        // A sign separates as well as signs.
        assert_eq!(ok("M1-2")[0], PathEl::MoveTo(p(1.0, -2.0)));
        assert_eq!(ok("M0 0l1-2-3-4").len(), 3);
        // Exponents, including one immediately followed by a signed number.
        assert_eq!(ok("M1e3 1E-2")[0], PathEl::MoveTo(p(1000.0, 0.01)));
        assert_eq!(ok("M1e3-1e-2")[0], PathEl::MoveTo(p(1000.0, -0.01)));
        assert_eq!(ok("M+1.5,+2.5")[0], PathEl::MoveTo(p(1.5, 2.5)));
        // Commas, tabs and newlines are all just separators.
        assert_eq!(ok("\tM 0,0\n\tL\r1 ,\t1 \n"), ok("M0 0L1 1"));
    }

    #[test]
    fn malformed_data_keeps_what_parsed() {
        // The `L` is missing its y: the moveto and the first line survive, the
        // half-read lineto does not.
        let (els, err) = parse("M0 0 L1 1 L2");
        assert_eq!(
            els,
            vec![PathEl::MoveTo(p(0.0, 0.0)), PathEl::LineTo(p(1.0, 1.0))]
        );
        assert_eq!(err.map(|e| e.offset), Some(12));

        // An unknown command letter stops the parse where it stands.
        let (els, err) = parse("M0 0 L1 1 X2 2");
        assert_eq!(els.len(), 2);
        assert!(err.is_some());

        // A bad arc flag drops the arc but keeps everything before it.
        let (els, err) = parse("M0 0 L1 1 A1 1 0 2 1 3 3");
        assert_eq!(els.len(), 2);
        assert!(err.unwrap().message.contains("flag"));

        // Not starting with a moveto renders nothing at all.
        let (els, err) = parse("L1 1");
        assert!(els.is_empty());
        assert_eq!(err.map(|e| e.offset), Some(0));

        // Infinities never reach the geometry.
        let (els, err) = parse("M0 0 L1e400 0");
        assert_eq!(els.len(), 1);
        assert!(err.unwrap().message.contains("range"));

        // Empty and whitespace-only data are legal and draw nothing.
        assert_eq!(parse(""), (Vec::new(), None));
        assert_eq!(parse("   \n "), (Vec::new(), None));
    }

    #[test]
    fn malformed_data_never_panics() {
        // Bytes that are not ASCII, truncations mid-number, lone separators.
        for d in [
            "M",
            "M0",
            "M0,",
            "M.",
            "M-",
            "M0 0e",
            "M0 0 A",
            "M0 0 A1",
            "Z",
            "M0 0 Zz",
            "M0 0 L\u{e9}",
            "M0 0 L1 1 \u{1f600}",
            "M0 0,,1 1",
            "m0 0 h",
            "M0 0 A1 1 0",
        ] {
            let _ = parse(d);
        }
    }

    #[test]
    fn round_trips_through_write() {
        let d = "M1 1 C2 2 3 3 4 4 Q5 5 6 6 A2 3 40 1 0 7 8 Z";
        let els = ok(d);
        assert_eq!(ok(&write(&els)), els);
    }

    #[test]
    fn round_trips_on_random_paths() {
        check("path data round-trip", 500, random_path, |els| {
            let (reparsed, err) = parse(&write(els));
            err.is_none() && reparsed == *els
        });
    }

    /// A path that starts with a moveto, so it is valid data to begin with, and
    /// exercises every element kind at magnitudes from `2^-40` up.
    fn random_path(rng: &mut Rng) -> Vec<PathEl> {
        let mut els = vec![PathEl::MoveTo(rng.point())];
        for _ in 0..rng.below(8) {
            els.push(match rng.below(5) {
                0 => PathEl::MoveTo(rng.point()),
                1 => PathEl::LineTo(rng.point()),
                2 => PathEl::QuadTo(rng.point(), rng.point()),
                3 => PathEl::CurveTo(rng.point(), rng.point(), rng.point()),
                _ => PathEl::ClosePath,
            });
        }
        els
    }
}
