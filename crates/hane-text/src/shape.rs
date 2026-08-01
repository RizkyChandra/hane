//! Shaping: GSUB ligature substitution, GPOS pair positioning and the legacy
//! `kern` table.
//!
//! [`Shaper::new`] resolves the feature-to-lookup chain once per font;
//! [`Shaper::shape`] then runs a string through it. Everything is in font
//! design units, so a caller scales by `size / units_per_em` exactly once.
//!
//! # What is covered
//!
//! Latin, per D-009. Concretely:
//!
//! | Table | Lookup | Formats |
//! |---|---|---|
//! | GSUB | 4, ligature substitution | 1 (the only one) |
//! | GSUB | 7, extension | 1, unwrapped into the type above |
//! | GPOS | 2, pair adjustment | 1 (explicit pairs) and 2 (class pairs) |
//! | GPOS | 9, extension | 1, unwrapped into the type above |
//! | `kern` | Microsoft version 0 | 0, horizontal, non-minimum, non-cross-stream |
//!
//! Coverage formats 1 and 2 and class-definition formats 1 and 2 are both
//! handled, because fonts mix them freely between subtables of one lookup.
//!
//! Not covered, and deliberately: single (1), cursive (3), mark (4-6) and
//! contextual (5, 6, 7, 8) positioning; single, multiple, alternate and
//! contextual substitution; Apple's version 1.0 `kern`; `Device` tables, whose
//! adjustments are ppem-quantised hinting corrections that do not exist at the
//! resolution-independent stage. A font that kerns only through a chained
//! context lookup gets no kerning here rather than wrong kerning.
//!
//! # Untrusted input
//!
//! GPOS and GSUB are nested offset tables reached from a file that arrives over
//! the network, so this module keeps the rules [`crate::opentype`] set: every
//! read goes through the checked `*_at` helpers, no count from the file bounds a
//! loop before it is clamped to the bytes that actually exist, and a subtable
//! that does not parse is skipped rather than fatal. Nothing recurses -- the one
//! place the format allows it, an extension lookup, is unwrapped exactly one
//! level, which is also all the specification permits.

use crate::opentype::{Font, i16_at, u16_at, u32_at};

/// Which OpenType features a shaping run turns on.
///
/// Selection is per run, not per font: a code listing wants ligatures off and a
/// heading wants them on, from the same [`Shaper`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    /// GSUB `liga`, `clig` and `rlig` -- fi, fl, ffi and friends.
    pub ligatures: bool,
    /// The GPOS `kern` feature, falling back to the legacy `kern` table when
    /// the font has no GPOS kerning.
    pub kerning: bool,
}

impl Features {
    /// Neither feature: character-to-glyph mapping and advances only.
    pub const NONE: Self = Self {
        ligatures: false,
        kerning: false,
    };
    /// Both features, which is what body text should use.
    pub const ALL: Self = Self {
        ligatures: true,
        kerning: true,
    };
}

impl Default for Features {
    fn default() -> Self {
        Self::ALL
    }
}

/// One shaped glyph, positioned in font design units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Glyph {
    /// Glyph index in the font. `0` is `.notdef`, for a character the font
    /// does not cover.
    pub id: u16,
    /// Byte offset into the shaped string of the first character this glyph
    /// came from. A ligature keeps the offset of its first component, so the
    /// mapping back to text stays monotonic.
    pub cluster: usize,
    /// How far the pen moves horizontally after drawing this glyph.
    pub x_advance: i32,
    /// Vertical pen movement, zero for every horizontal Latin run.
    pub y_advance: i32,
    /// Horizontal displacement of the outline from the pen position.
    pub x_offset: i32,
    /// Vertical displacement of the outline from the baseline.
    pub y_offset: i32,
}

/// A font with its shaping tables resolved, ready to shape strings.
///
/// Resolving the script, language and feature chain is a per-font cost, so it
/// happens once here rather than on every run.
#[derive(Debug)]
pub struct Shaper<'a> {
    font: &'a Font<'a>,
    /// GSUB lookups of the ligature features, in lookup-list order, which is
    /// the order the specification says to apply them in.
    ligature_lookups: Vec<Lookup<'a>>,
    /// GPOS lookups of the `kern` feature, likewise ordered.
    kern_lookups: Vec<Lookup<'a>>,
    /// Format 0 `kern` subtables, used only when GPOS has no `kern` feature.
    legacy_kern: Vec<&'a [u8]>,
    /// `GDEF` glyph class definitions, for the lookup flags that skip marks.
    gdef_class: Option<&'a [u8]>,
}

/// One lookup, with its subtables already unwrapped from any extension.
#[derive(Clone, Debug)]
struct Lookup<'a> {
    /// The effective lookup type, after unwrapping extensions.
    kind: u16,
    flags: u16,
    subtables: Vec<&'a [u8]>,
}

impl<'a> Shaper<'a> {
    /// Resolve `font`'s shaping tables.
    ///
    /// A font with no GSUB, no GPOS and no `kern` yields a shaper that still
    /// maps characters to glyphs and reads advances, which is the correct
    /// result for a font that carries no shaping data.
    pub fn new(font: &'a Font<'a>) -> Self {
        let ligature_lookups = font
            .table("GSUB")
            .map(|t| lookups_for(t, &[b"liga", b"clig", b"rlig"], 7, 4))
            .unwrap_or_default();
        let kern_lookups = font
            .table("GPOS")
            .map(|t| lookups_for(t, &[b"kern"], 9, 2))
            .unwrap_or_default();
        // The legacy table is a fallback, not an addition: a font shipping both
        // duplicates its kerning, and applying both double-kerns every pair.
        let legacy_kern = if kern_lookups.is_empty() {
            font.table("kern")
                .map(legacy_kern_subtables)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Self {
            font,
            ligature_lookups,
            kern_lookups,
            legacy_kern,
            gdef_class: font.table("GDEF").and_then(|t| {
                let off = u16_at(t, 4).ok()? as usize;
                // Offset 0 means the optional class definition is absent, and
                // `t.get(0..)` would hand back the header as if it were one.
                (off != 0).then(|| t.get(off..))?
            }),
        }
    }

    /// The font this shaper was built from.
    pub fn font(&self) -> &'a Font<'a> {
        self.font
    }

    /// Whether the font supplies any of the ligature features.
    pub fn has_ligatures(&self) -> bool {
        !self.ligature_lookups.is_empty()
    }

    /// Whether the font supplies kerning, from either GPOS or `kern`.
    pub fn has_kerning(&self) -> bool {
        !self.kern_lookups.is_empty() || !self.legacy_kern.is_empty()
    }

    /// Shape `text` into positioned glyphs, in font design units.
    ///
    /// The run is left-to-right and unsegmented: one string in, one glyph
    /// sequence out. Characters the font does not cover become `.notdef`
    /// rather than disappearing, so a missing glyph shows up in the output
    /// instead of silently shortening the line.
    pub fn shape(&self, text: &str, features: Features) -> Vec<Glyph> {
        let mut glyphs: Vec<Glyph> = text
            .char_indices()
            .map(|(cluster, ch)| Glyph {
                id: self.font.glyph_index(ch).unwrap_or(0),
                cluster,
                x_advance: 0,
                y_advance: 0,
                x_offset: 0,
                y_offset: 0,
            })
            .collect();

        if features.ligatures {
            for lookup in &self.ligature_lookups {
                self.apply_ligature_lookup(lookup, &mut glyphs);
            }
        }

        // Advances come after substitution and before positioning: a ligature
        // has its own advance, and GPOS adjusts the advance that results.
        for glyph in &mut glyphs {
            glyph.x_advance = i32::from(self.font.advance_width(glyph.id).unwrap_or(0));
        }

        if features.kerning {
            for lookup in &self.kern_lookups {
                self.apply_pair_lookup(lookup, &mut glyphs);
            }
            self.apply_legacy_kern(&mut glyphs);
        }
        glyphs
    }

    /// Total advance of a shaped run, in font design units.
    pub fn measure(&self, text: &str, features: Features) -> i32 {
        self.shape(text, features).iter().map(|g| g.x_advance).sum()
    }

    /// Whether a lookup's flags say to skip `glyph`.
    ///
    /// Without `GDEF` there is no way to know a glyph's class, so nothing is
    /// skipped -- which is the right failure for Latin, where a run has no
    /// marks to skip in the first place.
    fn skipped(&self, flags: u16, glyph: u16) -> bool {
        // ponytail: the mark attachment type in the high byte and the mark
        // filtering set are not honoured. Both only matter once mark
        // positioning exists, which is the same issue that would add GPOS 4-6.
        if flags & 0x000E == 0 {
            return false;
        }
        let Some(class) = self.gdef_class else {
            return false;
        };
        match class_of(class, glyph) {
            1 => flags & 0x0002 != 0, // base glyph
            2 => flags & 0x0004 != 0, // ligature
            3 => flags & 0x0008 != 0, // mark
            _ => false,
        }
    }

    /// The next position at or after `from` that this lookup does not skip.
    fn next_used(&self, flags: u16, glyphs: &[Glyph], from: usize) -> Option<usize> {
        (from..glyphs.len()).find(|&j| !self.skipped(flags, glyphs[j].id))
    }

    fn apply_ligature_lookup(&self, lookup: &Lookup<'_>, glyphs: &mut Vec<Glyph>) {
        if lookup.kind != 4 {
            return;
        }
        let mut i = 0;
        while i < glyphs.len() {
            if !self.skipped(lookup.flags, glyphs[i].id) {
                for sub in &lookup.subtables {
                    if let Some((lig, parts)) = self.ligature_at(sub, lookup.flags, glyphs, i) {
                        glyphs[i].id = lig;
                        // Remove back to front so the earlier indices stay valid.
                        for &p in parts.iter().rev() {
                            glyphs.remove(p);
                        }
                        break; // Only the first matching subtable of a lookup applies.
                    }
                }
            }
            // The ligature is not re-examined by the same lookup. A three-part
            // form spelled as two two-part ligatures still forms, because the
            // second one lives in a later lookup that sees the whole run again.
            i += 1;
        }
    }

    /// Match one `LigatureSubst` subtable at `i`, returning the ligature glyph
    /// and the positions of the components after the first.
    fn ligature_at(
        &self,
        sub: &[u8],
        flags: u16,
        glyphs: &[Glyph],
        i: usize,
    ) -> Option<(u16, Vec<usize>)> {
        if u16_at(sub, 0).ok()? != 1 {
            return None;
        }
        let coverage = sub.get(u16_at(sub, 2).ok()? as usize..)?;
        let index = coverage_index(coverage, glyphs[i].id)? as usize;
        let sets = (u16_at(sub, 4).ok()? as usize).min(sub.len().saturating_sub(6) / 2);
        if index >= sets {
            return None; // Coverage and the set array disagree; trust the array.
        }
        let set = sub.get(u16_at(sub, 6 + index * 2).ok()? as usize..)?;
        let count = (u16_at(set, 0).ok()? as usize).min(set.len().saturating_sub(2) / 2);

        // File order is preference order, and it puts longer ligatures first,
        // so `ffi` wins over `ff` without any length comparison here.
        'ligature: for l in 0..count {
            let Ok(off) = u16_at(set, 2 + l * 2) else {
                continue;
            };
            let Some(lig) = set.get(off as usize..) else {
                continue;
            };
            let (Ok(glyph), Ok(components)) = (u16_at(lig, 0), u16_at(lig, 2)) else {
                continue;
            };
            if components == 0 {
                continue; // A zero-component ligature would match everywhere.
            }
            let mut parts = Vec::with_capacity(components as usize - 1);
            let mut j = i + 1;
            for c in 0..components as usize - 1 {
                let Some(next) = self.next_used(flags, glyphs, j) else {
                    continue 'ligature;
                };
                let Ok(want) = u16_at(lig, 4 + c * 2) else {
                    continue 'ligature;
                };
                if glyphs[next].id != want {
                    continue 'ligature;
                }
                parts.push(next);
                j = next + 1;
            }
            return Some((glyph, parts));
        }
        None
    }

    fn apply_pair_lookup(&self, lookup: &Lookup<'_>, glyphs: &mut [Glyph]) {
        if lookup.kind != 2 {
            return;
        }
        let mut i = 0;
        while i < glyphs.len() {
            let Some(first) = self.next_used(lookup.flags, glyphs, i) else {
                return;
            };
            let Some(second) = self.next_used(lookup.flags, glyphs, first + 1) else {
                return;
            };
            let mut consumed_second = false;
            for sub in &lookup.subtables {
                if let Some((v1, v2, has_second)) =
                    pair_values(sub, glyphs[first].id, glyphs[second].id)
                {
                    apply_value(&mut glyphs[first], v1);
                    apply_value(&mut glyphs[second], v2);
                    consumed_second = has_second;
                    break;
                }
            }
            // When the subtable adjusts the second glyph too, that glyph is
            // spent and the next pair starts after it. Otherwise it becomes the
            // first glyph of the next pair, which is what makes a run of three
            // kerned letters kern twice.
            i = if consumed_second { second + 1 } else { second };
        }
    }

    fn apply_legacy_kern(&self, glyphs: &mut [Glyph]) {
        if self.legacy_kern.is_empty() {
            return;
        }
        for i in 0..glyphs.len().saturating_sub(1) {
            let (left, right) = (glyphs[i].id, glyphs[i + 1].id);
            // Subtables accumulate: the format defines the total as the sum
            // over every subtable that covers the pair.
            let delta: i32 = self
                .legacy_kern
                .iter()
                .filter_map(|t| kern_pair(t, left, right))
                .sum();
            // Split between the two glyphs rather than loading it all onto the
            // first. The `kern` table adjusts the space *between* a pair, and
            // moving both halves keeps the pair centred on the space it would
            // have occupied; the total advance is the same either way. This is
            // also what HarfBuzz does, so a rendering can be diffed against it.
            let half = delta >> 1;
            glyphs[i].x_advance += half;
            glyphs[i + 1].x_advance += delta - half;
            glyphs[i + 1].x_offset += delta - half;
        }
    }
}

/// Add a value record's four adjustments to a glyph.
fn apply_value(glyph: &mut Glyph, value: [i32; 4]) {
    glyph.x_offset += value[0];
    glyph.y_offset += value[1];
    glyph.x_advance += value[2];
    glyph.y_advance += value[3];
}

/// Resolve the lookups of `tags` in a GSUB or GPOS table.
///
/// `extension` is the lookup type that wraps another one -- 7 in GSUB, 9 in
/// GPOS -- and `kind` the type this caller can actually run; lookups of any
/// other type are dropped here so the apply loops never see them.
fn lookups_for<'a>(
    table: &'a [u8],
    tags: &[&[u8; 4]],
    extension: u16,
    kind: u16,
) -> Vec<Lookup<'a>> {
    let mut out = Vec::new();
    let Ok(list_offset) = u16_at(table, 8) else {
        return out;
    };
    let Some(list) = table.get(list_offset as usize..) else {
        return out;
    };
    let count = u16_at(list, 0)
        .map(|c| (c as usize).min(list.len().saturating_sub(2) / 2))
        .unwrap_or(0);

    for index in feature_lookup_indices(table, tags) {
        if index as usize >= count {
            continue;
        }
        let Ok(offset) = u16_at(list, 2 + index as usize * 2) else {
            continue;
        };
        let Some(lookup) = list.get(offset as usize..) else {
            continue;
        };
        let (Ok(declared), Ok(flags)) = (u16_at(lookup, 0), u16_at(lookup, 2)) else {
            continue;
        };
        let mut lookup_kind = declared;
        let subs = u16_at(lookup, 4)
            .map(|c| (c as usize).min(lookup.len().saturating_sub(6) / 2))
            .unwrap_or(0);

        let mut subtables = Vec::with_capacity(subs);
        for s in 0..subs {
            let Ok(offset) = u16_at(lookup, 6 + s * 2) else {
                continue;
            };
            let Some(sub) = lookup.get(offset as usize..) else {
                continue;
            };
            // `declared`, not `lookup_kind`: the latter is rewritten below once
            // the first extension is resolved, and testing it would leave every
            // subtable after the first wrapped -- which is where the kerning of
            // a large font actually lives.
            if declared != extension {
                subtables.push(sub);
                continue;
            }
            // An extension subtable is a header pointing at the real one, with
            // a 32-bit offset so a large font's lookups can live past 64 KiB.
            // Every subtable of a lookup has the same real type, so rewriting
            // the lookup's own type here is not a per-subtable decision.
            let (Ok(1), Ok(real), Ok(delta)) = (u16_at(sub, 0), u16_at(sub, 2), u32_at(sub, 4))
            else {
                continue;
            };
            let Some(inner) = sub.get(delta as usize..) else {
                continue;
            };
            lookup_kind = real;
            subtables.push(inner);
        }
        if lookup_kind == kind && !subtables.is_empty() {
            out.push(Lookup {
                kind: lookup_kind,
                flags,
                subtables,
            });
        }
    }
    out
}

/// Lookup indices of every feature whose tag is in `tags`, sorted and unique.
///
/// Sorted because the specification applies lookups in lookup-list order, not
/// in the order the features happen to name them.
fn feature_lookup_indices(table: &[u8], tags: &[&[u8; 4]]) -> Vec<u16> {
    let mut out = Vec::new();
    let Ok(list_offset) = u16_at(table, 6) else {
        return out;
    };
    let Some(list) = table.get(list_offset as usize..) else {
        return out;
    };
    let count = u16_at(list, 0)
        .map(|c| (c as usize).min(list.len().saturating_sub(2) / 6))
        .unwrap_or(0);
    // A font whose script list is unreadable still has features, and running
    // all of them beats running none: the tag filter below is the real one.
    let wanted = script_features(table);

    for i in 0..count {
        if wanted.as_ref().is_some_and(|w| !w.contains(&(i as u16))) {
            continue;
        }
        let record = 2 + i * 6;
        let Some(tag) = list.get(record..record + 4) else {
            continue;
        };
        if !tags.iter().any(|t| tag == t.as_slice()) {
            continue;
        }
        let Ok(offset) = u16_at(list, record + 4) else {
            continue;
        };
        let Some(feature) = list.get(offset as usize..) else {
            continue;
        };
        let lookups = u16_at(feature, 2)
            .map(|c| (c as usize).min(feature.len().saturating_sub(4) / 2))
            .unwrap_or(0);
        for l in 0..lookups {
            if let Ok(index) = u16_at(feature, 4 + l * 2) {
                out.push(index);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Feature indices the Latin -- or failing that the default -- script selects.
///
/// `None` means "no usable script record", which the caller reads as "do not
/// filter" rather than "no features".
fn script_features(table: &[u8]) -> Option<Vec<u16>> {
    let list = table.get(u16_at(table, 4).ok()? as usize..)?;
    let count = (u16_at(list, 0).ok()? as usize).min(list.len().saturating_sub(2) / 6);

    let mut best: Option<(u8, u16)> = None;
    for i in 0..count {
        let record = 2 + i * 6;
        let Some(tag) = list.get(record..record + 4) else {
            continue;
        };
        let Ok(offset) = u16_at(list, record + 4) else {
            continue;
        };
        // D-009 is Latin first. `DFLT` is the fallback every font has, and any
        // other script beats nothing at all in a font that ships only one.
        let rank = match tag {
            b"latn" => 3,
            b"DFLT" => 2,
            _ => 1,
        };
        if best.is_none_or(|(r, _)| r < rank) {
            best = Some((rank, offset));
        }
    }
    let script = list.get(best?.1 as usize..)?;
    // The default language system, not a specific one: `dflt` is what an
    // unlabelled run gets, and language-specific overrides are a later issue.
    let lang_sys_offset = u16_at(script, 0).ok()?;
    if lang_sys_offset == 0 {
        return None;
    }
    let lang_sys = script.get(lang_sys_offset as usize..)?;
    let count = (u16_at(lang_sys, 4).ok()? as usize).min(lang_sys.len().saturating_sub(6) / 2);
    let mut out = Vec::with_capacity(count + 1);
    // 0xFFFF is "no required feature", and is not an index.
    if let Ok(required) = u16_at(lang_sys, 2)
        && required != 0xFFFF
    {
        out.push(required);
    }
    for i in 0..count {
        if let Ok(index) = u16_at(lang_sys, 6 + i * 2) {
            out.push(index);
        }
    }
    Some(out)
}

/// The coverage index of `glyph`, or `None` when the table does not cover it.
fn coverage_index(coverage: &[u8], glyph: u16) -> Option<u16> {
    match u16_at(coverage, 0).ok()? {
        1 => {
            let count =
                (u16_at(coverage, 2).ok()? as usize).min(coverage.len().saturating_sub(4) / 2);
            // The glyph array is sorted, which is what makes coverage cheap;
            // a font that violates that gets a miss, never a bad read.
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if u16_at(coverage, 4 + mid * 2).ok()? < glyph {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            (lo < count && u16_at(coverage, 4 + lo * 2).ok()? == glyph).then_some(lo as u16)
        }
        2 => {
            let count =
                (u16_at(coverage, 2).ok()? as usize).min(coverage.len().saturating_sub(4) / 6);
            // Last range whose start is <= glyph, as the first strictly
            // greater one minus one.
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if u16_at(coverage, 4 + mid * 6).ok()? <= glyph {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            let record = 4 + lo.checked_sub(1)? * 6;
            let (start, end) = (
                u16_at(coverage, record).ok()?,
                u16_at(coverage, record + 2).ok()?,
            );
            if glyph > end {
                return None;
            }
            // `wrapping_add`: both operands are file data, and a corrupt pair
            // that overflows should miss the pair set, not panic.
            Some(
                u16_at(coverage, record + 4)
                    .ok()?
                    .wrapping_add(glyph - start),
            )
        }
        _ => None,
    }
}

/// The class of `glyph`, which is 0 -- "everything else" -- for anything the
/// table does not list, including a table too corrupt to read.
fn class_of(class_def: &[u8], glyph: u16) -> u16 {
    fn inner(t: &[u8], glyph: u16) -> Option<u16> {
        match u16_at(t, 0).ok()? {
            1 => {
                let start = u16_at(t, 2).ok()?;
                let count = (u16_at(t, 4).ok()? as usize).min(t.len().saturating_sub(6) / 2);
                let index = glyph.checked_sub(start)? as usize;
                (index < count).then(|| u16_at(t, 6 + index * 2).ok())?
            }
            2 => {
                let count = (u16_at(t, 2).ok()? as usize).min(t.len().saturating_sub(4) / 6);
                let (mut lo, mut hi) = (0usize, count);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if u16_at(t, 4 + mid * 6).ok()? <= glyph {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let record = 4 + lo.checked_sub(1)? * 6;
                (u16_at(t, record + 2).ok()? >= glyph).then(|| u16_at(t, record + 4).ok())?
            }
            _ => None,
        }
    }
    inner(class_def, glyph).unwrap_or(0)
}

/// Bytes a value record of `format` occupies.
///
/// The four `Device` bits are counted because the offsets still take up room,
/// even though nothing reads them.
fn value_size(format: u16) -> usize {
    (format & 0x00FF).count_ones() as usize * 2
}

/// Read a value record as `[x_placement, y_placement, x_advance, y_advance]`.
fn value_record(t: &[u8], offset: usize, format: u16) -> Option<[i32; 4]> {
    let mut value = [0i32; 4];
    let mut at = offset;
    for (bit, slot) in value.iter_mut().enumerate() {
        if format & (1 << bit) != 0 {
            *slot = i32::from(i16_at(t, at).ok()?);
            at = at.checked_add(2)?;
        }
    }
    Some(value)
}

/// Adjustments for one pair from a `PairPos` subtable.
///
/// The third element says whether the subtable also adjusts the second glyph,
/// which decides whether that glyph can start the following pair.
fn pair_values(sub: &[u8], first: u16, second: u16) -> Option<([i32; 4], [i32; 4], bool)> {
    let format = u16_at(sub, 0).ok()?;
    let coverage = sub.get(u16_at(sub, 2).ok()? as usize..)?;
    let index = coverage_index(coverage, first)? as usize;
    let (format1, format2) = (u16_at(sub, 4).ok()?, u16_at(sub, 6).ok()?);
    let (size1, size2) = (value_size(format1), value_size(format2));

    match format {
        1 => {
            let sets = (u16_at(sub, 8).ok()? as usize).min(sub.len().saturating_sub(10) / 2);
            if index >= sets {
                return None;
            }
            let set = sub.get(u16_at(sub, 10 + index * 2).ok()? as usize..)?;
            let stride = 2usize.checked_add(size1)?.checked_add(size2)?;
            let count = (u16_at(set, 0).ok()? as usize).min(set.len().saturating_sub(2) / stride);
            // Pair records are sorted by second glyph.
            let (mut lo, mut hi) = (0usize, count);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if u16_at(set, 2 + mid * stride).ok()? < second {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo >= count || u16_at(set, 2 + lo * stride).ok()? != second {
                return None;
            }
            let at = 2 + lo * stride + 2;
            Some((
                value_record(set, at, format1)?,
                value_record(set, at + size1, format2)?,
                format2 != 0,
            ))
        }
        2 => {
            let class1 = sub.get(u16_at(sub, 8).ok()? as usize..)?;
            let class2 = sub.get(u16_at(sub, 10).ok()? as usize..)?;
            let (count1, count2) = (
                u16_at(sub, 12).ok()? as usize,
                u16_at(sub, 14).ok()? as usize,
            );
            let (c1, c2) = (
                class_of(class1, first) as usize,
                class_of(class2, second) as usize,
            );
            if c1 >= count1 || c2 >= count2 {
                return None;
            }
            let stride = size1.checked_add(size2)?;
            // The matrix is class1Count x class2Count records; a font that
            // claims more than it stores must not read past its own subtable.
            let at = c1
                .checked_mul(count2)?
                .checked_add(c2)?
                .checked_mul(stride)?
                .checked_add(16)?;
            if at.checked_add(stride)? > sub.len() {
                return None;
            }
            Some((
                value_record(sub, at, format1)?,
                value_record(sub, at + size1, format2)?,
                format2 != 0,
            ))
        }
        _ => None,
    }
}

/// The horizontal format 0 subtables of a Microsoft version 0 `kern` table.
///
/// Apple's version 1.0 layout differs in every header field, and every font
/// that ships a `kern` for Windows ships version 0; a version 1.0 table is
/// skipped rather than misread.
fn legacy_kern_subtables(table: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    if u16_at(table, 0).unwrap_or(1) != 0 {
        return out;
    }
    let count = u16_at(table, 2).unwrap_or(0) as usize;
    let mut at = 4usize;
    // The subtable count is bounded by the smallest possible subtable, so a
    // huge claimed count cannot spin the loop.
    for _ in 0..count.min(table.len().saturating_sub(4) / 6) {
        let Ok(length) = u16_at(table, at + 2) else {
            break;
        };
        let Ok(coverage) = u16_at(table, at + 4) else {
            break;
        };
        let length = (length as usize).max(6).min(table.len().saturating_sub(at));
        // Horizontal, and neither a minimum-value table nor a cross-stream one:
        // those two adjust something other than the horizontal advance, and
        // adding them to it is worse than ignoring them.
        if coverage >> 8 == 0 && coverage & 0x07 == 0x01 {
            // ponytail: the override bit (0x08) is treated as accumulate. No
            // font on the development machine sets it; handle it when one does.
            if let Some(body) = table.get(at + 6..at + length) {
                out.push(body);
            }
        }
        at += length;
        if at >= table.len() {
            break;
        }
    }
    out
}

/// The adjustment a format 0 `kern` subtable body gives one pair.
fn kern_pair(body: &[u8], left: u16, right: u16) -> Option<i32> {
    let count = (u16_at(body, 0).ok()? as usize).min(body.len().saturating_sub(8) / 6);
    let key = (u32::from(left) << 16) | u32::from(right);
    let (mut lo, mut hi) = (0usize, count);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let at = 8 + mid * 6;
        let probe =
            (u32::from(u16_at(body, at).ok()?) << 16) | u32::from(u16_at(body, at + 2).ok()?);
        if probe < key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo >= count {
        return None;
    }
    let at = 8 + lo * 6;
    let probe = (u32::from(u16_at(body, at).ok()?) << 16) | u32::from(u16_at(body, at + 2).ok()?);
    if probe != key {
        return None;
    }
    i16_at(body, at + 4).ok().map(i32::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfont::{self, be16, font_with, gsub_liga, layout_table};
    use hane_geom::fuzz::{Rng, check};

    /// GPOS with one PairPos format 1 lookup: 1,2 kerns by -40.
    fn gpos_pair_format1() -> Vec<u8> {
        let mut set = Vec::new();
        be16(&mut set, 2); // pairValueCount, sorted by second glyph
        be16(&mut set, 2);
        be16(&mut set, (-40i16) as u16);
        be16(&mut set, 3);
        be16(&mut set, 25);

        let mut coverage = Vec::new();
        be16(&mut coverage, 1);
        be16(&mut coverage, 1);
        be16(&mut coverage, 1);

        let mut sub = Vec::new();
        be16(&mut sub, 1); // posFormat
        be16(&mut sub, 12); // coverageOffset
        be16(&mut sub, 0x0004); // valueFormat1: XAdvance
        be16(&mut sub, 0); // valueFormat2: nothing
        be16(&mut sub, 1); // pairSetCount
        be16(&mut sub, 12 + coverage.len() as u16);
        sub.extend_from_slice(&coverage);
        sub.extend_from_slice(&set);
        layout_table(2, &[sub], b"kern")
    }

    /// GPOS with a PairPos format 2 lookup, coverage format 2 and class
    /// definitions of both formats -- the combination real fonts ship.
    fn gpos_pair_format2() -> Vec<u8> {
        // Coverage format 2: glyphs 1..=3 covered.
        let mut coverage = Vec::new();
        be16(&mut coverage, 2);
        be16(&mut coverage, 1);
        be16(&mut coverage, 1);
        be16(&mut coverage, 3);
        be16(&mut coverage, 0);

        // ClassDef format 1: glyph 1 -> class 1, glyphs 2,3 -> class 0.
        let mut class1 = Vec::new();
        be16(&mut class1, 1);
        be16(&mut class1, 1); // startGlyph
        be16(&mut class1, 3);
        be16(&mut class1, 1);
        be16(&mut class1, 0);
        be16(&mut class1, 0);

        // ClassDef format 2: glyphs 2..=3 -> class 1.
        let mut class2 = Vec::new();
        be16(&mut class2, 2);
        be16(&mut class2, 1);
        be16(&mut class2, 2);
        be16(&mut class2, 3);
        be16(&mut class2, 1);

        // Header, then the 2x2 matrix of 4-byte records.
        let head = 16u16 + 16;
        let mut sub = Vec::new();
        be16(&mut sub, 2); // posFormat
        be16(&mut sub, head); // coverageOffset, past the 2x2 matrix
        be16(&mut sub, 0x0004); // valueFormat1: XAdvance
        be16(&mut sub, 0x0004); // valueFormat2: XAdvance, so the pair consumes both
        be16(&mut sub, head + coverage.len() as u16);
        be16(&mut sub, head + (coverage.len() + class1.len()) as u16);
        be16(&mut sub, 2); // class1Count
        be16(&mut sub, 2); // class2Count
        for value in [0i16, 0, 0, 0, 0, 0, -55, -7] {
            be16(&mut sub, value as u16);
        }
        sub.extend_from_slice(&coverage);
        sub.extend_from_slice(&class1);
        sub.extend_from_slice(&class2);
        layout_table(2, &[sub], b"kern")
    }

    /// A `kern` table pairing glyphs 1,2 at -30 and 2,3 at -10.
    fn kern_table() -> Vec<u8> {
        let mut body = Vec::new();
        be16(&mut body, 2); // nPairs
        be16(&mut body, 12);
        be16(&mut body, 1);
        be16(&mut body, 0);
        for (l, r, v) in [(1u16, 2u16, -30i16), (2, 3, -10)] {
            be16(&mut body, l);
            be16(&mut body, r);
            be16(&mut body, v as u16);
        }
        let mut t = Vec::new();
        be16(&mut t, 0); // version
        be16(&mut t, 1); // nTables
        be16(&mut t, 0); // subtable version
        be16(&mut t, 6 + body.len() as u16);
        be16(&mut t, 0x0001); // coverage: format 0, horizontal
        t.extend_from_slice(&body);
        t
    }

    fn ids(glyphs: &[Glyph]) -> Vec<u16> {
        glyphs.iter().map(|g| g.id).collect()
    }

    #[test]
    fn ligatures_prefer_the_longest_form() {
        let data = font_with(&[(b"GSUB", gsub_liga(false))]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        assert!(shaper.has_ligatures());

        // 'A' is glyph 1 and 'B' is glyph 2 in the synthetic cmap, so "AAB"
        // is the three-component ligature and "AA" the two-component one.
        assert_eq!(ids(&shaper.shape("AAB", Features::ALL)), [21]);
        assert_eq!(ids(&shaper.shape("AA", Features::ALL)), [20]);
        assert_eq!(ids(&shaper.shape("AAC", Features::ALL)), [20, 3]);
        // Feature selection is per run.
        assert_eq!(ids(&shaper.shape("AAB", Features::NONE)), [1, 1, 2]);
        assert_eq!(
            ids(&shaper.shape(
                "AAB",
                Features {
                    ligatures: false,
                    kerning: true
                }
            )),
            [1, 1, 2]
        );
    }

    #[test]
    fn ligature_clusters_stay_monotonic() {
        let data = font_with(&[(b"GSUB", gsub_liga(false))]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        let glyphs = shaper.shape("CAAB", Features::ALL);
        // The ligature keeps its first component's byte offset, so a hit test
        // that walks clusters never sees them go backwards.
        assert_eq!(glyphs.iter().map(|g| g.cluster).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn extension_lookups_are_unwrapped() {
        let plain = font_with(&[(b"GSUB", gsub_liga(false))]);
        let wrapped = font_with(&[(b"GSUB", gsub_liga(true))]);
        let (a, b) = (Font::parse(&plain).unwrap(), Font::parse(&wrapped).unwrap());
        assert_eq!(
            ids(&Shaper::new(&a).shape("AAB", Features::ALL)),
            ids(&Shaper::new(&b).shape("AAB", Features::ALL))
        );
        assert_eq!(ids(&Shaper::new(&b).shape("AAB", Features::ALL)), [21]);
    }

    #[test]
    fn pair_positioning_format_1() {
        let data = font_with(&[(b"GPOS", gpos_pair_format1())]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        assert!(shaper.has_kerning());
        // Advances are 600, 700, 800 for glyphs 1, 2, 3.
        let glyphs = shaper.shape("ABC", Features::ALL);
        assert_eq!(glyphs[0].x_advance, 700 - 40);
        assert_eq!(glyphs[1].x_advance, 800);
        // valueFormat2 is empty, so the second glyph of a pair can start the
        // next one: "AAC" kerns nothing but "ABC" kerns once.
        assert_eq!(shaper.measure("ABC", Features::ALL), 700 + 800 + 800 - 40);
        assert_eq!(shaper.measure("ABC", Features::NONE), 700 + 800 + 800);
    }

    #[test]
    fn pair_positioning_format_2_uses_classes() {
        let data = font_with(&[(b"GPOS", gpos_pair_format2())]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        // Class 1 x class 1 is (-55, -7); every other cell is zero.
        let ab = shaper.shape("AB", Features::ALL);
        assert_eq!(ab[0].x_advance, 700 - 55);
        assert_eq!(ab[1].x_advance, 800 - 7);
        // 'A' is class 1 on the left but class 0 on the right, so "AA" is the
        // zero cell -- which is the whole point of a class table.
        let aa = shaper.shape("AA", Features::ALL);
        assert_eq!(aa[0].x_advance, 700);
        // Coverage format 2 covers glyph 3 as well, and glyph 3 is class 0.
        assert_eq!(shaper.shape("CB", Features::ALL)[0].x_advance, 800);
    }

    #[test]
    fn value_format_2_consumes_the_second_glyph() {
        let data = font_with(&[(b"GPOS", gpos_pair_format2())]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        // "ABB": the first pair adjusts both glyphs, so the second B pairs
        // with nothing. Only one adjustment lands on glyph 2.
        let glyphs = shaper.shape("ABB", Features::ALL);
        assert_eq!(glyphs[1].x_advance, 800 - 7);
        assert_eq!(glyphs[2].x_advance, 800);
    }

    #[test]
    fn legacy_kern_applies_and_yields_to_gpos() {
        let only_kern = font_with(&[(b"kern", kern_table())]);
        let font = Font::parse(&only_kern).unwrap();
        let shaper = Shaper::new(&font);
        assert!(shaper.has_kerning());
        // -30 on the A/B pair and -10 on the B/C pair, each split between its
        // two glyphs, so the total is what matters and it is 700+800+800-40.
        let glyphs = shaper.shape("ABC", Features::ALL);
        assert_eq!(glyphs[0].x_advance, 700 - 15);
        assert_eq!(glyphs[1].x_advance, 800 - 15 - 5);
        assert_eq!(glyphs[2].x_advance, 800 - 5);
        assert_eq!(shaper.measure("ABC", Features::ALL), 700 + 800 + 800 - 40);
        assert_eq!(shaper.measure("ABC", Features::NONE), 700 + 800 + 800);

        // With GPOS kerning present the legacy table must not also apply, or
        // every pair is kerned twice.
        let both = font_with(&[(b"kern", kern_table()), (b"GPOS", gpos_pair_format1())]);
        let font = Font::parse(&both).unwrap();
        let glyphs = Shaper::new(&font).shape("ABC", Features::ALL);
        assert_eq!(glyphs[0].x_advance, 700 - 40);
        assert_eq!(glyphs[1].x_advance, 800);
    }

    #[test]
    fn a_font_without_shaping_tables_still_shapes() {
        let data = font_with(&[]);
        let font = Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        assert!(!shaper.has_ligatures() && !shaper.has_kerning());
        let glyphs = shaper.shape("A?C", Features::ALL);
        assert_eq!(ids(&glyphs), [1, 0, 3]); // '?' is unmapped: .notdef, not dropped
        assert_eq!(glyphs[2].cluster, 2);
    }

    /// Every entry point, so a corrupted table gets exercised past the header.
    fn exercise(data: &[u8]) {
        let Ok(font) = Font::parse(data) else {
            return;
        };
        let shaper = Shaper::new(&font);
        for text in ["", "A", "AAB", "ABC", "AABAAB", "CAAAB\u{10000}"] {
            for features in [Features::ALL, Features::NONE] {
                let glyphs = shaper.shape(text, features);
                // Clusters never go backwards, whatever the file said.
                assert!(glyphs.windows(2).all(|w| w[0].cluster <= w[1].cluster));
                assert!(glyphs.len() <= text.chars().count());
            }
        }
        let _ = (shaper.has_ligatures(), shaper.has_kerning(), shaper.font());
    }

    #[test]
    fn corrupted_shaping_tables_do_not_panic() {
        let base = font_with(&[
            (b"GSUB", gsub_liga(true)),
            (b"GPOS", gpos_pair_format2()),
            (b"kern", kern_table()),
        ]);
        for len in 0..base.len() {
            exercise(&base[..len]);
        }
        // Byte flips rather than noise: noise never gets past the signature,
        // so it would fuzz the directory and nothing else.
        check(
            "corrupted shaping tables",
            6_000,
            |r: &mut Rng| {
                let mut data = base.clone();
                for _ in 0..1 + r.below(10) {
                    let i = r.below(data.len() as u64) as usize;
                    data[i] = r.below(256) as u8;
                }
                data
            },
            |data| {
                exercise(data);
                true
            },
        );
    }

    /// Shape with every installed font, checking only that nothing panics and
    /// that the invariants hold. Skipped where no fonts are installed, which
    /// includes CI, so it is a local smoke test rather than a gate.
    #[test]
    fn installed_fonts_shape() {
        let files = testfont::installed_fonts();
        if files.len() < 20 {
            return;
        }
        let mut kerned = 0usize;
        let mut ligated = 0usize;
        for path in &files {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            let Ok(font) = Font::parse(&data) else {
                continue;
            };
            let shaper = Shaper::new(&font);
            kerned += usize::from(shaper.has_kerning());
            ligated += usize::from(shaper.has_ligatures());
            for text in ["AVATAR", "Waffle office", "To, To. Yo", "flight fjord"] {
                let on = shaper.shape(text, Features::ALL);
                let off = shaper.shape(text, Features::NONE);
                assert!(on.len() <= off.len(), "{path:?}");
                assert!(on.windows(2).all(|w| w[0].cluster <= w[1].cluster));
            }
        }
        // If nothing kerns, the feature chain is broken rather than the corpus
        // being unusual: kerning is near-universal in shipped Latin fonts.
        assert!(
            kerned * 4 > files.len(),
            "only {kerned}/{} kern",
            files.len()
        );
        assert!(
            ligated * 4 > files.len(),
            "only {ligated}/{} ligate",
            files.len()
        );
    }
}
