//! Glyph outlines, from either of the two ways OpenType stores them.
//!
//! `glyf`/`loca` holds quadratic contours as point lists; `CFF`/`CFF2` holds
//! cubic contours as Type 2 charstring programs. [`PathEl`] carries both
//! `QuadTo` and `CurveTo`, so neither is converted -- the outline comes out in
//! the degree the font wrote it, in font units with y up.
//!
//! # Untrusted input
//!
//! Same rules as [`crate::opentype`]: every read goes through the checked
//! accessors, no count from the file bounds a loop on its own, and nothing
//! recurses. Both formats are recursive by design -- composite glyphs nest and
//! charstrings call subroutines -- so both are run from an explicit stack with
//! a depth limit and a work budget. Without the budget a glyph that references
//! itself ten deep with ten components per level is a 10^10-element outline
//! from a 200-byte file.
//!
//! # What is deliberately not here
//!
//! - **Hinting.** `glyf` instructions and the charstring hint operators are
//!   skipped. Hints only matter when snapping to a pixel grid, which happens
//!   far downstream of an outline in font units. The charstring hints are
//!   nonetheless *parsed*, not merely ignored: they clear the operand stack and
//!   `hintmask` carries inline bytes whose length depends on how many stems
//!   were declared, so a reader that skipped them blindly would resume in the
//!   middle of a mask and misread everything after it.
//! - **`FontMatrix`.** CFF coordinates are returned in charstring units. Every
//!   font whose matrix is not the default 1/1000 sets `head.unitsPerEm` to
//!   match, so the two agree; this is also what `fontTools` reports, which is
//!   what the outlines were validated against.

use crate::opentype::{Font, i16_at, u16_at, u32_at};
use hane_geom::{Affine, PathEl, Point};

/// How deeply composite glyphs and charstring subroutines may nest.
///
/// The Type 2 specification's own limit is 10 call frames; composites in
/// shipped fonts never exceed two levels. The same number serves both.
const MAX_DEPTH: usize = 10;

/// Components (or charstring operators) one glyph may expand to.
///
/// A bound on total work, not on nesting: nesting alone is bounded by
/// [`MAX_DEPTH`], but a composite tree can still be exponential in it.
const BUDGET: usize = 100_000;

/// The Type 2 operand stack limit. The specification says 48 for CFF and 513
/// for CFF2; the larger is used for both, since accepting a font that overruns
/// the smaller costs nothing and rejecting one that does not is a real bug.
const MAX_STACK: usize = 513;

impl Font<'_> {
    /// The outline of `glyph`, in font units, as a sequence of path elements.
    ///
    /// `Some` of an empty vector is a blank glyph -- a space, or any glyph with
    /// no contours. `None` means the glyph does not exist, the font has no
    /// outline table this parser understands, or the outline data is malformed.
    ///
    /// Contours are closed with [`PathEl::ClosePath`]. TrueType outlines come
    /// out as [`PathEl::QuadTo`] and CFF ones as [`PathEl::CurveTo`]; nothing
    /// converts between the two, because nothing downstream needs it.
    pub fn glyph_outline(&self, glyph: u16) -> Option<Vec<PathEl>> {
        let mut out = Vec::new();
        if self.table("glyf").is_some() {
            truetype_outline(self, glyph, &mut out)?;
        } else {
            // ponytail: re-reads the CFF header, top DICT and one Private DICT
            // per glyph. That is a few hundred bytes of parsing against a
            // charstring interpreter, so it does not show up until a whole
            // paragraph is being laid out; cache a parsed `Cff` on `Font` when
            // P8 shaping starts drawing runs.
            Cff::parse(self)?.outline(glyph, &mut out)?;
        }
        Some(out)
    }
}

// --- TrueType: `glyf` and `loca`.

/// Append the contours of `glyph`, resolving composites depth-first.
fn truetype_outline(font: &Font<'_>, glyph: u16, out: &mut Vec<PathEl>) -> Option<()> {
    let glyf = font.table("glyf")?;
    let loca = font.table("loca")?;
    let long = font.head().index_to_loc_format == 1;
    let count = font.glyph_count();
    if glyph >= count {
        return None;
    }

    let mut budget = BUDGET;
    // Explicit stack rather than recursion: composite depth is file data, and
    // a font is free to claim any of it.
    let mut pending = vec![(glyph, Affine::IDENTITY, 0usize)];
    while let Some((gid, transform, depth)) = pending.pop() {
        // Two separate bounds: one on how many components are ever queued, one
        // on how much they draw. A composite tree ten deep with ten components
        // per level is a 10^10-element outline from a file that fits in a
        // packet, and neither bound alone stops it.
        budget = budget.checked_sub(1)?;
        if out.len() > BUDGET {
            return None;
        }
        let Some(data) = glyph_data(glyf, loca, long, gid) else {
            // A component pointing at a glyph the file does not contain is
            // corruption; a missing outline for the glyph asked for is not.
            return None;
        };
        let Some(contours) = i16_at(data, 0).ok() else {
            continue; // Empty slot: a glyph with no data at all is blank.
        };
        if contours >= 0 {
            simple_glyph(data, contours as usize, transform, out)?;
        } else {
            if depth >= MAX_DEPTH {
                return None;
            }
            // Pushed in reverse so they pop in file order, which is the order
            // a reference implementation emits them in.
            let mut components = Vec::new();
            read_components(data, count, &mut components)?;
            budget = budget.checked_sub(components.len())?;
            for (child, child_transform) in components.into_iter().rev() {
                pending.push((child, transform * child_transform, depth + 1));
            }
        }
    }
    Some(())
}

/// The `glyf` bytes for one glyph, via `loca`. `None` when the entry is bad.
///
/// An empty range is a blank glyph, and yields an empty slice rather than
/// `None`: a space is not an error.
fn glyph_data<'a>(glyf: &'a [u8], loca: &[u8], long: bool, gid: u16) -> Option<&'a [u8]> {
    let i = gid as usize;
    // `i` is at most 65535, so these products cannot overflow a usize.
    let (start, end) = if long {
        (
            u32_at(loca, i * 4).ok()? as usize,
            u32_at(loca, i * 4 + 4).ok()? as usize,
        )
    } else {
        // The short format stores halved offsets, which is why a `loca` in this
        // format cannot address a `glyf` larger than 128 KiB.
        (
            u16_at(loca, i * 2).ok()? as usize * 2,
            u16_at(loca, i * 2 + 2).ok()? as usize * 2,
        )
    };
    // Clamped, not rejected: the last entry of `loca` routinely points at the
    // padded length of `glyf` rather than its real one.
    glyf.get(start..end.min(glyf.len()))
}

/// Read the point lists of a simple glyph and emit its contours.
fn simple_glyph(data: &[u8], contours: usize, t: Affine, out: &mut Vec<PathEl>) -> Option<()> {
    if contours == 0 {
        return Some(());
    }
    // The last end-point index gives the point count; both reads are bounds
    // checked, so a dishonest contour count fails here rather than allocating.
    let points = u16_at(data, 8 + contours * 2).ok()? as usize + 1;
    let instructions = u16_at(data, 10 + contours * 2).ok()? as usize;
    let mut p = (12 + contours * 2).checked_add(instructions)?;

    // Flags, run-length encoded. The loop is bounded by `points` and by the
    // bytes available, so neither a huge count nor a truncated table spins.
    let mut flags = Vec::with_capacity(points.min(data.len()));
    while flags.len() < points {
        let f = *data.get(p)?;
        p += 1;
        flags.push(f);
        if f & 0x08 != 0 {
            let repeat = *data.get(p)? as usize;
            p += 1;
            for _ in 0..repeat.min(points - flags.len()) {
                flags.push(f);
            }
        }
    }

    // Coordinates are deltas, in one of three widths chosen per point by two
    // flag bits whose meaning changes with the width -- the "same" bit doubles
    // as the sign bit for a short delta.
    let mut coords = Vec::with_capacity(points * 2);
    for (short, same) in [(0x02u8, 0x10u8), (0x04, 0x20)] {
        // i64, not i32: 65536 points of -32768 each overflow an i32 by one,
        // which is a debug-build panic reachable from a crafted file.
        let mut v = 0i64;
        for &f in &flags {
            if f & short != 0 {
                let d = i64::from(*data.get(p)?);
                p += 1;
                v += if f & same != 0 { d } else { -d };
            } else if f & same == 0 {
                v += i64::from(i16_at(data, p).ok()?);
                p += 2;
            }
            coords.push(v as f64);
        }
    }

    let mut start = 0usize;
    for c in 0..contours {
        let end = u16_at(data, 10 + c * 2).ok()? as usize;
        if end >= points || end < start {
            // Out-of-order or out-of-range end indices are corruption; the
            // contours read so far are still valid, so stop rather than fail.
            break;
        }
        emit_contour(
            &flags[start..=end],
            &coords[start..=end],
            &coords[points + start..],
            t,
            out,
        );
        start = end + 1;
    }
    Some(())
}

/// One contour, reconstructing the on-curve points TrueType leaves out.
///
/// Between two consecutive off-curve points there is an implied on-curve point
/// at their midpoint. Omitting it is not an optional compression -- a reader
/// that joins consecutive control points directly draws a plausible but wrong
/// shape, which is why this is the single most-tested function in the file.
fn emit_contour(flags: &[u8], xs: &[f64], ys: &[f64], t: Affine, out: &mut Vec<PathEl>) {
    let n = flags.len();
    if n == 0 {
        return;
    }
    let at = |i: usize| Point::new(xs[i % n], ys[i % n]);
    let on = |i: usize| flags[i % n] & 0x01 != 0;
    let mid = |a: Point, b: Point| Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);

    // A contour may legally have no on-curve point at all -- a circle drawn as
    // four control points. Then every on-curve point is implied, and the start
    // is the midpoint of the wrap-around pair.
    let (start, first, steps) = match (0..n).find(|&i| on(i)) {
        Some(i) => (at(i), i + 1, n - 1),
        None => (mid(at(n - 1), at(0)), 0, n),
    };
    out.push(PathEl::MoveTo(t * start));

    let mut control: Option<Point> = None;
    for k in 0..steps {
        let p = at(first + k);
        if on(first + k) {
            match control.take() {
                Some(c) => out.push(PathEl::QuadTo(t * c, t * p)),
                None => out.push(PathEl::LineTo(t * p)),
            }
        } else {
            if let Some(c) = control {
                out.push(PathEl::QuadTo(t * c, t * mid(c, p)));
            }
            control = Some(p);
        }
    }
    if let Some(c) = control {
        out.push(PathEl::QuadTo(t * c, t * start));
    }
    out.push(PathEl::ClosePath);
}

/// The components of a composite glyph, each with its placement transform.
fn read_components(data: &[u8], glyphs: u16, out: &mut Vec<(u16, Affine)>) -> Option<()> {
    let mut p = 10usize;
    loop {
        let flags = u16_at(data, p).ok()?;
        let gid = u16_at(data, p + 2).ok()?;
        p += 4;
        let (arg1, arg2) = if flags & 0x0001 != 0 {
            let v = (i16_at(data, p).ok()?, i16_at(data, p + 2).ok()?);
            p += 4;
            (f64::from(v.0), f64::from(v.1))
        } else {
            let v = (*data.get(p)? as i8, *data.get(p + 1)? as i8);
            p += 2;
            (f64::from(v.0), f64::from(v.1))
        };

        // TrueType's 2x2 is stored row-major-by-column as (xscale, scale01,
        // scale10, yscale), which is exactly SVG's `matrix()` order, so it
        // drops into `Affine` unpermuted.
        let mut m = [1.0, 0.0, 0.0, 1.0];
        if flags & 0x0008 != 0 {
            m[0] = f2dot14(data, p)?;
            m[3] = m[0];
            p += 2;
        } else if flags & 0x0040 != 0 {
            m[0] = f2dot14(data, p)?;
            m[3] = f2dot14(data, p + 2)?;
            p += 4;
        } else if flags & 0x0080 != 0 {
            for (i, slot) in m.iter_mut().enumerate() {
                *slot = f2dot14(data, p + i * 2)?;
            }
            p += 8;
        }

        // ARGS_ARE_XY_VALUES clear means the arguments are point indices to
        // align rather than an offset.
        // ponytail: point matching is treated as a zero offset. It needs the
        // parent's accumulated point list, which nothing else here keeps, and
        // not one component in the 780 TrueType faces on the development
        // machine uses it. Thread the point list through if one ever does.
        let (mut dx, mut dy) = if flags & 0x0002 != 0 {
            (arg1, arg2)
        } else {
            (0.0, 0.0)
        };
        // The default is Microsoft's: the offset is in the parent's space, not
        // the component's, so it is not scaled. Apple's flag reverses that.
        if flags & 0x0800 != 0 {
            (dx, dy) = (m[0] * dx + m[2] * dy, m[1] * dx + m[3] * dy);
        }

        if gid < glyphs {
            out.push((gid, Affine::new([m[0], m[1], m[2], m[3], dx, dy])));
        }
        if flags & 0x0020 == 0 {
            return Some(());
        }
        if out.len() > BUDGET {
            return None;
        }
    }
}

/// A 2.14 fixed-point number, the format composite transforms are stored in.
fn f2dot14(data: &[u8], off: usize) -> Option<f64> {
    Some(f64::from(i16_at(data, off).ok()?) / 16384.0)
}

// --- CFF: INDEX and DICT structures.

/// A CFF INDEX: a count, an offset array and a blob the offsets point into.
#[derive(Clone, Copy, Debug)]
struct Index<'a> {
    data: &'a [u8],
    offsets: usize,
    off_size: usize,
    count: usize,
    /// One before the first data byte, because INDEX offsets are 1-based.
    base: usize,
}

impl<'a> Index<'a> {
    /// An INDEX at `pos`, plus the position just past it.
    ///
    /// CFF2 widened the count to 32 bits and changed nothing else.
    fn parse(data: &'a [u8], pos: usize, cff2: bool) -> Option<(Self, usize)> {
        let header = if cff2 { 4 } else { 2 };
        let count = if cff2 {
            u32_at(data, pos).ok()? as usize
        } else {
            u16_at(data, pos).ok()? as usize
        };
        let empty = Self {
            data,
            offsets: 0,
            off_size: 1,
            count: 0,
            base: 0,
        };
        if count == 0 {
            return Some((empty, pos.checked_add(header)?));
        }
        let off_size = *data.get(pos.checked_add(header)?)? as usize;
        if !(1..=4).contains(&off_size) {
            return None;
        }
        let offsets = pos.checked_add(header + 1)?;
        // The offset array must fit; this is where an absurd count is rejected,
        // rather than by trusting the count against the file length directly.
        let base = offsets
            .checked_add(count.checked_add(1)?.checked_mul(off_size)?)?
            .checked_sub(1)?;
        if base > data.len() {
            return None;
        }
        let index = Self {
            data,
            offsets,
            off_size,
            count,
            base,
        };
        let end = base.checked_add(index.offset(count)?)?;
        if end > data.len() {
            return None;
        }
        Some((index, end))
    }

    /// The `i`th entry of the offset array, 0-based over `0..=count`.
    fn offset(&self, i: usize) -> Option<usize> {
        let at = self.offsets.checked_add(i.checked_mul(self.off_size)?)?;
        let bytes = self.data.get(at..at.checked_add(self.off_size)?)?;
        Some(bytes.iter().fold(0usize, |acc, &b| acc << 8 | b as usize))
    }

    /// The `i`th object, or `None` if absent or self-contradictory.
    fn get(&self, i: usize) -> Option<&'a [u8]> {
        if i >= self.count {
            return None;
        }
        let start = self.base.checked_add(self.offset(i)?)?;
        let end = self.base.checked_add(self.offset(i + 1)?)?;
        if start > end {
            return None;
        }
        self.data.get(start..end)
    }
}

/// The operands of the last occurrence of `key` in a DICT, or `None`.
///
/// DICTs are a few dozen bytes, so re-scanning per key is cheaper than
/// building a map. Real operands are skipped rather than decoded: no key this
/// module reads is ever written as one.
fn dict_get(dict: &[u8], key: u16) -> Option<Vec<f64>> {
    let mut operands: Vec<f64> = Vec::new();
    let mut p = 0usize;
    while p < dict.len() {
        let b0 = dict[p];
        match b0 {
            // 22..=27 and 31 are reserved in CFF and are `blend`/`vsindex` in
            // CFF2. Treated as unknown operators: they clear the operands, and
            // the keys read here never follow one in a way that matters.
            0..=27 | 31 => {
                let op = if b0 == 12 {
                    p += 2;
                    0x0c00 | u16::from(*dict.get(p - 1)?)
                } else {
                    p += 1;
                    u16::from(b0)
                };
                if op == key {
                    return Some(operands);
                }
                operands.clear();
            }
            28 => {
                operands.push(f64::from(i16_at(dict, p + 1).ok()?));
                p += 3;
            }
            29 => {
                operands.push(f64::from(u32_at(dict, p + 1).ok()? as i32));
                p += 5;
            }
            30 => {
                // Binary-coded decimal, terminated by an `f` nibble.
                p += 1;
                loop {
                    let b = *dict.get(p)?;
                    p += 1;
                    if b & 0xf0 == 0xf0 || b & 0x0f == 0x0f {
                        break;
                    }
                }
                operands.push(0.0);
            }
            32..=246 => {
                operands.push(f64::from(b0) - 139.0);
                p += 1;
            }
            247..=250 => {
                let b1 = f64::from(*dict.get(p + 1)?);
                operands.push((f64::from(b0) - 247.0) * 256.0 + b1 + 108.0);
                p += 2;
            }
            251..=254 => {
                let b1 = f64::from(*dict.get(p + 1)?);
                operands.push(-(f64::from(b0) - 251.0) * 256.0 - b1 - 108.0);
                p += 2;
            }
            255 => return None,
        }
        if operands.len() > MAX_STACK {
            return None;
        }
    }
    None
}

/// The last operand of `key`, which is how every offset in a DICT is spelled.
fn dict_offset(dict: &[u8], key: u16) -> Option<usize> {
    let v = *dict_get(dict, key)?.last()?;
    (v >= 0.0 && v < u32::MAX as f64).then_some(v as usize)
}

// --- CFF: the font-level structures a charstring runs against.

/// Everything a charstring needs, resolved from `CFF ` or `CFF2`.
struct Cff<'a> {
    data: &'a [u8],
    charstrings: Index<'a>,
    gsubrs: Index<'a>,
    /// Local subroutines of the top-level Private DICT. Empty for a CID font,
    /// which has one Private DICT per FD instead.
    subrs: Index<'a>,
    fd_array: Option<Index<'a>>,
    fd_select: Option<&'a [u8]>,
    /// `charset` offset, or 0/1/2 for the predefined ones.
    charset: usize,
    /// CFF2 only: the `ItemVariationStore`, needed to size a `blend`.
    var_store: Option<&'a [u8]>,
    cff2: bool,
}

impl<'a> Cff<'a> {
    fn parse(font: &Font<'a>) -> Option<Self> {
        match font.table("CFF2") {
            Some(t) => Self::parse_cff2(t),
            None => Self::parse_cff1(font.table("CFF ")?),
        }
    }

    fn parse_cff1(data: &'a [u8]) -> Option<Self> {
        let header = *data.get(2)? as usize;
        let (_names, p) = Index::parse(data, header, false)?;
        let (tops, p) = Index::parse(data, p, false)?;
        let (_strings, p) = Index::parse(data, p, false)?;
        let (gsubrs, _) = Index::parse(data, p, false)?;
        let top = tops.get(0)?;

        // CharstringType defaults to 2. A font declaring Type 1 charstrings in
        // a CFF wrapper exists in theory and would be silently misparsed.
        if dict_get(top, 0x0c06).is_some_and(|v| v.last() != Some(&2.0)) {
            return None;
        }
        let (charstrings, _) = Index::parse(data, dict_offset(top, 17)?, false)?;
        let subrs = dict_get(top, 18)
            .and_then(|v| private_subrs(data, &v, false))
            .unwrap_or(EMPTY_INDEX);

        Some(Self {
            data,
            charstrings,
            gsubrs,
            subrs,
            fd_array: dict_offset(top, 0x0c24)
                .and_then(|o| Index::parse(data, o, false))
                .map(|(i, _)| i),
            fd_select: dict_offset(top, 0x0c25).and_then(|o| data.get(o..)),
            charset: dict_offset(top, 15).unwrap_or(0),
            var_store: None,
            cff2: false,
        })
    }

    fn parse_cff2(data: &'a [u8]) -> Option<Self> {
        let header = *data.get(2)? as usize;
        let top_len = u16_at(data, 3).ok()? as usize;
        let top = data.get(header..header.checked_add(top_len)?)?;
        let (gsubrs, _) = Index::parse(data, header + top_len, true)?;
        let (charstrings, _) = Index::parse(data, dict_offset(top, 17)?, true)?;
        Some(Self {
            data,
            charstrings,
            gsubrs,
            subrs: EMPTY_INDEX,
            // CFF2 always routes through the FDArray, even for one FD.
            fd_array: Index::parse(data, dict_offset(top, 0x0c24)?, true).map(|(i, _)| i),
            fd_select: dict_offset(top, 0x0c25).and_then(|o| data.get(o..)),
            charset: 0,
            // The store is prefixed by its own u16 length, which nothing reads.
            var_store: dict_offset(top, 24).and_then(|o| data.get(o + 2..)),
            cff2: true,
        })
    }

    /// The local subroutines and default variation index for one glyph.
    ///
    /// A CID-keyed font partitions its glyphs across Private DICTs, so which
    /// subroutines a charstring may call depends on which glyph is running it.
    fn private(&self, glyph: u16) -> (Index<'a>, usize) {
        let Some(fds) = self.fd_array else {
            return (self.subrs, 0);
        };
        let fd = self.fd_select.map_or(0, |s| fd_select(s, glyph));
        let Some(dict) = fds.get(fd) else {
            return (self.subrs, 0);
        };
        let vsindex = dict_offset(dict, 22).unwrap_or(0);
        match dict_get(dict, 18).and_then(|v| private_subrs(self.data, &v, self.cff2)) {
            Some(subrs) => (subrs, vsindex),
            None => (self.subrs, vsindex),
        }
    }

    /// Append the outline of `glyph`, resolving a `seac` accent composition.
    fn outline(&self, glyph: u16, out: &mut Vec<PathEl>) -> Option<()> {
        if let Some((adx, ady, bchar, achar)) = self.run(glyph, 0.0, 0.0, out)? {
            // `seac` cannot nest: it only reaches here from `endchar`, which
            // ends the charstring, and the two components are run with the
            // result discarded rather than followed.
            let base = self.standard_glyph(bchar)?;
            let accent = self.standard_glyph(achar)?;
            self.run(base, 0.0, 0.0, out)?;
            self.run(accent, adx, ady, out)?;
        }
        Some(())
    }

    /// Run one charstring, translated by `(dx, dy)`.
    ///
    /// The translation is applied by starting the pen there rather than by
    /// transforming the output: every charstring move is relative to the
    /// origin, so the two are the same thing and this one is free.
    fn run(
        &self,
        glyph: u16,
        dx: f64,
        dy: f64,
        out: &mut Vec<PathEl>,
    ) -> Option<Option<(f64, f64, u8, u8)>> {
        let code = self.charstrings.get(glyph as usize)?;
        let (subrs, vsindex) = self.private(glyph);
        let mut m = Machine {
            cff: self,
            subrs,
            out,
            stack: Vec::new(),
            x: dx,
            y: dy,
            stems: 0,
            width_done: self.cff2, // CFF2 charstrings carry no width.
            open: false,
            regions: self.regions(vsindex).unwrap_or(0),
            seac: None,
            budget: BUDGET,
        };
        m.run(code)?;
        if m.open {
            m.out.push(PathEl::ClosePath);
        }
        Some(m.seac)
    }

    /// The number of variation regions the `blend` operator expects, for the
    /// `ItemVariationData` at `vsindex`.
    fn regions(&self, vsindex: usize) -> Option<usize> {
        let store = self.var_store?;
        let count = u16_at(store, 6).ok()? as usize;
        if vsindex >= count {
            return None;
        }
        let offset = u32_at(store, 8 + vsindex * 4).ok()? as usize;
        Some(u16_at(store, offset + 4).ok()? as usize)
    }

    /// The glyph for a Standard Encoding code, as `seac` names its components.
    fn standard_glyph(&self, code: u8) -> Option<u16> {
        let sid = standard_encoding(code)?;
        match self.charset {
            // A predefined charset assigns SID `i` to glyph `i`. Only ISOAdobe
            // is really that; the two Expert charsets are not, but no font
            // combines one with `seac`.
            0..=2 => (usize::from(sid) < self.charstrings.count).then_some(sid),
            offset => self.charset_lookup(offset, sid),
        }
    }

    /// Scan a custom `charset` for the glyph whose SID is `sid`.
    fn charset_lookup(&self, offset: usize, sid: u16) -> Option<u16> {
        if sid == 0 {
            return Some(0); // .notdef is glyph 0 in every charset.
        }
        let t = self.data.get(offset..)?;
        let glyphs = self.charstrings.count;
        let mut gid = 1usize;
        match *t.first()? {
            0 => {
                while gid < glyphs {
                    if u16_at(t, 1 + (gid - 1) * 2).ok()? == sid {
                        return u16::try_from(gid).ok();
                    }
                    gid += 1;
                }
            }
            format @ (1 | 2) => {
                let step = if format == 1 { 3 } else { 4 };
                let mut p = 1usize;
                while gid < glyphs {
                    let first = u16_at(t, p).ok()?;
                    let left = if format == 1 {
                        u16::from(*t.get(p + 2)?)
                    } else {
                        u16_at(t, p + 2).ok()?
                    };
                    if (first..=first.saturating_add(left)).contains(&sid) {
                        return u16::try_from(gid + usize::from(sid - first)).ok();
                    }
                    gid += usize::from(left) + 1;
                    p += step;
                }
            }
            _ => return None,
        }
        None
    }
}

/// An INDEX with no entries, for a font with no local subroutines.
const EMPTY_INDEX: Index<'static> = Index {
    data: &[],
    offsets: 0,
    off_size: 1,
    count: 0,
    base: 0,
};

/// The local subroutine INDEX of a Private DICT given as `[size, offset]`.
///
/// The `Subrs` offset inside is relative to the Private DICT, not to the table
/// -- one of the two places in CFF where an offset is not absolute.
fn private_subrs<'a>(data: &'a [u8], private: &[f64], cff2: bool) -> Option<Index<'a>> {
    let (&size, &offset) = (private.first()?, private.get(1)?);
    if size < 0.0 || offset < 0.0 {
        return None;
    }
    let (size, offset) = (size as usize, offset as usize);
    let dict = data.get(offset..offset.checked_add(size)?)?;
    let subrs = dict_offset(dict, 19)?;
    Index::parse(data, offset.checked_add(subrs)?, cff2).map(|(i, _)| i)
}

/// The FD index for a glyph, from either `FDSelect` format.
fn fd_select(t: &[u8], glyph: u16) -> usize {
    match t.first() {
        Some(0) => t.get(1 + glyph as usize).map_or(0, |&fd| fd as usize),
        Some(3) => {
            let ranges = u16_at(t, 1).map_or(0, |n| n as usize);
            // Ranges are sorted by first glyph; the last one whose first glyph
            // is not past ours wins.
            let mut fd = 0usize;
            for i in 0..ranges {
                let Ok(first) = u16_at(t, 3 + i * 3) else {
                    break;
                };
                if first > glyph {
                    break;
                }
                fd = t.get(5 + i * 3).map_or(0, |&v| v as usize);
            }
            fd
        }
        _ => 0,
    }
}

/// The subroutine index bias, which depends on how many there are.
///
/// Type 2 numbers subroutines from the middle of the INDEX outwards so that
/// small charstring integers can reach both ends of a large one.
fn bias(count: usize) -> i32 {
    if count < 1240 {
        107
    } else if count < 33900 {
        1131
    } else {
        32768
    }
}

/// Standard Encoding code to SID, for `seac`.
///
/// Codes 32..=126 are SIDs 1..=95 in order, because the first 95 standard
/// strings were defined as exactly that range of the encoding. The upper half
/// is irregular and is these thirteen runs.
fn standard_encoding(code: u8) -> Option<u16> {
    const RUNS: [(u8, u8, u16); 13] = [
        (161, 175, 96),
        (177, 180, 111),
        (182, 189, 115),
        (191, 191, 123),
        (193, 200, 124),
        (202, 203, 132),
        (205, 208, 134),
        (225, 225, 138),
        (227, 227, 139),
        (232, 235, 140),
        (241, 241, 144),
        (245, 245, 145),
        (248, 251, 146),
    ];
    if (32..=126).contains(&code) {
        return Some(u16::from(code) - 31);
    }
    RUNS.iter()
        .find(|&&(lo, hi, _)| (lo..=hi).contains(&code))
        .map(|&(lo, _, sid)| sid + u16::from(code - lo))
}

// --- CFF: the Type 2 charstring interpreter.

/// The charstring stack machine: an operand stack, a pen and a call stack.
struct Machine<'a, 'b> {
    cff: &'b Cff<'a>,
    subrs: Index<'a>,
    out: &'b mut Vec<PathEl>,
    stack: Vec<f64>,
    x: f64,
    y: f64,
    stems: usize,
    width_done: bool,
    open: bool,
    regions: usize,
    seac: Option<(f64, f64, u8, u8)>,
    budget: usize,
}

impl<'a> Machine<'a, '_> {
    fn moveto(&mut self) {
        if self.open {
            self.out.push(PathEl::ClosePath);
        }
        self.out.push(PathEl::MoveTo(Point::new(self.x, self.y)));
        self.open = true;
    }

    fn lineto(&mut self) {
        self.out.push(PathEl::LineTo(Point::new(self.x, self.y)));
    }

    /// A cubic from six relative deltas, the only shape a charstring curve has.
    fn curveto(&mut self, dx1: f64, dy1: f64, dx2: f64, dy2: f64, dx3: f64, dy3: f64) {
        let c1 = Point::new(self.x + dx1, self.y + dy1);
        let c2 = Point::new(c1.x + dx2, c1.y + dy2);
        self.x = c2.x + dx3;
        self.y = c2.y + dy3;
        self.out
            .push(PathEl::CurveTo(c1, c2, Point::new(self.x, self.y)));
    }

    /// Drop the leading operand if it is the glyph width.
    ///
    /// The width is optional and unmarked: it is present exactly when the first
    /// stack-clearing operator has one more operand than it takes. Miscounting
    /// here shifts every coordinate in the glyph by one operand.
    fn take_width(&mut self, expected: usize, even: bool) {
        if self.width_done {
            return;
        }
        self.width_done = true;
        let extra = if even {
            self.stack.len() % 2 == 1
        } else {
            self.stack.len() > expected
        };
        if extra && !self.stack.is_empty() {
            self.stack.remove(0);
        }
    }

    /// Count stem hints and clear the stack, which every hint operator does.
    fn stems(&mut self) {
        self.take_width(0, true);
        self.stems += self.stack.len() / 2;
        self.stack.clear();
    }

    fn run(&mut self, code: &'a [u8]) -> Option<()> {
        let mut frames: Vec<(&'a [u8], usize)> = Vec::new();
        let (mut code, mut pos) = (code, 0usize);
        loop {
            if pos >= code.len() {
                // Falling off the end of a subroutine without `return` is
                // malformed but harmless; treat it as one.
                match frames.pop() {
                    Some(frame) => (code, pos) = frame,
                    None => return Some(()),
                }
                continue;
            }
            self.budget = self.budget.checked_sub(1)?;
            let b0 = code[pos];
            pos += 1;

            // Operands first: everything from 32 up, plus 28, is a number.
            let operand = match b0 {
                28 => {
                    let v = f64::from(i16_at(code, pos).ok()?);
                    pos += 2;
                    Some(v)
                }
                32..=246 => Some(f64::from(b0) - 139.0),
                247..=250 => {
                    let b1 = f64::from(*code.get(pos)?);
                    pos += 1;
                    Some((f64::from(b0) - 247.0) * 256.0 + b1 + 108.0)
                }
                251..=254 => {
                    let b1 = f64::from(*code.get(pos)?);
                    pos += 1;
                    Some(-(f64::from(b0) - 251.0) * 256.0 - b1 - 108.0)
                }
                255 => {
                    // 16.16 fixed point, unlike the DICT operator of the same
                    // byte, which is a 32-bit integer.
                    let v = f64::from(u32_at(code, pos).ok()? as i32) / 65536.0;
                    pos += 4;
                    Some(v)
                }
                _ => None,
            };
            if let Some(v) = operand {
                if self.stack.len() >= MAX_STACK {
                    return None;
                }
                self.stack.push(v);
                continue;
            }

            match b0 {
                1 | 3 | 18 | 23 => self.stems(),
                19 | 20 => {
                    // The mask itself is inline: one bit per stem declared so
                    // far. Skipping the wrong number of bytes here resumes in
                    // the middle of a mask and corrupts everything after.
                    self.stems();
                    pos = pos.checked_add(self.stems.div_ceil(8))?;
                    if pos > code.len() {
                        return None;
                    }
                }
                21 => {
                    self.take_width(2, false);
                    self.x += self.stack.first().copied().unwrap_or(0.0);
                    self.y += self.stack.get(1).copied().unwrap_or(0.0);
                    self.moveto();
                    self.stack.clear();
                }
                22 | 4 => {
                    self.take_width(1, false);
                    let d = self.stack.first().copied().unwrap_or(0.0);
                    if b0 == 22 {
                        self.x += d;
                    } else {
                        self.y += d;
                    }
                    self.moveto();
                    self.stack.clear();
                }
                5 => {
                    let s = core::mem::take(&mut self.stack);
                    for d in s.chunks_exact(2) {
                        self.x += d[0];
                        self.y += d[1];
                        self.lineto();
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                6 | 7 => {
                    let s = core::mem::take(&mut self.stack);
                    let mut horizontal = b0 == 6;
                    for &d in &s {
                        if horizontal {
                            self.x += d;
                        } else {
                            self.y += d;
                        }
                        self.lineto();
                        horizontal = !horizontal;
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                8 => {
                    let s = core::mem::take(&mut self.stack);
                    for d in s.chunks_exact(6) {
                        self.curveto(d[0], d[1], d[2], d[3], d[4], d[5]);
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                24 => {
                    let s = core::mem::take(&mut self.stack);
                    let mut i = 0;
                    while s.len() >= i + 8 {
                        self.curveto(s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5]);
                        i += 6;
                    }
                    if s.len() >= i + 2 {
                        self.x += s[i];
                        self.y += s[i + 1];
                        self.lineto();
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                25 => {
                    let s = core::mem::take(&mut self.stack);
                    let mut i = 0;
                    while s.len() >= i + 8 {
                        self.x += s[i];
                        self.y += s[i + 1];
                        self.lineto();
                        i += 2;
                    }
                    if s.len() >= i + 6 {
                        self.curveto(s[i], s[i + 1], s[i + 2], s[i + 3], s[i + 4], s[i + 5]);
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                26 | 27 => {
                    // vvcurveto and hhcurveto: a run of curves that all start
                    // in the same direction, with one optional leading delta in
                    // the other axis, signalled by an odd operand count.
                    let s = core::mem::take(&mut self.stack);
                    let mut i = 0;
                    let mut lead = 0.0;
                    if s.len() % 4 == 1 {
                        lead = s[0];
                        i = 1;
                    }
                    while s.len() >= i + 4 {
                        if b0 == 26 {
                            self.curveto(lead, s[i], s[i + 1], s[i + 2], 0.0, s[i + 3]);
                        } else {
                            self.curveto(s[i], lead, s[i + 1], s[i + 2], s[i + 3], 0.0);
                        }
                        lead = 0.0;
                        i += 4;
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                30 | 31 => {
                    // vhcurveto and hvcurveto alternate direction each curve. A
                    // fifth operand on the final group is the free coordinate
                    // of its end point, which is otherwise implied.
                    let s = core::mem::take(&mut self.stack);
                    let mut i = 0;
                    let mut horizontal = b0 == 31;
                    while s.len() >= i + 4 {
                        let extra = if s.len() == i + 5 { s[i + 4] } else { 0.0 };
                        if horizontal {
                            self.curveto(s[i], 0.0, s[i + 1], s[i + 2], extra, s[i + 3]);
                        } else {
                            self.curveto(0.0, s[i], s[i + 1], s[i + 2], s[i + 3], extra);
                        }
                        i += 4;
                        horizontal = !horizontal;
                    }
                    self.stack = s;
                    self.stack.clear();
                }
                10 | 29 => {
                    let index = if b0 == 10 {
                        self.subrs
                    } else {
                        self.cff.gsubrs
                    };
                    let n = self.stack.pop()? as i32 + bias(index.count);
                    let subr = usize::try_from(n).ok().and_then(|i| index.get(i))?;
                    if frames.len() >= MAX_DEPTH {
                        return None;
                    }
                    frames.push((code, pos));
                    (code, pos) = (subr, 0);
                }
                11 => match frames.pop() {
                    Some(frame) => (code, pos) = frame,
                    None => return Some(()),
                },
                14 => {
                    // The legacy accent form: `endchar` with the four operands
                    // the withdrawn `seac` operator used to take. So `endchar`
                    // takes either zero or four, and a width makes it one or
                    // five -- four is the accent form, not a width plus three.
                    let expected = if self.stack.len() >= 4 { 4 } else { 0 };
                    self.take_width(expected, false);
                    if self.stack.len() >= 4 {
                        let s = &self.stack[self.stack.len() - 4..];
                        self.seac = Some((s[0], s[1], s[2] as u8, s[3] as u8));
                    }
                    return Some(());
                }
                15 => {
                    // vsindex, CFF2 only: selects which variation data, and so
                    // how many deltas a later `blend` carries.
                    let i = self.stack.pop().unwrap_or(0.0);
                    self.regions = usize::try_from(i as i64)
                        .ok()
                        .and_then(|i| self.cff.regions(i))?;
                    self.stack.clear();
                }
                16 => {
                    // blend, CFF2 only: n base values followed by n*regions
                    // deltas. At the default instance every delta scales to
                    // zero, so the deltas are simply dropped.
                    let n = usize::try_from(self.stack.pop()? as i64).ok()?;
                    let deltas = n.checked_mul(self.regions)?;
                    if self.stack.len() < n.checked_add(deltas)? {
                        return None;
                    }
                    self.stack.truncate(self.stack.len() - deltas);
                }
                12 => {
                    let b1 = *code.get(pos)?;
                    pos += 1;
                    self.escape(b1)?;
                }
                _ => return None,
            }
        }
    }

    /// The two-byte operators: the four flex forms and the arithmetic ones.
    fn escape(&mut self, op: u8) -> Option<()> {
        match op {
            34 => {
                // hflex: a flex whose end points and outer controls all sit on
                // the starting y, so only one y delta is stored.
                let s = core::mem::take(&mut self.stack);
                if s.len() < 7 {
                    return None;
                }
                let y0 = self.y;
                self.curveto(s[0], 0.0, s[1], s[2], s[3], 0.0);
                let dy = y0 - self.y;
                self.curveto(s[4], 0.0, s[5], dy, s[6], 0.0);
                self.stack = s;
                self.stack.clear();
            }
            35 => {
                // flex: two ordinary curves plus a flex depth that only a
                // hinting rasteriser cares about.
                let s = core::mem::take(&mut self.stack);
                if s.len() < 13 {
                    return None;
                }
                self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                self.curveto(s[6], s[7], s[8], s[9], s[10], s[11]);
                self.stack = s;
                self.stack.clear();
            }
            36 => {
                let s = core::mem::take(&mut self.stack);
                if s.len() < 9 {
                    return None;
                }
                let y0 = self.y;
                self.curveto(s[0], s[1], s[2], s[3], s[4], 0.0);
                let dy = y0 - self.y - s[7];
                self.curveto(s[5], 0.0, s[6], s[7], s[8], dy);
                self.stack = s;
                self.stack.clear();
            }
            37 => {
                // flex1: the last point closes back to the start in whichever
                // axis the flex moved less, so only one of its deltas is given.
                let s = core::mem::take(&mut self.stack);
                if s.len() < 11 {
                    return None;
                }
                let (x0, y0) = (self.x, self.y);
                let dx = s[0] + s[2] + s[4] + s[6] + s[8];
                let dy = s[1] + s[3] + s[5] + s[7] + s[9];
                self.curveto(s[0], s[1], s[2], s[3], s[4], s[5]);
                let (cx, cy) = (self.x + s[6] + s[8], self.y + s[7] + s[9]);
                let (d5x, d5y) = if dx.abs() > dy.abs() {
                    (s[10], y0 - cy)
                } else {
                    (x0 - cx, s[10])
                };
                self.curveto(s[6], s[7], s[8], s[9], d5x, d5y);
                self.stack = s;
                self.stack.clear();
            }
            // Arithmetic. Present in the specification, near-absent from real
            // fonts; the storage, boolean and random operators are not
            // implemented at all and make the glyph fail rather than silently
            // produce a wrong outline.
            9 => {
                let v = self.stack.last_mut()?;
                *v = v.abs();
            }
            10 | 11 | 24 | 12 => {
                let b = self.stack.pop()?;
                let a = self.stack.pop()?;
                self.stack.push(match op {
                    10 => a + b,
                    11 => a - b,
                    24 => a * b,
                    _ => a / b,
                });
            }
            14 => {
                let v = self.stack.last_mut()?;
                *v = -*v;
            }
            18 => {
                self.stack.pop()?;
            }
            26 => {
                let v = self.stack.last_mut()?;
                *v = v.abs().sqrt();
            }
            27 => {
                let v = *self.stack.last()?;
                self.stack.push(v);
            }
            28 => {
                let n = self.stack.len();
                if n < 2 {
                    return None;
                }
                self.stack.swap(n - 1, n - 2);
            }
            _ => return None,
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opentype::tests::{assemble, be16, be32, head_table, hhea_table, maxp_table};
    use hane_geom::fuzz::{Rng, check};

    // --- TrueType fixtures.

    /// A simple glyph from `(x, y, on_curve)` triples, one contour per slice.
    fn simple(contours: &[&[(i16, i16, bool)]]) -> Vec<u8> {
        let mut g = Vec::new();
        be16(&mut g, contours.len() as u16);
        for _ in 0..4 {
            be16(&mut g, 0); // bounding box, which nothing here reads
        }
        let mut end = 0usize;
        for c in contours {
            end += c.len();
            be16(&mut g, end as u16 - 1);
        }
        be16(&mut g, 0); // instructionLength
        let points: Vec<_> = contours.iter().flat_map(|c| c.iter()).collect();
        // Longest encoding throughout: every delta is a signed 16-bit word, so
        // the flag byte carries only the on-curve bit.
        for p in &points {
            g.push(u8::from(p.2));
        }
        let (mut px, mut py) = (0i16, 0i16);
        for p in &points {
            be16(&mut g, (p.0 - px) as u16);
            px = p.0;
        }
        for p in &points {
            be16(&mut g, (p.1 - py) as u16);
            py = p.1;
        }
        g
    }

    /// A composite referencing `parts` as `(glyph, dx, dy, scale)`.
    fn composite(parts: &[(u16, i16, i16, Option<f64>)]) -> Vec<u8> {
        let mut g = Vec::new();
        be16(&mut g, (-1i16) as u16);
        for _ in 0..4 {
            be16(&mut g, 0);
        }
        for (i, &(gid, dx, dy, scale)) in parts.iter().enumerate() {
            let more = if i + 1 < parts.len() { 0x0020 } else { 0 };
            let scaled = if scale.is_some() { 0x0008 } else { 0 };
            be16(&mut g, 0x0001 | 0x0002 | more | scaled);
            be16(&mut g, gid);
            be16(&mut g, dx as u16);
            be16(&mut g, dy as u16);
            if let Some(s) = scale {
                be16(&mut g, ((s * 16384.0) as i16) as u16);
            }
        }
        g
    }

    /// A font whose `glyf` holds `glyphs`, with a matching long-format `loca`.
    fn truetype_font(glyphs: &[Vec<u8>]) -> Vec<u8> {
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        be32(&mut loca, 0);
        for g in glyphs {
            glyf.extend_from_slice(g);
            glyf.resize(glyf.len().next_multiple_of(4), 0);
            be32(&mut loca, glyf.len() as u32);
        }
        assemble(&[
            (b"glyf", glyf),
            (b"head", head_table()),
            (b"hhea", hhea_table(1)),
            (b"loca", loca),
            (b"maxp", maxp_table(glyphs.len() as u16)),
        ])
    }

    fn p(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    // --- TrueType tests.

    #[test]
    fn a_square_is_four_lines_and_a_close() {
        let data = truetype_font(&[simple(&[&[
            (0, 0, true),
            (100, 0, true),
            (100, 100, true),
            (0, 100, true),
        ]])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::LineTo(p(100.0, 0.0)),
                PathEl::LineTo(p(100.0, 100.0)),
                PathEl::LineTo(p(0.0, 100.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn consecutive_off_curve_points_imply_a_midpoint() {
        // Two control points in a row: the on-curve point between them is not
        // in the file and must be reconstructed at their midpoint. A reader
        // that joins them directly draws a shape that looks like a glyph and
        // is wrong everywhere.
        let data = truetype_font(&[simple(&[&[
            (0, 0, true),
            (100, 0, false),
            (100, 100, false),
            (0, 100, true),
        ]])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::QuadTo(p(100.0, 0.0), p(100.0, 50.0)),
                PathEl::QuadTo(p(100.0, 100.0), p(0.0, 100.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn a_contour_of_only_control_points_starts_at_a_midpoint() {
        // Four off-curve points, the compact way to draw a circle. Every
        // on-curve point is implied, including the start.
        let data = truetype_font(&[simple(&[&[
            (-100, -100, false),
            (100, -100, false),
            (100, 100, false),
            (-100, 100, false),
        ]])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(-100.0, 0.0)),
                PathEl::QuadTo(p(-100.0, -100.0), p(0.0, -100.0)),
                PathEl::QuadTo(p(100.0, -100.0), p(100.0, 0.0)),
                PathEl::QuadTo(p(100.0, 100.0), p(0.0, 100.0)),
                PathEl::QuadTo(p(-100.0, 100.0), p(-100.0, 0.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn a_contour_starting_off_curve_wraps_to_its_first_on_curve_point() {
        let data = truetype_font(&[simple(&[&[
            (50, 0, false),
            (100, 100, true),
            (0, 100, true),
        ]])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(100.0, 100.0)),
                PathEl::LineTo(p(0.0, 100.0)),
                PathEl::QuadTo(p(50.0, 0.0), p(100.0, 100.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn composites_place_and_scale_their_components() {
        let square = simple(&[&[(0, 0, true), (10, 0, true), (10, 10, true)]]);
        // 1.5 rather than 2: component scales are 2.14 fixed point, whose
        // range stops just short of 2.
        let data = truetype_font(&[
            square,
            composite(&[(0, 100, 200, None), (0, 0, 0, Some(1.5))]),
        ]);
        let font = Font::parse(&data).unwrap();
        let out = font.glyph_outline(1).unwrap();
        assert_eq!(out.len(), 8);
        // First component: translated only, offsets unscaled by default.
        assert_eq!(out[0], PathEl::MoveTo(p(100.0, 200.0)));
        assert_eq!(out[1], PathEl::LineTo(p(110.0, 200.0)));
        // Second: scaled about the origin.
        assert_eq!(out[4], PathEl::MoveTo(p(0.0, 0.0)));
        assert_eq!(out[5], PathEl::LineTo(p(15.0, 0.0)));
        assert_eq!(out[6], PathEl::LineTo(p(15.0, 15.0)));
    }

    #[test]
    fn nested_composites_compose_their_transforms() {
        let unit = simple(&[&[(0, 0, true), (10, 0, true), (10, 10, true)]]);
        let data = truetype_font(&[
            unit,
            composite(&[(0, 5, 0, Some(1.5))]),
            composite(&[(1, 1, 1, Some(0.5))]),
        ]);
        let font = Font::parse(&data).unwrap();
        let out = font.glyph_outline(2).unwrap();
        // The inner offset is scaled by the outer transform, the outer offset
        // is not: 0.5*(1.5*10 + 5) + 1 for the second point's x.
        assert_eq!(out[0], PathEl::MoveTo(p(0.5 * 5.0 + 1.0, 1.0)));
        assert_eq!(out[1], PathEl::LineTo(p(0.5 * 20.0 + 1.0, 1.0)));
    }

    #[test]
    fn an_empty_loca_entry_is_a_blank_glyph_not_an_error() {
        let data = truetype_font(&[Vec::new(), simple(&[&[(0, 0, true), (1, 1, true)]])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.glyph_outline(0), Some(Vec::new()));
        assert!(!font.glyph_outline(1).unwrap().is_empty());
        assert_eq!(font.glyph_outline(2), None); // past numGlyphs
    }

    #[test]
    fn a_composite_cycle_terminates() {
        // Two glyphs referencing each other. Depth-limited rather than
        // detected: a diamond is legal and a cycle is not worth a visited set.
        let data = truetype_font(&[composite(&[(1, 0, 0, None)]), composite(&[(0, 0, 0, None)])]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.glyph_outline(0), None);
    }

    // --- CFF fixtures.

    /// A CFF INDEX over `items`, with the one-byte offset size.
    fn cff_index(items: &[Vec<u8>]) -> Vec<u8> {
        let mut t = Vec::new();
        be16(&mut t, items.len() as u16);
        if items.is_empty() {
            return t;
        }
        let total: usize = items.iter().map(Vec::len).sum();
        assert!(total < 250, "fixture needs a wider offset size");
        t.push(1);
        let mut offset = 1u8;
        t.push(offset);
        for item in items {
            offset += item.len() as u8;
            t.push(offset);
        }
        for item in items {
            t.extend_from_slice(item);
        }
        t
    }

    /// A charstring integer in the two-byte form, which covers -1131..=1131.
    fn cs_num(out: &mut Vec<u8>, v: i32) {
        if (-107..=107).contains(&v) {
            out.push((v + 139) as u8);
        } else {
            // 16.16 fixed, the only encoding that needs no range analysis.
            out.push(255);
            be32(out, ((v as i64) << 16) as u32);
        }
    }

    /// Assemble a `CFF ` table around its INDEXes and an optional charset.
    ///
    /// Every offset in the Top DICT is written in the five-byte form, so the
    /// DICT's length does not change when the offsets do and the layout
    /// converges after one trial pass.
    fn cff_table(
        charstrings: &[Vec<u8>],
        subrs: &[Vec<u8>],
        gsubrs: &[Vec<u8>],
        charset: &[u8],
    ) -> Vec<u8> {
        // DICT operand 29 is a 32-bit integer -- unlike charstring operand 255,
        // which is 16.16 fixed point. The two formats share the idea and not
        // the encoding.
        let dict_offset = |out: &mut Vec<u8>, v: usize| {
            out.push(29);
            be32(out, v as u32);
        };
        let mut top_len = 0usize;
        loop {
            let header = vec![1u8, 0, 4, 1];
            let names = cff_index(&[b"Test".to_vec()]);
            let strings = cff_index(&[]);
            let gsubr_index = cff_index(gsubrs);
            let tops = cff_index(&[vec![0u8; top_len]]);

            let private_at =
                header.len() + names.len() + tops.len() + strings.len() + gsubr_index.len();
            // The Subrs offset is relative to the Private DICT, not the table.
            let pd = vec![139 + 2u8, 19];
            let mut private = pd.clone();
            private.extend_from_slice(&cff_index(subrs));

            let charstrings_at = private_at + private.len();
            let cs = cff_index(charstrings);
            let charset_at = charstrings_at + cs.len();

            let mut top = Vec::new();
            dict_offset(&mut top, charstrings_at);
            top.push(17); // CharStrings
            top.push(139 + pd.len() as u8);
            dict_offset(&mut top, private_at);
            top.push(18); // Private
            if !charset.is_empty() {
                dict_offset(&mut top, charset_at);
                top.push(15); // charset
            }

            if top.len() != top_len {
                top_len = top.len();
                continue;
            }
            let mut out = header;
            out.extend_from_slice(&names);
            out.extend_from_slice(&cff_index(&[top]));
            out.extend_from_slice(&strings);
            out.extend_from_slice(&gsubr_index);
            out.extend_from_slice(&private);
            out.extend_from_slice(&cs);
            out.extend_from_slice(charset);
            return out;
        }
    }

    fn cff_font(charstrings: &[Vec<u8>], subrs: &[Vec<u8>], gsubrs: &[Vec<u8>]) -> Vec<u8> {
        cff_font_with(charstrings, subrs, gsubrs, &[])
    }

    fn cff_font_with(
        charstrings: &[Vec<u8>],
        subrs: &[Vec<u8>],
        gsubrs: &[Vec<u8>],
        charset: &[u8],
    ) -> Vec<u8> {
        assemble(&[
            (b"CFF ", cff_table(charstrings, subrs, gsubrs, charset)),
            (b"head", head_table()),
            (b"hhea", hhea_table(1)),
            (b"maxp", maxp_table(charstrings.len() as u16)),
        ])
    }

    /// A charstring from `(operands, operator)` pairs.
    fn cs(program: &[(&[i32], u8)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (args, op) in program {
            for &a in *args {
                cs_num(&mut out, a);
            }
            out.push(*op);
        }
        out
    }

    // --- CFF tests.

    #[test]
    fn a_charstring_draws_lines_and_curves() {
        let data = cff_font(
            &[cs(&[
                (&[50, 100], 21),              // rmoveto
                (&[100, 0], 5),                // rlineto
                (&[10, 20, 30, 40, 50, 0], 8), // rrcurveto
                (&[], 14),                     // endchar
            ])],
            &[],
            &[],
        );
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(50.0, 100.0)),
                PathEl::LineTo(p(150.0, 100.0)),
                PathEl::CurveTo(p(160.0, 120.0), p(190.0, 160.0), p(240.0, 160.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn alternating_curves_infer_the_implied_coordinate() {
        // hvcurveto: each group of four leaves one end-point coordinate
        // implied, and the direction flips every group.
        let data = cff_font(
            &[cs(&[
                (&[0, 0], 21),
                (&[10, 20, 30, 40, 50, 60, 70, 80], 31),
                (&[], 14),
            ])],
            &[],
            &[],
        );
        let font = Font::parse(&data).unwrap();
        let out = font.glyph_outline(0).unwrap();
        // First group is horizontal: the first control shares the start's y
        // and the end point shares the second control's x.
        assert_eq!(
            out[1],
            PathEl::CurveTo(p(10.0, 0.0), p(30.0, 30.0), p(30.0, 70.0))
        );
        // Second group is vertical, so the roles swap.
        assert_eq!(
            out[2],
            PathEl::CurveTo(p(30.0, 120.0), p(90.0, 190.0), p(170.0, 190.0))
        );
    }

    #[test]
    fn subroutines_resolve_through_the_index_bias() {
        // The callee is stored at index 0, which a charstring names as -107
        // because subroutine numbers are biased by the INDEX size.
        let body = cs(&[(&[100, 0], 5), (&[0, 100], 5)]);
        let mut body = body;
        body.push(11); // return
        let global = {
            let mut g = Vec::new();
            cs_num(&mut g, -107);
            g.push(10); // callsubr, from inside a global subroutine
            g.push(11);
            g
        };
        let data = cff_font(
            &[cs(&[(&[0, 0], 21), (&[-107], 29), (&[], 14)])],
            &[body],
            &[global],
        );
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::LineTo(p(100.0, 0.0)),
                PathEl::LineTo(p(100.0, 100.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn hint_operators_consume_their_operands_and_their_mask() {
        // Four stems then a hintmask: one mask byte follows inline. Reading it
        // as an operator instead would decode 0xff as a 16.16 number and take
        // the next four bytes with it.
        let mut program = cs(&[(&[0, 10, 20, 30], 1), (&[40, 50, 60, 70], 18)]);
        program.push(19); // hintmask
        program.push(0xff);
        program.extend_from_slice(&cs(&[(&[5, 5], 21), (&[10, 0], 5), (&[], 14)]));
        let data = cff_font(&[program], &[], &[]);
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap(),
            vec![
                PathEl::MoveTo(p(5.0, 5.0)),
                PathEl::LineTo(p(15.0, 5.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn a_leading_width_operand_is_not_a_coordinate() {
        // rmoveto with three operands: the first is the glyph width.
        let data = cff_font(
            &[cs(&[(&[500, 7, 9], 21), (&[1, 0], 5), (&[], 14)])],
            &[],
            &[],
        );
        let font = Font::parse(&data).unwrap();
        assert_eq!(
            font.glyph_outline(0).unwrap()[0],
            PathEl::MoveTo(p(7.0, 9.0))
        );
    }

    #[test]
    fn endchar_with_four_operands_composes_an_accent() {
        // seac: glyph 1 is Standard Encoding code 65 ('A', SID 34) and glyph 2
        // is code 193 (grave, SID 124), placed at the given offset.
        let base = cs(&[(&[0, 0], 21), (&[10, 0], 5), (&[], 14)]);
        let accent = cs(&[(&[0, 0], 21), (&[0, 10], 5), (&[], 14)]);
        let composed = cs(&[(&[30, 40, 65, 193], 14)]);
        // A format 0 charset naming glyphs 1 and 2 as SIDs 34 and 124.
        let mut charset = vec![0u8];
        be16(&mut charset, 34);
        be16(&mut charset, 124);

        let data = cff_font_with(&[composed, base, accent], &[], &[], &charset);
        let font = Font::parse(&data).unwrap();
        let out = font.glyph_outline(0).unwrap();
        assert_eq!(
            out,
            vec![
                PathEl::MoveTo(p(0.0, 0.0)),
                PathEl::LineTo(p(10.0, 0.0)),
                PathEl::ClosePath,
                PathEl::MoveTo(p(30.0, 40.0)),
                PathEl::LineTo(p(30.0, 50.0)),
                PathEl::ClosePath,
            ]
        );
    }

    #[test]
    fn standard_encoding_covers_both_halves() {
        assert_eq!(standard_encoding(32), Some(1)); // space
        assert_eq!(standard_encoding(65), Some(34)); // A
        assert_eq!(standard_encoding(126), Some(95)); // asciitilde
        assert_eq!(standard_encoding(161), Some(96)); // exclamdown
        assert_eq!(standard_encoding(251), Some(149)); // germandbls
        assert_eq!(standard_encoding(0), None);
        assert_eq!(standard_encoding(176), None); // a hole in the upper half
    }

    #[test]
    fn subroutine_bias_matches_the_index_size() {
        assert_eq!(bias(0), 107);
        assert_eq!(bias(1239), 107);
        assert_eq!(bias(1240), 1131);
        assert_eq!(bias(33899), 1131);
        assert_eq!(bias(33900), 32768);
    }

    // --- Robustness.

    /// Pull every glyph, so corruption is exercised past the table directory.
    fn exercise(data: &[u8]) {
        let Ok(font) = Font::parse(data) else {
            return;
        };
        for gid in 0..font.glyph_count().min(8) {
            let _ = font.glyph_outline(gid);
        }
        for gid in [100u16, 1000, u16::MAX] {
            let _ = font.glyph_outline(gid);
        }
    }

    #[test]
    fn corrupted_outlines_do_not_panic() {
        let fonts = [
            truetype_font(&[
                simple(&[
                    &[
                        (0, 0, true),
                        (100, 0, false),
                        (100, 100, false),
                        (0, 100, true),
                    ],
                    &[(10, 10, false), (20, 20, false), (30, 10, false)],
                ]),
                composite(&[(0, 5, 5, Some(1.5)), (0, -5, -5, None)]),
            ]),
            cff_font(
                &[cs(&[
                    (&[10, 20, 30, 40], 1),
                    (&[50, 50], 21),
                    (&[10, 20, 30, 40, 50, 60, 70, 80], 31),
                    (&[100], 6),
                    (&[], 14),
                ])],
                &[{
                    let mut s = cs(&[(&[5, 5], 5)]);
                    s.push(11);
                    s
                }],
                &[],
            ),
        ];
        for (i, base) in fonts.iter().enumerate() {
            let name = format!("corrupted outline {i}");
            check(
                &name,
                6_000,
                |r: &mut Rng| {
                    let mut data = base.clone();
                    for _ in 0..1 + r.below(10) {
                        let at = r.below(data.len() as u64) as usize;
                        data[at] = r.below(256) as u8;
                    }
                    if r.below(4) == 0 {
                        data.truncate(r.below(data.len() as u64) as usize);
                    }
                    data
                },
                |data| {
                    exercise(data);
                    true
                },
            );
            // Every truncation as well: the byte flips above rarely produce a
            // table that ends exactly mid-structure.
            for len in 0..base.len() {
                exercise(&base[..len]);
            }
        }
    }

    /// Extract every glyph of every font installed on this machine.
    ///
    /// Skipped where there are none, which includes CI, so it is a local smoke
    /// test rather than a gate.
    #[test]
    fn installed_fonts_yield_outlines() {
        let mut files = Vec::new();
        collect_fonts(std::path::Path::new("/usr/share/fonts"), &mut files);
        if files.len() < 20 {
            return;
        }
        let (mut faces, mut glyphs, mut failed) = (0usize, 0usize, Vec::new());
        for path in &files {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            for i in 0..Font::count(&data) {
                let Ok(font) = Font::parse_index(&data, i) else {
                    continue;
                };
                // A colour-emoji font has no outline table at all: its glyphs
                // are PNGs in `CBDT`. Nothing to extract is not a failure.
                if ["glyf", "CFF ", "CFF2"]
                    .iter()
                    .all(|t| font.table(t).is_none())
                {
                    continue;
                }
                faces += 1;
                let mut bad = 0usize;
                for gid in 0..font.glyph_count() {
                    match font.glyph_outline(gid) {
                        Some(_) => glyphs += 1,
                        None => bad += 1,
                    }
                }
                if bad > 0 {
                    failed.push((path.clone(), bad));
                }
            }
        }
        assert!(glyphs > 100_000, "only {glyphs} glyphs from {faces} faces");
        assert!(failed.is_empty(), "glyphs failed to extract: {failed:?}");
    }

    fn collect_fonts(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_fonts(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("ttf" | "otf" | "ttc" | "otc")
            ) {
                out.push(path);
            }
        }
    }
}
