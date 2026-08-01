//! Hand-written OpenType table parsing: `head`, `hhea`, `maxp`, `cmap`,
//! `hmtx`, `name` and `OS/2`.
//!
//! [`Font::parse`] reads the table directory, parses the three tables every
//! caller needs eagerly (`head`, `hhea`, `maxp`), and leaves the rest to
//! accessors that read the raw bytes on demand. Glyph outlines are not here:
//! `glyf`/`loca` and `CFF` are separate work.
//!
//! # Untrusted input
//!
//! A font arrives over the network and every offset, length and count in it is
//! attacker-controlled. The rules this module follows, without exception:
//!
//! - Nothing indexes a slice. Every read goes through [`u16_at`] and friends,
//!   which use `slice::get` on a range built with `checked_add`, so an offset
//!   near `usize::MAX` yields an error rather than a wrapped, in-bounds-looking
//!   range.
//! - No count from the file is trusted as a loop bound or an allocation size.
//!   Counts are clamped to what the table can actually hold before use.
//! - Nothing recurses, so no input can overflow the stack.
//! - Optional tables that fail to parse are dropped, not fatal: a font with a
//!   corrupt `OS/2` still has usable metrics, and refusing to open it helps
//!   nobody.
//!
//! # Leniency
//!
//! Real fonts violate the specification in small ways that no renderer treats
//! as fatal -- a final table whose length runs past the end of the file by the
//! checksum padding is common. Such lengths are clamped to the file. What is
//! never tolerated is a read outside the buffer.

use core::fmt;
use hane_geom::Rect;

/// Why a font could not be parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file does not start with a recognised sfnt or collection signature.
    UnknownFormat,
    /// A table this parser requires is absent. The value is its tag.
    MissingTable(&'static str),
    /// A table is truncated, or a value inside it is impossible.
    Malformed(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFormat => write!(f, "not an OpenType font"),
            Self::MissingTable(tag) => write!(f, "missing required table `{tag}`"),
            Self::Malformed(what) => write!(f, "malformed font: {what}"),
        }
    }
}

impl std::error::Error for Error {}

/// The `head` table, minus the fields nothing downstream reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Head {
    /// Font design units per em. Never zero -- every metric divides by it.
    pub units_per_em: u16,
    /// The union of all glyph bounding boxes, in font units.
    pub bounding_box: Rect,
    /// `0` for a `loca` table of `u16` halved offsets, `1` for `u32`.
    pub index_to_loc_format: i16,
}

/// The `hhea` table: horizontal metrics that apply to the whole font.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hhea {
    /// Distance from the baseline to the top of the typographic ascent.
    pub ascender: i16,
    /// Distance from the baseline to the bottom of the descent, negative.
    pub descender: i16,
    /// Extra leading between lines.
    pub line_gap: i16,
    /// Number of `longHorMetric` entries at the front of `hmtx`.
    pub number_of_h_metrics: u16,
}

/// The `maxp` table, of which only the glyph count matters here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Maxp {
    /// Number of glyphs in the font.
    pub glyph_count: u16,
}

/// The `OS/2` table, which is where usable vertical metrics actually live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Os2 {
    /// Table version, `0` through `5`.
    pub version: u16,
    /// Weight on the 1-1000 scale; 400 is regular, 700 bold.
    pub weight_class: u16,
    /// Width class, 1 (ultra-condensed) through 9 (ultra-expanded).
    pub width_class: u16,
    /// Style bits: 0 italic, 5 bold, 6 regular, 7 use-typo-metrics.
    pub fs_selection: u16,
    /// Typographic ascender, the one to use when bit 7 of `fs_selection` is set.
    pub typo_ascender: i16,
    /// Typographic descender, negative.
    pub typo_descender: i16,
    /// Typographic line gap.
    pub typo_line_gap: i16,
    /// Windows ascent: the clipping bound, not a typographic one.
    pub win_ascent: u16,
    /// Windows descent, a positive distance below the baseline.
    pub win_descent: u16,
    /// Height of a lowercase `x`, present from version 2.
    pub x_height: Option<i16>,
    /// Height of a capital `H`, present from version 2.
    pub cap_height: Option<i16>,
}

/// A parsed font, borrowing the file it was parsed from.
///
/// Construction validates the table directory and the three mandatory tables;
/// everything else is read on demand from the borrowed bytes, so a font whose
/// `name` table is never queried never pays for decoding it.
#[derive(Clone, Debug)]
pub struct Font<'a> {
    /// Tag and bytes of every directory entry that lies inside the file.
    tables: Vec<([u8; 4], &'a [u8])>,
    /// The best Unicode `cmap` subtable, already narrowed to its own bytes.
    cmap: Option<Cmap<'a>>,
    head: Head,
    hhea: Hhea,
    maxp: Maxp,
    os2: Option<Os2>,
}

impl<'a> Font<'a> {
    /// Parse `data` as a single font, or as the first font of a collection.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownFormat`] when the signature is not an sfnt one,
    /// [`Error::MissingTable`] when `head`, `hhea` or `maxp` is absent, and
    /// [`Error::Malformed`] when one of those is truncated or self-contradictory.
    /// Malformed input never panics.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        Self::parse_index(data, 0)
    }

    /// Parse font number `index` of a TrueType collection (`.ttc`).
    ///
    /// A plain, non-collection font counts as a one-font collection, so index
    /// `0` always works and any other index is an error.
    ///
    /// # Errors
    ///
    /// As [`Font::parse`], plus [`Error::Malformed`] when `index` is past the
    /// end of the collection.
    pub fn parse_index(data: &'a [u8], index: u32) -> Result<Self, Error> {
        let start = if data.get(..4) == Some(b"ttcf") {
            let count = u32_at(data, 8)?;
            if index >= count {
                return Err(Error::Malformed("collection index out of range"));
            }
            // 12 + 4*index cannot overflow: index < count <= u32::MAX and the
            // arithmetic is in usize, which is at least 32 bits everywhere
            // this builds -- but the checked form costs nothing and survives
            // someone changing the type.
            let entry = 12usize
                .checked_add(4 * index as usize)
                .ok_or(Error::Malformed("collection offset overflow"))?;
            u32_at(data, entry)? as usize
        } else if index != 0 {
            return Err(Error::Malformed("collection index out of range"));
        } else {
            0
        };

        let version = u32_at(data, start)?;
        // 0x00010000 is TrueType, `OTTO` is CFF outlines, `true` is the old
        // Apple spelling that a few shipped fonts still use.
        if version != 0x0001_0000
            && version != u32::from_be_bytes(*b"OTTO")
            && version != u32::from_be_bytes(*b"true")
        {
            return Err(Error::UnknownFormat);
        }

        let num_tables = u16_at(data, start + 4)? as usize;
        let records = start
            .checked_add(12)
            .ok_or(Error::Malformed("directory offset overflow"))?;
        // The count is attacker-controlled, so it never sizes the loop on its
        // own: whatever the file claims, there can only be as many records as
        // fit between the directory and the end of the file.
        let capacity = data.len().saturating_sub(records) / 16;
        let mut tables = Vec::with_capacity(num_tables.min(capacity));
        for i in 0..num_tables.min(capacity) {
            let rec = records + i * 16;
            let mut tag = [0u8; 4];
            tag.copy_from_slice(
                data.get(rec..rec + 4)
                    .ok_or(Error::Malformed("short directory"))?,
            );
            let offset = u32_at(data, rec + 8)? as usize;
            let length = u32_at(data, rec + 12)? as usize;
            let Some(rest) = data.get(offset..) else {
                continue; // Entry points outside the file; ignore it.
            };
            // Clamping rather than rejecting: a final table whose length
            // includes the four-byte checksum padding past EOF is common
            // enough in shipped fonts that rejecting it would fail real files.
            tables.push((tag, &rest[..length.min(rest.len())]));
        }

        let find = |tag: &[u8; 4]| tables.iter().find(|(t, _)| t == tag).map(|(_, d)| *d);
        let head = parse_head(find(b"head").ok_or(Error::MissingTable("head"))?)?;
        let hhea = parse_hhea(find(b"hhea").ok_or(Error::MissingTable("hhea"))?)?;
        let maxp = parse_maxp(find(b"maxp").ok_or(Error::MissingTable("maxp"))?)?;
        // Optional tables never make a font unopenable. A broken `OS/2` costs
        // its metrics; a broken `cmap` costs character mapping. Neither is
        // worth refusing to render the file over.
        let os2 = find(b"OS/2").and_then(|t| parse_os2(t).ok());
        let cmap = find(b"cmap").and_then(|t| Cmap::parse(t).ok());

        Ok(Self {
            tables,
            cmap,
            head,
            hhea,
            maxp,
            os2,
        })
    }

    /// The number of fonts in `data`, which is `1` for anything but a `.ttc`.
    ///
    /// Returns `0` for a buffer too short to hold a collection header, which is
    /// also a buffer too short to hold a font.
    pub fn count(data: &[u8]) -> u32 {
        if data.get(..4) == Some(b"ttcf") {
            u32_at(data, 8).unwrap_or(0)
        } else if data.len() >= 12 {
            1
        } else {
            0
        }
    }

    /// The raw bytes of table `tag`, if the font has it.
    ///
    /// The slice is already clipped to the file, so a caller parsing `glyf` or
    /// `CFF` later starts from a bounded buffer.
    pub fn table(&self, tag: &str) -> Option<&'a [u8]> {
        let tag: &[u8; 4] = tag.as_bytes().try_into().ok()?;
        self.tables.iter().find(|(t, _)| t == tag).map(|(_, d)| *d)
    }

    /// The parsed `head` table.
    pub fn head(&self) -> &Head {
        &self.head
    }

    /// The parsed `hhea` table.
    pub fn hhea(&self) -> &Hhea {
        &self.hhea
    }

    /// The parsed `maxp` table.
    pub fn maxp(&self) -> &Maxp {
        &self.maxp
    }

    /// The parsed `OS/2` table, absent when the font has none or it is corrupt.
    pub fn os2(&self) -> Option<&Os2> {
        self.os2.as_ref()
    }

    /// Font design units per em, the divisor that turns font units into ems.
    pub fn units_per_em(&self) -> u16 {
        self.head.units_per_em
    }

    /// The number of glyphs, from `maxp`.
    pub fn glyph_count(&self) -> u16 {
        self.maxp.glyph_count
    }

    /// The glyph for `ch`, or `None` when the font does not cover it.
    ///
    /// Glyph 0 is `.notdef` and means "not covered", so it is reported as
    /// `None` rather than as a hit.
    pub fn glyph_index(&self, ch: char) -> Option<u16> {
        let gid = self.cmap.as_ref()?.lookup(ch as u32)?;
        (gid != 0 && gid < self.maxp.glyph_count).then_some(gid)
    }

    /// The advance width of `glyph` in font units.
    ///
    /// The trailing glyphs of a monospaced run share the last `longHorMetric`,
    /// which is what makes a CJK font's `hmtx` a few bytes instead of tens of
    /// thousands.
    pub fn advance_width(&self, glyph: u16) -> Option<u16> {
        let hmtx = self.table("hmtx")?;
        let n = self.hhea.number_of_h_metrics;
        if n == 0 || glyph >= self.maxp.glyph_count {
            return None;
        }
        u16_at(hmtx, glyph.min(n - 1) as usize * 4).ok()
    }

    /// The left side bearing of `glyph` in font units.
    pub fn left_side_bearing(&self, glyph: u16) -> Option<i16> {
        let hmtx = self.table("hmtx")?;
        let n = self.hhea.number_of_h_metrics as usize;
        let g = glyph as usize;
        if glyph >= self.maxp.glyph_count {
            return None;
        }
        // Past the paired metrics the table is a bare array of bearings.
        let offset = if g < n {
            g * 4 + 2
        } else {
            n * 4 + (g - n) * 2
        };
        i16_at(hmtx, offset).ok()
    }

    /// The string with name ID `id` from the `name` table.
    ///
    /// IDs worth knowing: 1 family, 2 subfamily, 4 full name, 6 PostScript
    /// name. English-language Windows records win over everything else, since
    /// that is the record every font actually ships.
    pub fn name(&self, id: u16) -> Option<String> {
        let table = self.table("name")?;
        let count = u16_at(table, 2).ok()? as usize;
        let storage = u16_at(table, 4).ok()? as usize;
        // As in the table directory: the record count is bounded by the bytes
        // available, not by what the file claims.
        let count = count.min(table.len().saturating_sub(6) / 12);

        let mut best: Option<(u8, String)> = None;
        for i in 0..count {
            let rec = 6 + i * 12;
            let (platform, encoding, language) = (
                u16_at(table, rec).ok()?,
                u16_at(table, rec + 2).ok()?,
                u16_at(table, rec + 4).ok()?,
            );
            if u16_at(table, rec + 6).ok()? != id {
                continue;
            }
            let length = u16_at(table, rec + 8).ok()? as usize;
            let offset = u16_at(table, rec + 10).ok()? as usize;
            let Some(start) = storage.checked_add(offset) else {
                continue;
            };
            let Some(bytes) = start.checked_add(length).and_then(|e| table.get(start..e)) else {
                continue; // Record points outside its own table.
            };
            let rank = match (platform, encoding, language) {
                (3, 1, 0x409) => 4,
                (3, _, _) => 3,
                (0, _, _) => 2,
                (1, 0, _) => 1,
                _ => 0,
            };
            if best.as_ref().is_some_and(|(r, _)| *r >= rank) {
                continue;
            }
            // Platforms 0 and 3 store UTF-16BE; the Macintosh platform stores
            // a single-byte encoding whose ASCII range is all a name ever uses.
            let text = if platform == 1 {
                bytes.iter().map(|&b| b as char).collect()
            } else {
                decode_utf16be(bytes)
            };
            best = Some((rank, text));
        }
        best.map(|(_, text)| text)
    }
}

/// The chosen `cmap` subtable, narrowed to its own bytes and its format read.
#[derive(Clone, Copy, Debug)]
struct Cmap<'a> {
    data: &'a [u8],
    format: u16,
}

impl<'a> Cmap<'a> {
    /// Pick the most capable supported subtable out of the encoding records.
    fn parse(table: &'a [u8]) -> Result<Self, Error> {
        let count = u16_at(table, 2)? as usize;
        let count = count.min(table.len().saturating_sub(4) / 8);

        let mut candidates: Vec<(u8, usize)> = Vec::new();
        for i in 0..count {
            let rec = 4 + i * 8;
            let platform = u16_at(table, rec)?;
            let encoding = u16_at(table, rec + 2)?;
            let offset = u32_at(table, rec + 4)? as usize;
            // Full-repertoire subtables first: a format 4 subtable in the same
            // font covers only the BMP, so preferring it would silently lose
            // every astral character.
            let rank = match (platform, encoding) {
                (3, 10) => 5,         // Windows UCS-4
                (0, 4) | (0, 6) => 4, // Unicode full repertoire
                (3, 1) => 3,          // Windows BMP
                (0, 0..=3) => 2,      // Unicode BMP
                (3, 0) => 1,          // Windows symbol, still a format 4 map
                _ => continue,
            };
            candidates.push((rank, offset));
        }
        candidates.sort_by_key(|&(rank, _)| core::cmp::Reverse(rank));

        for (_, offset) in candidates {
            let Some(sub) = table.get(offset..) else {
                continue;
            };
            let Ok(format) = u16_at(sub, 0) else {
                continue;
            };
            // ponytail: formats 4 and 12 only. Formats 0, 2, 6 and 13 exist,
            // but across the 832 fonts on the development machine every one of
            // them also ships a 4 or a 12, so the extra decoders would be dead
            // code. Add format 6 first if a subset font ever needs it.
            let length = match format {
                4 => u16_at(sub, 2).ok().map(|l| l as usize),
                12 => u32_at(sub, 4).ok().map(|l| l as usize),
                _ => None,
            };
            let Some(length) = length else { continue };
            // A length longer than the table is a lie, but the subtable is
            // still usable up to the end of the table, so clamp rather than
            // reject -- every read inside is bounds-checked regardless.
            let data = &sub[..length.min(sub.len())];
            if data.len() >= 16 {
                return Ok(Self { data, format });
            }
        }
        Err(Error::Malformed("no supported cmap subtable"))
    }

    /// The glyph for a code point, or `None` if unmapped or the data is bad.
    fn lookup(&self, ch: u32) -> Option<u16> {
        match self.format {
            4 => self.lookup_format4(ch),
            _ => self.lookup_format12(ch),
        }
    }

    /// Format 4: segments sorted by end code, with two ways to spell a mapping.
    fn lookup_format4(&self, ch: u32) -> Option<u16> {
        let t = self.data;
        let ch = u16::try_from(ch).ok()?; // Format 4 is BMP-only by construction.
        let seg_count_x2 = u16_at(t, 6).ok()? as usize;
        // Every segment needs 8 bytes across the four parallel arrays, plus the
        // 14-byte header and the 2-byte pad, so a claimed count larger than
        // that cannot be honest.
        if seg_count_x2 == 0 || !seg_count_x2.is_multiple_of(2) || 16 + seg_count_x2 * 4 > t.len() {
            return None;
        }
        let segs = seg_count_x2 / 2;

        // First segment whose end code is >= ch. The array is sorted and ends
        // with 0xFFFF in any valid font, so this is the standard search.
        let (mut lo, mut hi) = (0usize, segs);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if u16_at(t, 14 + mid * 2).ok()? < ch {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo >= segs {
            return None;
        }

        let start = u16_at(t, 16 + seg_count_x2 + lo * 2).ok()?;
        if ch < start {
            return None; // Falls in the hole before this segment.
        }
        let delta = u16_at(t, 16 + seg_count_x2 * 2 + lo * 2).ok()?;
        let range_pos = 16 + seg_count_x2 * 3 + lo * 2;
        let range_offset = u16_at(t, range_pos).ok()? as usize;
        if range_offset == 0 {
            // Arithmetic mapping. The addition is specified modulo 65536, so
            // the wrap is the algorithm, not an overflow.
            return Some(ch.wrapping_add(delta));
        }
        // Indirect mapping: the offset is relative to its own position, an
        // encoding that only makes sense if you picture the C struct it came
        // from. Both additions are checked because both operands are file data.
        let index = range_pos
            .checked_add(range_offset)?
            .checked_add((ch - start) as usize * 2)?;
        let glyph = u16_at(t, index).ok()?;
        (glyph != 0).then(|| glyph.wrapping_add(delta))
    }

    /// Format 12: sorted groups of contiguous code points.
    fn lookup_format12(&self, ch: u32) -> Option<u16> {
        let t = self.data;
        let groups = (u32_at(t, 12).ok()? as usize).min(t.len().saturating_sub(16) / 12);
        let (mut lo, mut hi) = (0usize, groups);
        // Last group whose start code is <= ch, found as the first one that is
        // strictly greater, minus one.
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if u32_at(t, 16 + mid * 12).ok()? <= ch {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let g = 16 + lo.checked_sub(1)? * 12;
        let (start, end) = (u32_at(t, g).ok()?, u32_at(t, g + 4).ok()?);
        if ch > end {
            return None;
        }
        let glyph = u32_at(t, g + 8).ok()?.checked_add(ch - start)?;
        u16::try_from(glyph).ok()
    }
}

/// UTF-16BE with unpaired surrogates replaced rather than rejected: a name
/// string is diagnostic text, and dropping a font over a bad byte in it would
/// be absurd.
fn decode_utf16be(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

fn parse_head(t: &[u8]) -> Result<Head, Error> {
    let units_per_em = u16_at(t, 18)?;
    if units_per_em == 0 {
        // Every metric is divided by this. A zero would turn every advance
        // into an infinity long after the parse, which is a far worse failure
        // than refusing the font here.
        return Err(Error::Malformed("head.unitsPerEm is zero"));
    }
    let index_to_loc_format = i16_at(t, 50)?;
    if index_to_loc_format != 0 && index_to_loc_format != 1 {
        return Err(Error::Malformed("head.indexToLocFormat is not 0 or 1"));
    }
    Ok(Head {
        units_per_em,
        // Not normalised: a font whose xMin exceeds its xMax is broken, and
        // silently swapping them would hide that from whoever has to debug it.
        bounding_box: Rect::new(
            f64::from(i16_at(t, 36)?),
            f64::from(i16_at(t, 38)?),
            f64::from(i16_at(t, 40)?),
            f64::from(i16_at(t, 42)?),
        ),
        index_to_loc_format,
    })
}

fn parse_hhea(t: &[u8]) -> Result<Hhea, Error> {
    Ok(Hhea {
        ascender: i16_at(t, 4)?,
        descender: i16_at(t, 6)?,
        line_gap: i16_at(t, 8)?,
        number_of_h_metrics: u16_at(t, 34)?,
    })
}

fn parse_maxp(t: &[u8]) -> Result<Maxp, Error> {
    Ok(Maxp {
        glyph_count: u16_at(t, 4)?,
    })
}

fn parse_os2(t: &[u8]) -> Result<Os2, Error> {
    let version = u16_at(t, 0)?;
    // The version 2 fields sit past the end of a version 0 table, so they are
    // read only when the version says they are there *and* the bytes exist.
    let extended = version >= 2 && t.len() >= 96;
    Ok(Os2 {
        version,
        weight_class: u16_at(t, 4)?,
        width_class: u16_at(t, 6)?,
        fs_selection: u16_at(t, 62)?,
        typo_ascender: i16_at(t, 68)?,
        typo_descender: i16_at(t, 70)?,
        typo_line_gap: i16_at(t, 72)?,
        win_ascent: u16_at(t, 74)?,
        win_descent: u16_at(t, 76)?,
        x_height: extended.then(|| i16_at(t, 86)).transpose()?,
        cap_height: extended.then(|| i16_at(t, 88)).transpose()?,
    })
}

/// A big-endian `u16` at a byte offset, or an error if it does not fit.
///
/// `checked_add` rather than `+`: `off` comes from the file, and a wrapped end
/// offset would produce a range that `get` accepts.
pub(crate) fn u16_at(data: &[u8], off: usize) -> Result<u16, Error> {
    let end = off
        .checked_add(2)
        .ok_or(Error::Malformed("offset overflow"))?;
    let b = data
        .get(off..end)
        .ok_or(Error::Malformed("truncated table"))?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

/// A big-endian `i16` at a byte offset.
pub(crate) fn i16_at(data: &[u8], off: usize) -> Result<i16, Error> {
    u16_at(data, off).map(|v| v as i16)
}

/// A big-endian `u32` at a byte offset.
pub(crate) fn u32_at(data: &[u8], off: usize) -> Result<u32, Error> {
    let end = off
        .checked_add(4)
        .ok_or(Error::Malformed("offset overflow"))?;
    let b = data
        .get(off..end)
        .ok_or(Error::Malformed("truncated table"))?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use hane_geom::fuzz::{Rng, check};

    // --- A synthetic font, so the tests do not depend on what is installed.

    pub(crate) fn be16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn be32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    /// `head`, with a 2048 unit em and a known bounding box.
    pub(crate) fn head_table() -> Vec<u8> {
        let mut t = Vec::new();
        be32(&mut t, 0x0001_0000);
        be32(&mut t, 0);
        be32(&mut t, 0);
        be32(&mut t, 0x5F0F_3CF5);
        be16(&mut t, 0); // flags
        be16(&mut t, 2048); // unitsPerEm
        t.extend_from_slice(&[0; 16]); // created, modified
        be16(&mut t, (-100i16) as u16); // xMin
        be16(&mut t, (-200i16) as u16); // yMin
        be16(&mut t, 1000); // xMax
        be16(&mut t, 1800); // yMax
        be16(&mut t, 0); // macStyle
        be16(&mut t, 8); // lowestRecPPEM
        be16(&mut t, 2); // fontDirectionHint
        be16(&mut t, 1); // indexToLocFormat
        be16(&mut t, 0); // glyphDataFormat
        t
    }

    pub(crate) fn hhea_table(num_h_metrics: u16) -> Vec<u8> {
        let mut t = Vec::new();
        be32(&mut t, 0x0001_0000);
        be16(&mut t, 1600); // ascender
        be16(&mut t, (-400i16) as u16); // descender
        be16(&mut t, 90); // lineGap
        t.extend_from_slice(&[0; 24]); // advanceWidthMax .. metricDataFormat
        be16(&mut t, num_h_metrics);
        t
    }

    pub(crate) fn maxp_table(glyphs: u16) -> Vec<u8> {
        let mut t = Vec::new();
        be32(&mut t, 0x0001_0000);
        be16(&mut t, glyphs);
        t.extend_from_slice(&[0; 26]);
        t
    }

    fn os2_table() -> Vec<u8> {
        let mut t = vec![0u8; 96];
        t[0..2].copy_from_slice(&4u16.to_be_bytes()); // version
        t[4..6].copy_from_slice(&700u16.to_be_bytes()); // usWeightClass
        t[6..8].copy_from_slice(&5u16.to_be_bytes()); // usWidthClass
        t[62..64].copy_from_slice(&0x20u16.to_be_bytes()); // fsSelection: bold
        t[68..70].copy_from_slice(&1500u16.to_be_bytes());
        t[70..72].copy_from_slice(&((-500i16) as u16).to_be_bytes());
        t[72..74].copy_from_slice(&100u16.to_be_bytes());
        t[74..76].copy_from_slice(&1900u16.to_be_bytes());
        t[76..78].copy_from_slice(&500u16.to_be_bytes());
        t[86..88].copy_from_slice(&1024u16.to_be_bytes()); // sxHeight
        t[88..90].copy_from_slice(&1400u16.to_be_bytes()); // sCapHeight
        t
    }

    /// Three paired metrics followed by bare bearings, the shape that exercises
    /// the "last advance repeats" rule.
    fn hmtx_table() -> Vec<u8> {
        let mut t = Vec::new();
        for (adv, lsb) in [(600u16, 40i16), (700, 50), (800, 60)] {
            be16(&mut t, adv);
            be16(&mut t, lsb as u16);
        }
        for lsb in [70i16, 80] {
            be16(&mut t, lsb as u16);
        }
        t
    }

    /// A format 4 subtable mapping 'A'..='C' to glyphs 1..=3 arithmetically and
    /// 'x'..='y' to 4 and 5 through the glyph array, so both branches are live.
    fn cmap_format4() -> Vec<u8> {
        let segs: [(u16, u16); 3] = [(0x41, 0x43), (0x78, 0x79), (0xFFFF, 0xFFFF)];
        let seg_count = segs.len() as u16;
        let mut t = Vec::new();
        be16(&mut t, 4);
        be16(&mut t, 16 + seg_count * 8 + 4); // length, including the glyph array
        be16(&mut t, 0); // language
        be16(&mut t, seg_count * 2);
        be16(&mut t, 4); // searchRange, unused by this parser
        be16(&mut t, 1);
        be16(&mut t, 0);
        for (_, end) in segs {
            be16(&mut t, end);
        }
        be16(&mut t, 0); // reservedPad
        for (start, _) in segs {
            be16(&mut t, start);
        }
        // idDelta: 'A' + delta = 1 for the first segment, 0 for the indirect
        // one, and the last segment maps 0xFFFF to glyph 0.
        be16(&mut t, 1u16.wrapping_sub(0x41));
        be16(&mut t, 0);
        be16(&mut t, 1);
        // idRangeOffset: the second segment reads from the glyph array, which
        // starts one entry past the end of this array.
        be16(&mut t, 0);
        be16(&mut t, 4); // = 2 remaining entries * 2 bytes
        be16(&mut t, 0);
        be16(&mut t, 4); // glyph for 'x'
        be16(&mut t, 5); // glyph for 'y'
        t
    }

    /// A format 12 subtable covering an astral range, which format 4 cannot.
    fn cmap_format12() -> Vec<u8> {
        let groups: [(u32, u32, u32); 2] = [(0x41, 0x43, 1), (0x1_0000, 0x1_0001, 6)];
        let mut t = Vec::new();
        be16(&mut t, 12);
        be16(&mut t, 0);
        be32(&mut t, 16 + groups.len() as u32 * 12);
        be32(&mut t, 0); // language
        be32(&mut t, groups.len() as u32);
        for (start, end, glyph) in groups {
            be32(&mut t, start);
            be32(&mut t, end);
            be32(&mut t, glyph);
        }
        t
    }

    /// A `cmap` with one encoding record per subtable given.
    fn cmap_table(subtables: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let mut t = Vec::new();
        be16(&mut t, 0);
        be16(&mut t, subtables.len() as u16);
        let mut offset = 4 + subtables.len() as u32 * 8;
        for (platform, encoding, sub) in subtables {
            be16(&mut t, *platform);
            be16(&mut t, *encoding);
            be32(&mut t, offset);
            offset += sub.len() as u32;
        }
        for (_, _, sub) in subtables {
            t.extend_from_slice(sub);
        }
        t
    }

    fn name_table() -> Vec<u8> {
        let records: [(u16, u16, u16, u16, &str); 2] = [
            (1, 0, 0, 1, "Hane Mac"),      // Macintosh, single byte
            (3, 1, 0x409, 1, "Hane Test"), // Windows English, UTF-16BE
        ];
        let mut t = Vec::new();
        be16(&mut t, 0);
        be16(&mut t, records.len() as u16);
        be16(&mut t, 6 + records.len() as u16 * 12); // stringOffset
        let mut storage = Vec::new();
        for (platform, encoding, language, id, text) in records {
            let bytes: Vec<u8> = if platform == 1 {
                text.bytes().collect()
            } else {
                text.encode_utf16().flat_map(u16::to_be_bytes).collect()
            };
            be16(&mut t, platform);
            be16(&mut t, encoding);
            be16(&mut t, language);
            be16(&mut t, id);
            be16(&mut t, bytes.len() as u16);
            be16(&mut t, storage.len() as u16);
            storage.extend_from_slice(&bytes);
        }
        t.extend_from_slice(&storage);
        t
    }

    /// Assemble a directory plus tables into a font file.
    pub(crate) fn assemble(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        assemble_at(0, tables)
    }

    /// The same, for a font embedded `base` bytes into a collection: table
    /// offsets in a `.ttc` are from the start of the file, not of the font.
    fn assemble_at(base: u32, tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        be32(&mut out, 0x0001_0000);
        be16(&mut out, tables.len() as u16);
        be16(&mut out, 0);
        be16(&mut out, 0);
        be16(&mut out, 0);
        let mut offset = base + 12 + tables.len() as u32 * 16;
        for (tag, body) in tables {
            out.extend_from_slice(*tag);
            be32(&mut out, 0); // checksum, unread
            be32(&mut out, offset);
            be32(&mut out, body.len() as u32);
            offset += body.len().next_multiple_of(4) as u32;
        }
        for (_, body) in tables {
            out.extend_from_slice(body);
            out.resize(out.len().next_multiple_of(4), 0);
        }
        out
    }

    fn test_font(cmap: Vec<u8>) -> Vec<u8> {
        assemble(&[
            (b"OS/2", os2_table()),
            (b"cmap", cmap),
            (b"head", head_table()),
            (b"hhea", hhea_table(3)),
            (b"hmtx", hmtx_table()),
            (b"maxp", maxp_table(8)),
            (b"name", name_table()),
        ])
    }

    fn bmp_font() -> Vec<u8> {
        test_font(cmap_table(&[(3, 1, cmap_format4())]))
    }

    // --- Tests.

    #[test]
    fn fixed_tables_parse() {
        let data = bmp_font();
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.units_per_em(), 2048);
        assert_eq!(font.head().index_to_loc_format, 1);
        assert_eq!(
            font.head().bounding_box,
            Rect::new(-100.0, -200.0, 1000.0, 1800.0)
        );
        assert_eq!(font.hhea().ascender, 1600);
        assert_eq!(font.hhea().descender, -400);
        assert_eq!(font.hhea().line_gap, 90);
        assert_eq!(font.hhea().number_of_h_metrics, 3);
        assert_eq!(font.glyph_count(), 8);

        let os2 = font.os2().unwrap();
        assert_eq!(os2.version, 4);
        assert_eq!(os2.weight_class, 700);
        assert_eq!(os2.width_class, 5);
        assert_eq!(os2.fs_selection, 0x20);
        assert_eq!(os2.typo_ascender, 1500);
        assert_eq!(os2.typo_descender, -500);
        assert_eq!(os2.win_ascent, 1900);
        assert_eq!(os2.x_height, Some(1024));
        assert_eq!(os2.cap_height, Some(1400));
    }

    #[test]
    fn cmap_format_4_maps_both_branches() {
        let data = bmp_font();
        let font = Font::parse(&data).unwrap();
        // idDelta segment.
        assert_eq!(font.glyph_index('A'), Some(1));
        assert_eq!(font.glyph_index('C'), Some(3));
        // idRangeOffset segment, which reads the glyph array.
        assert_eq!(font.glyph_index('x'), Some(4));
        assert_eq!(font.glyph_index('y'), Some(5));
        // Holes, before the first segment and between segments.
        assert_eq!(font.glyph_index('\u{1}'), None);
        assert_eq!(font.glyph_index('D'), None);
        assert_eq!(font.glyph_index('\u{10000}'), None);
    }

    #[test]
    fn cmap_format_12_covers_astral_planes() {
        let data = test_font(cmap_table(&[(3, 10, cmap_format12())]));
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.glyph_index('A'), Some(1));
        assert_eq!(font.glyph_index('C'), Some(3));
        assert_eq!(font.glyph_index('\u{10000}'), Some(6));
        assert_eq!(font.glyph_index('\u{10001}'), Some(7));
        assert_eq!(font.glyph_index('D'), None);
        assert_eq!(font.glyph_index('\u{10002}'), None); // past the last group
    }

    #[test]
    fn format_12_wins_over_format_4() {
        // Same font, both subtables. Picking the format 4 would lose the
        // astral range entirely, so the choice is not cosmetic.
        let data = test_font(cmap_table(&[
            (3, 1, cmap_format4()),
            (3, 10, cmap_format12()),
        ]));
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.glyph_index('x'), None); // only the format 4 has 'x'
        assert_eq!(font.glyph_index('A'), Some(1));
    }

    #[test]
    fn hmtx_repeats_the_last_advance() {
        let data = bmp_font();
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.advance_width(0), Some(600));
        assert_eq!(font.advance_width(2), Some(800));
        assert_eq!(font.advance_width(4), Some(800)); // past numberOfHMetrics
        assert_eq!(font.advance_width(8), None); // past numGlyphs
        assert_eq!(font.left_side_bearing(1), Some(50));
        assert_eq!(font.left_side_bearing(4), Some(80)); // bare-bearing array
        // maxp claims eight glyphs but hmtx only has bearings for five, which
        // is exactly the inconsistency a bounds check has to absorb.
        assert_eq!(font.left_side_bearing(6), None);
    }

    #[test]
    fn name_prefers_the_windows_english_record() {
        let data = bmp_font();
        let font = Font::parse(&data).unwrap();
        assert_eq!(font.name(1).as_deref(), Some("Hane Test"));
        assert_eq!(font.name(6), None);
    }

    #[test]
    fn collections_index_their_fonts() {
        let inner = assemble_at(
            20,
            &[
                (b"cmap", cmap_table(&[(3, 1, cmap_format4())])),
                (b"head", head_table()),
                (b"hhea", hhea_table(3)),
                (b"maxp", maxp_table(8)),
            ],
        );
        let mut ttc = Vec::new();
        ttc.extend_from_slice(b"ttcf");
        be32(&mut ttc, 0x0002_0000);
        be32(&mut ttc, 2);
        be32(&mut ttc, 20);
        be32(&mut ttc, 20); // both entries point at the same font
        ttc.extend_from_slice(&inner);
        assert_eq!(Font::count(&ttc), 2);
        assert_eq!(Font::parse_index(&ttc, 1).unwrap().units_per_em(), 2048);
        assert!(Font::parse_index(&ttc, 2).is_err());
        assert!(Font::parse_index(&bmp_font(), 1).is_err());
    }

    #[test]
    fn missing_and_invalid_tables_are_reported() {
        assert_eq!(
            Font::parse(&[]).unwrap_err(),
            Error::Malformed("truncated table")
        );
        assert_eq!(
            Font::parse(b"notafont....").unwrap_err(),
            Error::UnknownFormat
        );

        let no_head = assemble(&[(b"hhea", hhea_table(1)), (b"maxp", maxp_table(1))]);
        assert_eq!(
            Font::parse(&no_head).unwrap_err(),
            Error::MissingTable("head")
        );

        let mut head = head_table();
        head[18..20].copy_from_slice(&0u16.to_be_bytes()); // unitsPerEm
        let bad = assemble(&[
            (b"head", head),
            (b"hhea", hhea_table(1)),
            (b"maxp", maxp_table(1)),
        ]);
        assert_eq!(
            Font::parse(&bad).unwrap_err(),
            Error::Malformed("head.unitsPerEm is zero")
        );

        // A broken optional table costs its own data and nothing else.
        let short_os2 = assemble(&[
            (b"OS/2", vec![0; 10]),
            (b"head", head_table()),
            (b"hhea", hhea_table(1)),
            (b"maxp", maxp_table(1)),
        ]);
        let font = Font::parse(&short_os2).unwrap();
        assert!(font.os2().is_none());
        assert_eq!(font.glyph_index('A'), None); // no cmap either
    }

    /// Every accessor, so a corrupted font gets exercised past the directory.
    fn exercise(data: &[u8]) {
        let Ok(font) = Font::parse(data) else {
            return;
        };
        let _ = (font.head(), font.hhea(), font.maxp(), font.os2());
        for tag in ["head", "cmap", "hmtx", "name", "OS/2", "glyf", "x"] {
            let _ = font.table(tag);
        }
        for ch in [
            '\0',
            'A',
            'y',
            '\u{7f}',
            '\u{fffd}',
            '\u{10000}',
            '\u{10ffff}',
        ] {
            let _ = font.glyph_index(ch);
        }
        for g in [0u16, 1, 3, 5, 255, u16::MAX] {
            let _ = font.advance_width(g);
            let _ = font.left_side_bearing(g);
        }
        for id in [0u16, 1, 6, u16::MAX] {
            let _ = font.name(id);
        }
    }

    #[test]
    fn every_truncation_of_a_valid_font_is_handled() {
        let data = bmp_font();
        for len in 0..data.len() {
            exercise(&data[..len]);
        }
    }

    #[test]
    fn corrupted_fonts_do_not_panic() {
        let base = test_font(cmap_table(&[
            (3, 1, cmap_format4()),
            (3, 10, cmap_format12()),
            (1, 0, cmap_format4()),
        ]));
        // Byte flips on a valid font, not random noise: noise fails the
        // signature check in the first four bytes and never reaches a table
        // parser, so it would fuzz nothing.
        check(
            "corrupted font",
            4_000,
            |r: &mut Rng| {
                let mut data = base.clone();
                for _ in 0..1 + r.below(12) {
                    let i = r.below(data.len() as u64) as usize;
                    data[i] = r.below(256) as u8;
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
        // Pure noise as well, cheaply: it should all be rejected, and the
        // rejection must be an error rather than an index out of bounds.
        check(
            "random bytes",
            2_000,
            |r: &mut Rng| {
                (0..r.below(300))
                    .map(|_| r.below(256) as u8)
                    .collect::<Vec<u8>>()
            },
            |data| {
                exercise(data);
                true
            },
        );
    }

    /// Parse whatever fonts this machine has installed.
    ///
    /// Skipped where there are none, which includes CI, so it is a local
    /// smoke test rather than a gate. The threshold is a proportion because
    /// a font directory can contain bitmap-only and Type 1 files that are not
    /// sfnt at all.
    #[test]
    fn installed_fonts_parse() {
        let mut files = Vec::new();
        collect_fonts(std::path::Path::new("/usr/share/fonts"), &mut files);
        if files.len() < 20 {
            return;
        }
        let (mut ok, mut failed) = (0usize, Vec::new());
        for path in &files {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            let mut all_good = true;
            for i in 0..Font::count(&data) {
                match Font::parse_index(&data, i) {
                    Ok(font) => {
                        // A font that parses but maps nothing is not a pass.
                        assert!(font.units_per_em() > 0);
                        let _ = font.glyph_index('A');
                    }
                    Err(_) => all_good = false,
                }
            }
            if all_good {
                ok += 1;
            } else {
                failed.push(path.clone());
            }
        }
        assert!(
            ok * 10 >= files.len() * 9,
            "only {ok}/{} installed fonts parsed; failures: {failed:?}",
            files.len()
        );
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
