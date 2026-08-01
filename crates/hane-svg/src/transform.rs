//! The `transform` attribute grammar.
//!
//! [`parse_transform`] turns a transform list into the single [`Affine`] that
//! has the same effect, because nothing downstream benefits from knowing the
//! list was ever a list -- the composed matrix is what a renderer, a bounding
//! box and an export all want.
//!
//! # Composition order
//!
//! `transform="translate(10) scale(2)"` scales *first*: the list reads
//! outermost-to-innermost, so the leftmost function is applied last to a point.
//! [`Affine`]'s `Mul` applies its right-hand side first, so the list folds
//! left-to-right with `acc = acc * next`, and getting that backwards is the
//! classic SVG bug -- see `composition_applies_the_last_function_first`.

use crate::xml::Error;
use hane_geom::{Affine, Point, Vec2};

/// Parse a `transform` attribute value into the equivalent transform.
///
/// An empty or all-whitespace value is the identity, which is what an author
/// writing `transform=""` means.
///
/// # Errors
///
/// Returns the first malformedness found. Positions are reported as column
/// numbers within the attribute value -- the value has no lines of its own, so
/// [`Error::line`] is always 1 and a caller that knows where the attribute sat
/// in the document adds its own offset.
pub fn parse_transform(value: &str) -> Result<Affine, Error> {
    let mut s = Scanner {
        input: value,
        pos: 0,
    };
    let mut acc = Affine::IDENTITY;
    s.skip_comma_wsp();
    while s.pos < s.input.len() {
        // Right-multiplication, so a function later in the list is applied
        // earlier to the point. Reversing this silently mirrors and misplaces
        // every transformed shape rather than failing.
        acc = acc * s.function()?;
        s.skip_comma_wsp();
    }
    Ok(acc)
}

struct Scanner<'a> {
    input: &'a str,
    pos: usize,
}

impl Scanner<'_> {
    fn err<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        self.err_at(self.pos, message)
    }

    fn err_at<T>(&self, pos: usize, message: impl Into<String>) -> Result<T, Error> {
        Err(Error {
            message: message.into(),
            line: 1,
            // Characters, not bytes, to match `xml::Error`'s columns.
            column: self.input[..pos].chars().count() + 1,
        })
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn skip_wsp(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')) {
            self.pos += 1;
        }
    }

    /// The `comma-wsp` production: whitespace around at most one comma.
    fn skip_comma_wsp(&mut self) {
        self.skip_wsp();
        if self.peek() == Some(b',') {
            self.pos += 1;
            self.skip_wsp();
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), Error> {
        if self.peek() != Some(byte) {
            return self.err(format!("expected `{}`", byte as char));
        }
        self.pos += 1;
        Ok(())
    }

    fn function(&mut self) -> Result<Affine, Error> {
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let name = &self.input[start..self.pos];
        self.skip_wsp();
        self.expect(b'(')?;
        let transform = match name {
            "matrix" => {
                let m = [
                    self.arg()?,
                    self.arg()?,
                    self.arg()?,
                    self.arg()?,
                    self.arg()?,
                    self.arg()?,
                ];
                // The attribute's argument order *is* the storage order, which
                // is the whole reason `Affine` is stored the way it is.
                Affine::new(m)
            }
            "translate" => {
                let tx = self.arg()?;
                Affine::translate(Vec2::new(tx, self.optional_arg()?.unwrap_or(0.0)))
            }
            "scale" => {
                let sx = self.arg()?;
                // A single argument scales both axes: `scale(2)` is `scale(2 2)`,
                // not `scale(2 1)`.
                Affine::scale_non_uniform(sx, self.optional_arg()?.unwrap_or(sx))
            }
            "rotate" => {
                let angle = self.arg()?.to_radians();
                match self.optional_arg()? {
                    // The centre is all-or-nothing: a lone `cx` is malformed,
                    // so `arg` rather than `optional_arg` for `cy`.
                    Some(cx) => Affine::rotate_about(angle, Point::new(cx, self.arg()?)),
                    None => Affine::rotate(angle),
                }
            }
            "skewX" => Affine::skew(self.arg()?.to_radians().tan(), 0.0),
            "skewY" => Affine::skew(0.0, self.arg()?.to_radians().tan()),
            "" => return self.err_at(start, "expected a transform function"),
            other => return self.err_at(start, format!("unknown transform function `{other}`")),
        };
        self.skip_wsp();
        self.expect(b')')?;
        Ok(transform)
    }

    /// A required argument, preceded by an optional separator.
    ///
    /// SVG 1.1's grammar demands a `comma-wsp` between arguments, but `1-2` and
    /// `1.5.5` are unambiguous and every real document relies on it, so the
    /// separator is optional here.
    fn arg(&mut self) -> Result<f64, Error> {
        self.skip_comma_wsp();
        self.number()
    }

    /// An argument that may be absent, for the variadic forms.
    fn optional_arg(&mut self) -> Result<Option<f64>, Error> {
        let start = self.pos;
        self.skip_comma_wsp();
        match self.peek() {
            Some(b'0'..=b'9' | b'.' | b'+' | b'-') => self.number().map(Some),
            _ => {
                // A skipped comma with no number after it belongs to nobody;
                // rewinding lets `)` be seen where it actually is.
                self.pos = start;
                Ok(None)
            }
        }
    }

    fn number(&mut self) -> Result<f64, Error> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.pos += 1;
        }
        let mut digits = self.skip_digits();
        if self.peek() == Some(b'.') {
            self.pos += 1;
            digits |= self.skip_digits();
        }
        if !digits {
            return self.err_at(start, "expected a number");
        }
        // Only commit to the exponent once it is known to have digits:
        // `1e` is a number followed by junk, not a malformed number, and
        // `rotate(1)scale(2)` needs the same rewind discipline.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let mark = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !self.skip_digits() {
                self.pos = mark;
            }
        }
        let text = &self.input[start..self.pos];
        // `str::parse` also accepts `inf` and `NaN`, which the SVG number
        // grammar does not -- the scan above is what keeps them out. Overflow
        // to infinity is still reachable from `1e999`, and an infinite
        // coefficient poisons every coordinate it ever touches.
        match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(n),
            _ => self.err_at(start, format!("number `{text}` is out of range")),
        }
    }

    fn skip_digits(&mut self) -> bool {
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos > start
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    const EPS: f64 = 1e-9;

    fn map(transform: &str, x: f64, y: f64) -> Point {
        parse_transform(transform).expect("should parse") * Point::new(x, y)
    }

    fn close(a: Point, b: Point) -> bool {
        (a - b).length() < EPS
    }

    #[test]
    fn empty_is_the_identity() {
        assert_eq!(parse_transform(""), Ok(Affine::IDENTITY));
        assert_eq!(parse_transform("   \n "), Ok(Affine::IDENTITY));
    }

    #[test]
    fn matrix_arguments_are_the_coefficients_unchanged() {
        let t = parse_transform("matrix(1 2 3 4 5 6)").expect("should parse");
        assert_eq!(t.as_coeffs(), [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn every_function_parses() {
        assert!(close(
            map("translate(10 20)", 1.0, 2.0),
            Point::new(11.0, 22.0)
        ));
        // One argument translates in x only.
        assert!(close(map("translate(10)", 1.0, 2.0), Point::new(11.0, 2.0)));
        assert!(close(map("scale(2 3)", 1.0, 2.0), Point::new(2.0, 6.0)));
        // One argument scales both axes.
        assert!(close(map("scale(2)", 1.0, 2.0), Point::new(2.0, 4.0)));
        assert!(close(map("rotate(90)", 1.0, 0.0), Point::new(0.0, 1.0)));
        assert!(close(map("skewX(45)", 0.0, 1.0), Point::new(1.0, 1.0)));
        assert!(close(map("skewY(45)", 1.0, 0.0), Point::new(1.0, 1.0)));
        assert!(close(
            map("matrix(0 1 -1 0 0 0)", 1.0, 0.0),
            Point::new(0.0, 1.0)
        ));
    }

    #[test]
    fn rotate_about_a_point_fixes_that_point() {
        assert!(close(
            map("rotate(37 5 -7)", 5.0, -7.0),
            Point::new(5.0, -7.0)
        ));
        // A quarter turn about (1,1) takes the origin to (2,0).
        assert!(close(map("rotate(90 1 1)", 0.0, 0.0), Point::new(2.0, 0.0)));
    }

    #[test]
    fn composition_applies_the_last_function_first() {
        // Scale runs first: (1 * 2) + 10 = 12. Composing the other way round
        // gives (1 + 10) * 2 = 22, which is the bug this pins down.
        assert!(close(
            map("translate(10) scale(2)", 1.0, 0.0),
            Point::new(12.0, 0.0)
        ));
        assert!(close(
            map("scale(2) translate(10)", 1.0, 0.0),
            Point::new(22.0, 0.0)
        ));

        // And the same order holds against a hand-composed equivalent.
        let parsed = parse_transform("translate(10 20) rotate(30) scale(2 3)").expect("parses");
        let built = Affine::translate(Vec2::new(10.0, 20.0))
            * Affine::rotate(30f64.to_radians())
            * Affine::scale_non_uniform(2.0, 3.0);
        let p = Point::new(-4.0, 9.0);
        assert!(close(parsed * p, built * p));
    }

    #[test]
    fn separators_are_flexible() {
        let expected = parse_transform("translate(1 -2)").expect("should parse");
        for input in [
            "translate(1,-2)",
            "translate( 1 , -2 )",
            "translate(1-2)",
            "translate(1e0,-2e0)",
            "\ttranslate(1\n-2)\n",
        ] {
            assert_eq!(parse_transform(input), Ok(expected), "{input}");
        }
        // A list needs no separator between its functions either.
        assert_eq!(
            parse_transform("translate(1)scale(2)"),
            parse_transform("translate(1) , scale(2)")
        );
    }

    #[test]
    fn malformed_input_is_rejected_with_a_position() {
        for (input, column) in [
            ("wobble(1)", 1),
            ("translate", 10),
            ("translate(", 11),
            ("translate(1", 12),
            ("translate(1 2 3)", 15),
            ("matrix(1 2 3 4 5)", 17),
            ("rotate(90 1)", 12),
            // `1` parses and the stray `e` is what the close paren trips on.
            ("scale(1e)", 8),
            ("scale(.)", 7),
            ("scale(1e999)", 7),
            ("scale(nan)", 7),
            // A bare word is a function missing its arguments, so the paren is
            // what gets blamed, at the end of the name.
            ("scale(1) junk", 14),
            ("scale(1) 2", 10),
        ] {
            let err = parse_transform(input).expect_err(input);
            assert_eq!((err.line, err.column), (1, column), "{input}: {err}");
        }
    }

    #[test]
    fn non_ascii_before_the_error_counts_as_one_column() {
        let err = parse_transform("  ünï  scale(1)").expect_err("not a function");
        assert_eq!(err.column, 3);
    }

    #[test]
    fn matrix_round_trips_every_coefficient_exactly() {
        // `{:?}` on `f64` is the shortest round-tripping form, so any loss here
        // is the parser's, not the formatter's.
        check("matrix round-trip", 2000, Rng::affine, |t| {
            let [a, b, c, d, e, f] = t.as_coeffs();
            let text = format!("matrix({a:?} {b:?} {c:?} {d:?} {e:?} {f:?})");
            parse_transform(&text) == Ok(*t)
        });
    }

    #[test]
    fn arbitrary_junk_never_panics() {
        check("transform fuzz", 4000, arbitrary_text, |text| {
            let _ = parse_transform(text);
            true
        });
    }

    /// Text built from the alphabet the grammar cares about, so the generator
    /// spends its draws on near-misses rather than on bytes that fail at the
    /// first character.
    fn arbitrary_text(rng: &mut Rng) -> String {
        const PIECES: &[&str] = &[
            "translate",
            "scale",
            "rotate",
            "matrix",
            "skewX",
            "skewY",
            "(",
            ")",
            ",",
            " ",
            "1",
            "-2.5",
            "1e",
            "e9",
            ".",
            "+",
            "\n",
            "x",
        ];
        let len = rng.below(12);
        (0..len)
            .map(|_| PIECES[rng.below(PIECES.len() as u64) as usize])
            .collect()
    }
}
