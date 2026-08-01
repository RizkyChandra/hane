//! Text layout: grapheme clusters, line breaking, alignment and metrics.
//!
//! Three separable pieces. [`grapheme_boundaries`] and its cursor helpers are
//! pure text and need no font; [`line_breaks`] likewise; [`Paragraph::layout`]
//! puts them together with a [`Shaper`] and produces positioned glyphs in
//! pixels.
//!
//! # UAX #29, grapheme cluster boundaries
//!
//! *Extended* grapheme clusters, rules GB1-GB13. The classes are `CR`, `LF`,
//! `Control`, `Extend`, `ZWJ`, `Regional_Indicator`, `Prepend`, `SpacingMark`
//! and the Hangul syllable types `L`, `V`, `T`, `LV` and `LVT`; everything
//! else is `Other`. The class tables are generated from the Unicode 16.0
//! character database: `Extend` is `Mn | Me | Other_Grapheme_Extend`,
//! `SpacingMark` is `Mc` minus the exceptions UAX #29 names, and `Control` is
//! `Cc | Cf | Zl | Zp` minus CR, LF, ZWNJ, ZWJ and the `Prepend` format
//! characters.
//!
//! Two gaps, both outside D-009's Latin scope:
//!
//! - **GB9c**, the Indic conjunct rule, is not implemented. A Devanagari
//!   consonant joined to the next by a virama splits into two clusters where
//!   the specification keeps one. It needs the `InCB` property, which is a
//!   table for nine scripts none of which this phase renders.
//! - **GB11** needs `Extended_Pictographic`, which lives in `emoji-data.txt`
//!   rather than in the character database, so [`is_extended_pictographic`]
//!   uses the emoji blocks instead. A ZWJ sequence built from a pictograph
//!   outside those blocks splits where it should not.
//!
//! Against the Unicode 16.0 `GraphemeBreakTest.txt`: 1086 of 1093 cases, and
//! all seven failures are the GB9c cases above. Restricted to Latin, 37 of 37.
//!
//! # UAX #14, line break opportunities
//!
//! Rules LB2-LB31 over the classes Latin text produces: `BK`, `CR`, `LF`,
//! `NL`, `SP`, `ZW`, `ZWJ`, `CM`, `WJ`, `GL`, `AL`, `HL`, `NU`, `PR`, `PO`,
//! `OP`, `CL`, `CP`, `QU`, `IS`, `SY`, `HY`, `BA`, `BB`, `B2`, `EX`, `IN`,
//! `NS`, `ID` and `RI`. `AI`, `SG`, `XX` and everything unlisted resolve to
//! `AL` and `CJ` to `NS`, which is what LB1 prescribes. The Unicode 15.1
//! quotation rules LB15a and LB15b and the leading-hyphen rule LB20a are
//! implemented, because `"quoted"`, `<<guillemets>>` and `-flag` are Latin
//! typography and getting them wrong is visible in one paragraph.
//!
//! Deliberately absent: the Hangul jamo classes and rules LB26-LB27, the emoji
//! rule LB30b, and LB20's `CB` class, which is for objects embedded in text
//! that this crate has no concept of. Beyond Latin the *class table* thins out
//! rather than the rules: it is written by hand from `LineBreak.txt` for the
//! ranges real documents use, and a CJK or Indic character that falls through
//! it becomes `AL` or `ID` rather than its true class.
//!
//! Against the Unicode 16.0 `LineBreakTest.txt`: 14341 of 16672 cases overall
//! -- most failures are the thin class table on CJK and emoji -- and 1990 of
//! 2000 restricted to Latin. Those ten are LB25, the numeric rule, which is
//! implemented as its pairwise approximation rather than as the regular
//! expression: it holds `$1,234.00` together, and can also hold together a
//! sequence such as `)$` that the full grammar would break.
//!
//! # Cursor positions
//!
//! [`snap_to_grapheme`], [`next_grapheme`] and [`prev_grapheme`] are the only
//! way a caller should move a caret. A byte offset in the middle of a cluster
//! is snapped back to the cluster's start, so no edit can split `e` from the
//! combining acute that follows it.

use crate::opentype::Font;
use crate::shape::{Features, Glyph, Shaper};
use core::ops::Range;

// ---------------------------------------------------------------------------
// UAX #29 -- grapheme clusters
// ---------------------------------------------------------------------------

/// The UAX #29 grapheme cluster break property, as far as this module needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Grapheme {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    RegionalIndicator,
    Prepend,
    SpacingMark,
    HangulL,
    HangulV,
    HangulT,
    HangulLv,
    HangulLvt,
}

/// Whether `ch` is `Extended_Pictographic`, approximately.
///
/// The exact property is in `emoji-data.txt`, which is not part of the
/// character database; these are the blocks that are entirely pictographic
/// plus the handful of BMP symbols that emoji sequences actually use. It only
/// ever affects GB11, so a miss costs a split emoji sequence and nothing else.
pub fn is_extended_pictographic(ch: char) -> bool {
    matches!(ch as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139
        | 0x2194..=0x21AA | 0x231A..=0x23FA | 0x24C2
        | 0x25AA..=0x25FE | 0x2600..=0x27BF | 0x2934..=0x2935
        | 0x2B05..=0x2B55 | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x1F000..=0x1F0FF | 0x1F10D..=0x1F1AD | 0x1F201..=0x1F2FF
        | 0x1F300..=0x1F5FF | 0x1F600..=0x1F64F | 0x1F680..=0x1F6FF
        | 0x1F7E0..=0x1F7FF | 0x1F900..=0x1FAFF)
}

/// The grapheme break class of `ch`.
fn grapheme_class(ch: char) -> Grapheme {
    let cp = ch as u32;
    match cp {
        0x0D => return Grapheme::Cr,
        0x0A => return Grapheme::Lf,
        0x200D => return Grapheme::Zwj,
        0x1F1E6..=0x1F1FF => return Grapheme::RegionalIndicator,
        // Hangul. The syllable block is L, V and optionally T fused into one
        // code point; `% 28` recovers whether the T slot was used.
        0x1100..=0x115F | 0xA960..=0xA97C => return Grapheme::HangulL,
        0x1160..=0x11A7 | 0xD7B0..=0xD7C6 => return Grapheme::HangulV,
        0x11A8..=0x11FF | 0xD7CB..=0xD7FB => return Grapheme::HangulT,
        0xAC00..=0xD7A3 => {
            return if (cp - 0xAC00).is_multiple_of(28) {
                Grapheme::HangulLv
            } else {
                Grapheme::HangulLvt
            };
        }
        _ => {}
    }
    // Extend before Control: a few characters are in both sets by category,
    // and Extend is the one that keeps a cluster together.
    if in_ranges(EXTEND, cp) {
        Grapheme::Extend
    } else if in_ranges(SPACING_MARK, cp) {
        Grapheme::SpacingMark
    } else if in_ranges(PREPEND, cp) {
        Grapheme::Prepend
    } else if in_ranges(CONTROL, cp) {
        Grapheme::Control
    } else {
        Grapheme::Other
    }
}

/// Membership in a sorted, disjoint range table.
fn in_ranges(table: &[(u32, u32)], cp: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                core::cmp::Ordering::Less
            } else if lo > cp {
                core::cmp::Ordering::Greater
            } else {
                core::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Byte offsets of every grapheme cluster boundary in `text`, ascending.
///
/// Always starts with `0` and, for non-empty text, ends with `text.len()`:
/// these are exactly the positions a caret may occupy.
pub fn grapheme_boundaries(text: &str) -> Vec<usize> {
    let mut out = vec![0];
    if text.is_empty() {
        return out;
    }
    let mut previous: Option<(Grapheme, char)> = None;
    // Consecutive regional indicators ending at `previous`, for GB12/GB13, and
    // the two states of the GB11 pattern `ExtPict Extend* ZWJ`.
    let mut flag_run = 0usize;
    let mut pictograph_run = false;
    let mut pictograph_zwj = false;

    for (offset, ch) in text.char_indices() {
        let class = grapheme_class(ch);
        if let Some((previous_class, _)) = previous
            && breaks_before(previous_class, class, ch, flag_run, pictograph_zwj)
        {
            out.push(offset);
        }
        flag_run = if class == Grapheme::RegionalIndicator {
            flag_run + 1
        } else {
            0
        };
        if is_extended_pictographic(ch) {
            pictograph_run = true;
            pictograph_zwj = false;
        } else if class == Grapheme::Extend && pictograph_run {
            // Extend* between the pictograph and the ZWJ: state unchanged.
        } else if class == Grapheme::Zwj && pictograph_run {
            pictograph_run = false;
            pictograph_zwj = true;
        } else {
            pictograph_run = false;
            pictograph_zwj = false;
        }
        previous = Some((class, ch));
    }
    out.push(text.len());
    out
}

/// GB3 through GB999, in the order the specification lists them.
fn breaks_before(
    previous: Grapheme,
    current: Grapheme,
    ch: char,
    flag_run: usize,
    pictograph_zwj: bool,
) -> bool {
    use Grapheme::*;
    match (previous, current) {
        (Cr, Lf) => false,                                            // GB3
        (Control | Cr | Lf, _) => true,                               // GB4
        (_, Control | Cr | Lf) => true,                               // GB5
        (HangulL, HangulL | HangulV | HangulLv | HangulLvt) => false, // GB6
        (HangulLv | HangulV, HangulV | HangulT) => false,             // GB7
        (HangulLvt | HangulT, HangulT) => false,                      // GB8
        (_, Extend | Zwj) => false,                                   // GB9
        (_, SpacingMark) => false,                                    // GB9a
        (Prepend, _) => false,                                        // GB9b
        // GB11: ExtPict Extend* ZWJ x ExtPict.
        _ if pictograph_zwj && is_extended_pictographic(ch) => false,
        // GB12/GB13: an odd number of preceding flags means this one pairs
        // with the last, so a run of four regional indicators is two flags.
        (RegionalIndicator, RegionalIndicator) => flag_run.is_multiple_of(2),
        _ => true, // GB999
    }
}

/// Whether `byte` is a grapheme cluster boundary.
///
/// A byte offset that is not a character boundary is never a cluster boundary.
pub fn is_grapheme_boundary(text: &str, byte: usize) -> bool {
    byte == 0 || byte == text.len() || grapheme_boundaries(text).contains(&byte)
}

/// The largest cluster boundary at or before `byte`.
///
/// This is the function a caret assignment goes through. An offset past the end
/// clamps to the end, and an offset inside a cluster -- or inside a character --
/// moves back to where that cluster starts.
pub fn snap_to_grapheme(text: &str, byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    grapheme_boundaries(text)
        .into_iter()
        .rev()
        .find(|&b| b <= byte)
        .unwrap_or(0)
}

/// The next cluster boundary strictly after `byte`, or the end of the text.
pub fn next_grapheme(text: &str, byte: usize) -> usize {
    grapheme_boundaries(text)
        .into_iter()
        .find(|&b| b > byte)
        .unwrap_or(text.len())
}

/// The previous cluster boundary strictly before `byte`, or `0`.
pub fn prev_grapheme(text: &str, byte: usize) -> usize {
    grapheme_boundaries(text)
        .into_iter()
        .rev()
        .find(|&b| b < byte)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// UAX #14 -- line breaking
// ---------------------------------------------------------------------------

/// The UAX #14 line break classes this module distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    Bk,
    Cr,
    Lf,
    Nl,
    Sp,
    Zw,
    Zwj,
    Cm,
    Wj,
    Gl,
    Al,
    Hl,
    Nu,
    Pr,
    Po,
    Op,
    Cl,
    Cp,
    Qu,
    Is,
    Sy,
    Hy,
    Ba,
    Bb,
    B2,
    Ex,
    In,
    Ns,
    Id,
    Ri,
}

/// A place a line may end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineBreak {
    /// Byte offset where the *next* line starts.
    pub offset: usize,
    /// A break that must be taken -- a newline, a form feed, `U+2028`.
    pub mandatory: bool,
}

/// The line break class of `ch`.
///
/// ASCII is spelled out because it is where Latin text lives and where the
/// rules bite; beyond it the table thins out to the ranges that appear in
/// real documents, with everything unlisted resolving to `AL` as LB1 says.
fn break_class(ch: char) -> Break {
    use Break::*;
    let cp = ch as u32;
    if cp < 0x80 {
        return match ch {
            '\t' => Ba,
            '\n' => Lf,
            '\r' => Cr,
            '\x0B' | '\x0C' => Bk,
            ' ' => Sp,
            '!' | '?' => Ex,
            '"' | '\'' => Qu,
            '$' | '+' | '\\' => Pr,
            '%' => Po,
            '(' | '[' | '{' => Op,
            ')' | ']' => Cp,
            '}' => Cl,
            ',' | '.' | ':' | ';' => Is,
            '-' => Hy,
            '/' => Sy,
            '|' => Ba,
            '0'..='9' => Nu,
            'A'..='Z' | 'a'..='z' => Al,
            c if (c as u32) < 0x20 || c as u32 == 0x7F => Cm,
            _ => Al,
        };
    }
    match cp {
        0x85 => Nl,
        0xA0 | 0x2007 | 0x2011 | 0x202F => Gl,
        0xA1 | 0xBF => Op,
        0xA2 | 0xB0 | 0x2030..=0x2031 | 0x2103 | 0x2109 => Po,
        0xA3..=0xA5 | 0xB1 | 0x20A0..=0x20CF => Pr,
        0xAB | 0xBB | 0x2018..=0x201F | 0x2039 | 0x203A => Qu,
        0xAD | 0x2000..=0x2006 | 0x2008..=0x200A | 0x2010 | 0x2012 | 0x2013 | 0x058A => Ba,
        0x200B => Zw,
        0x200D => Zwj,
        0x2014 => B2,
        0x2024..=0x2026 => In,
        0x2028 | 0x2029 => Bk,
        0x2044 => Is,
        0x2060 | 0xFEFF => Wj,
        0x00B4 | 0x02C8 | 0x02CC | 0x1FFD => Bb,
        0x301C | 0x30FB | 0xFF65 => Ns,
        0x05D0..=0x05F2 => Hl,
        0x1F1E6..=0x1F1FF => Ri,
        // Ideographic, which is where the small-kana `CJ` class and the emoji
        // classes end up too: a break between any two of them is allowed.
        0x1100..=0x11FF
        | 0x2E80..=0x303A
        | 0x303C..=0xA4CF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0x1F300..=0x1FAFF
        | 0x20000..=0x3FFFD => Id,
        _ => {
            // Combining marks bind to what precedes them (LB9); anything else
            // unlisted is alphabetic.
            if in_ranges(EXTEND, cp) || in_ranges(SPACING_MARK, cp) {
                Cm
            } else {
                Al
            }
        }
    }
}

/// One position that survived LB9's absorption of combining marks.
struct Cluster {
    offset: usize,
    class: Break,
    /// LB8a: no break after a zero width joiner, even one LB9 absorbed.
    ends_with_zwj: bool,
    /// `Pi`, an opening quote: LB15a hangs off this.
    initial_quote: bool,
    /// `Pf`, a closing quote: LB15b hangs off this.
    final_quote: bool,
    /// A hyphen for LB20a's purposes, which names `HY` and `U+2010` by hand.
    hyphen: bool,
}

/// The `Pi` general category, restricted to characters that are also `QU`.
///
/// Latin quotation marks, which is all LB15a needs; the set is short enough
/// that a table would cost more than it saves.
fn is_initial_quote(ch: char) -> bool {
    matches!(
        ch as u32,
        0x00AB
            | 0x2018
            | 0x201B
            | 0x201C
            | 0x201F
            | 0x2039
            | 0x2E02
            | 0x2E04
            | 0x2E09
            | 0x2E0C
            | 0x2E1C
            | 0x2E20
    )
}

/// The `Pf` general category, restricted to characters that are also `QU`.
fn is_final_quote(ch: char) -> bool {
    matches!(
        ch as u32,
        0x00BB | 0x2019 | 0x201D | 0x203A | 0x2E03 | 0x2E05 | 0x2E0A | 0x2E0D | 0x2E1D | 0x2E21
    )
}

/// Every place a line may end in `text`, ascending, always ending with a
/// mandatory break at `text.len()` (LB3).
///
/// Offsets are where the following line starts, so `text[..offset]` is a
/// complete line -- including its trailing spaces, which a renderer trims.
pub fn line_breaks(text: &str) -> Vec<LineBreak> {
    use Break::*;
    let mut out = Vec::new();

    // LB9: X (CM | ZWJ)* becomes X, except after a break or a space, where
    // LB10 turns the mark itself into AL. Doing it as a pre-pass means the
    // rules below never see a combining mark.
    let mut clusters: Vec<Cluster> = Vec::new();
    for (offset, ch) in text.char_indices() {
        let class = break_class(ch);
        let attaches = matches!(class, Cm | Zwj)
            && clusters
                .last()
                .is_some_and(|c| !matches!(c.class, Bk | Cr | Lf | Nl | Sp | Zw));
        if attaches {
            let last = clusters.last_mut().expect("attaches implies a previous");
            last.ends_with_zwj = class == Zwj;
        } else {
            clusters.push(Cluster {
                offset,
                class: if matches!(class, Cm | Zwj) { Al } else { class },
                ends_with_zwj: class == Zwj,
                initial_quote: class == Qu && is_initial_quote(ch),
                final_quote: class == Qu && is_final_quote(ch),
                hyphen: class == Hy || ch as u32 == 0x2010,
            });
        }
    }

    // `before_space` is the class that preceded the current run of spaces,
    // which is what the SP* in LB8, LB14, LB15, LB16 and LB17 needs.
    if let Some(first) = clusters.first() {
        let mut before_space = first.class;
        let mut flag_run = usize::from(first.class == Ri);
        // LB15a: an opening quote in a position where one can open, with
        // nothing but spaces since. `sot` counts as such a position.
        let mut open_quote = first.initial_quote;
        // LB20a: a hyphen that opens a word binds to the word.
        let mut leading_hyphen = first.hyphen;
        for i in 1..clusters.len() {
            let (previous, current) = (&clusters[i - 1], &clusters[i]);
            let next = clusters.get(i + 1).map(|c| c.class);
            if let Some(mandatory) = break_after(
                previous,
                current,
                next,
                before_space,
                flag_run,
                open_quote,
                leading_hyphen,
            ) {
                out.push(LineBreak {
                    offset: current.offset,
                    mandatory,
                });
            }
            leading_hyphen =
                current.hyphen && matches!(previous.class, Bk | Cr | Lf | Nl | Sp | Zw | Gl);
            open_quote = if current.initial_quote {
                matches!(previous.class, Bk | Cr | Lf | Nl | Op | Qu | Gl | Sp | Zw)
            } else {
                open_quote && current.class == Sp
            };
            if current.class != Sp {
                before_space = current.class;
            }
            flag_run = if current.class == Ri { flag_run + 1 } else { 0 };
        }
    }
    out.push(LineBreak {
        offset: text.len(),
        mandatory: true,
    });
    out
}

/// `Some(mandatory)` when a line may end between `previous` and `current`.
fn break_after(
    previous: &Cluster,
    cluster: &Cluster,
    next: Option<Break>,
    before_space: Break,
    flag_run: usize,
    open_quote: bool,
    leading_hyphen: bool,
) -> Option<bool> {
    use Break::*;
    let (p, current) = (previous.class, cluster.class);
    let allowed = Some(false);
    let required = Some(true);
    let no = None;

    if previous.ends_with_zwj {
        return no; // LB8a
    }
    match (p, current) {
        (Bk, _) => required,           // LB4
        (Cr, Lf) => no,                // LB5
        (Cr | Lf | Nl, _) => required, // LB5
        (_, Bk | Cr | Lf | Nl) => no,  // LB6
        (_, Sp | Zw) => no,            // LB7
        // LB8, before LB18 claims the space run.
        _ if before_space == Zw => allowed,
        (Wj, _) | (_, Wj) => no, // LB11
        (Gl, _) => no,           // LB12
        // LB12a. The three exempt classes fall through to the rules below,
        // where only LB18 and LB31 can still fire.
        (_, Gl) if !matches!(p, Sp | Ba | Hy) => no,
        (_, Cl | Cp | Ex | Is | Sy) => no, // LB13
        _ if before_space == Op && matches!(p, Op | Sp) => no, // LB14
        // LB15a: nothing breaks between an opening quote and what it opens,
        // however many spaces the author left in between.
        _ if open_quote => no,
        // LB15b: nor before a closing quote that is itself followed by
        // something a line may not start with.
        _ if cluster.final_quote
            && next.is_none_or(|n| {
                matches!(
                    n,
                    Sp | Gl | Wj | Cl | Qu | Cp | Ex | Is | Sy | Bk | Cr | Lf | Nl | Zw
                )
            }) =>
        {
            no
        }
        _ if matches!(before_space, Cl | Cp) && current == Ns => no, // LB16
        _ if before_space == B2 && current == B2 => no,              // LB17
        (Sp, _) => allowed,                                          // LB18
        (Qu, _) | (_, Qu) => no,                                     // LB19
        // LB20a: `-tietokone` and `- item` keep their leading hyphen.
        _ if leading_hyphen && matches!(current, Al | Hl) => no,
        (_, Ba | Hy | Ns) | (Bb, _) => no,             // LB21
        (Sy, Hl) => no,                                // LB21b
        (_, In) => no,                                 // LB22
        (Al | Hl, Nu) | (Nu, Al | Hl) => no,           // LB23
        (Pr, Id) | (Id, Po) => no,                     // LB23a
        (Pr | Po, Al | Hl) | (Al | Hl, Pr | Po) => no, // LB24
        // LB25, pairwise: enough to hold a price or a decimal together.
        (Cl | Cp | Nu, Po | Pr) | (Po | Pr, Nu) | (Hy | Is | Nu, Nu) | (Sy, Nu) => no,
        (Al | Hl, Al | Hl) => no,                      // LB28
        (Is, Al | Hl) => no,                           // LB29
        (Al | Hl | Nu, Op) | (Cp, Al | Hl | Nu) => no, // LB30
        // LB30a: flags pair up, so a break falls between pairs.
        (Ri, Ri) if !flag_run.is_multiple_of(2) => no,
        _ => allowed, // LB31
    }
}

// ---------------------------------------------------------------------------
// Metrics, alignment and the paragraph
// ---------------------------------------------------------------------------

/// Vertical metrics of a line, in pixels at the size they were taken for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    /// Baseline to the top of the ascenders, positive upwards.
    pub ascent: f64,
    /// Baseline to the bottom of the descenders, negative.
    pub descent: f64,
    /// Extra leading the designer asked for between lines.
    pub line_gap: f64,
}

impl LineMetrics {
    /// Vertical metrics of `font` at `size` pixels per em.
    ///
    /// `hhea` is the source, except when `OS/2` sets `USE_TYPO_METRICS`, which
    /// is a font explicitly saying its typographic metrics are the right ones.
    /// A font whose `hhea` ascent is zero -- which happens, and would collapse
    /// every line on top of the last -- falls back to the `OS/2` Windows
    /// metrics, the pair that is never zero in a font that renders anywhere.
    pub fn from_font(font: &Font<'_>, size: f64) -> Self {
        let scale = size / f64::from(font.units_per_em());
        let os2 = font.os2();
        let use_typo = os2.is_some_and(|o| o.fs_selection & 0x0080 != 0);
        let hhea = font.hhea();
        let (ascent, descent, line_gap) = match os2 {
            Some(o) if use_typo => (
                f64::from(o.typo_ascender),
                f64::from(o.typo_descender),
                f64::from(o.typo_line_gap),
            ),
            _ if hhea.ascender != 0 => (
                f64::from(hhea.ascender),
                f64::from(hhea.descender),
                f64::from(hhea.line_gap),
            ),
            Some(o) => (f64::from(o.win_ascent), -f64::from(o.win_descent), 0.0),
            None => (
                font.head().bounding_box.y1,
                font.head().bounding_box.y0,
                0.0,
            ),
        };
        Self {
            ascent: ascent * scale,
            descent: descent * scale,
            line_gap: line_gap * scale,
        }
    }

    /// Baseline-to-baseline distance.
    pub fn line_height(&self) -> f64 {
        self.ascent - self.descent + self.line_gap
    }
}

/// How lines sit inside the layout width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Align {
    /// Flush left, ragged right.
    #[default]
    Left,
    /// Flush right, ragged left.
    Right,
    /// Centred, ragged both sides.
    Center,
    /// Flush both sides, by stretching the spaces. The last line of the
    /// paragraph and any line ending at a mandatory break stay left-aligned,
    /// because stretching a half-empty line is the classic justification bug.
    Justify,
}

/// A glyph placed on a line, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacedGlyph {
    /// Glyph index in the font.
    pub id: u16,
    /// Byte offset of the first character this glyph came from.
    pub cluster: usize,
    /// Pen position: the left edge of the glyph's advance, not of its ink.
    pub x: f64,
    /// How far the pen moves after it, including any justification stretch.
    pub advance: f64,
    /// Positioning displacement of the outline from the pen, from GPOS.
    pub x_offset: f64,
    /// Positioning displacement above the baseline, from GPOS.
    pub y_offset: f64,
}

/// One laid-out line.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    /// Byte range of the source text this line covers, trailing spaces and all.
    pub range: Range<usize>,
    /// The glyphs, in visual order, already offset by the alignment.
    pub glyphs: Vec<PlacedGlyph>,
    /// Distance from the top of the paragraph to this line's baseline.
    pub baseline: f64,
    /// Left edge after alignment.
    pub x: f64,
    /// Advance width excluding trailing whitespace, which is what alignment
    /// measures: a right-aligned line must not hang its trailing space.
    pub width: f64,
}

/// A paragraph broken into lines and positioned.
#[derive(Clone, Debug, PartialEq)]
pub struct Paragraph {
    /// The lines, top to bottom.
    pub lines: Vec<Line>,
    /// Vertical metrics every line shares.
    pub metrics: LineMetrics,
    /// The widest line.
    pub width: f64,
    /// `lines.len()` line heights.
    pub height: f64,
}

impl Paragraph {
    /// Shape and break `text` into lines no wider than `max_width` pixels.
    ///
    /// `size` is pixels per em. A word longer than `max_width` overflows rather
    /// than being chopped: there is no break opportunity inside it, and
    /// inventing one would hyphenate without a dictionary.
    ///
    /// The whole paragraph is shaped once and then cut at break offsets, so a
    /// pair that kerns across a line end keeps the kerning it would have had
    /// mid-line. The alternative -- reshaping every candidate line -- costs a
    /// shaping pass per break for a sub-pixel difference at one glyph.
    pub fn layout(
        shaper: &Shaper<'_>,
        text: &str,
        size: f64,
        max_width: f64,
        align: Align,
        features: Features,
    ) -> Self {
        let font = shaper.font();
        let metrics = LineMetrics::from_font(font, size);
        let scale = size / f64::from(font.units_per_em());
        let glyphs = shaper.shape(text, features);
        // Prefix advances keyed by cluster, so measuring a candidate line is a
        // binary search rather than a walk: greedy breaking measures O(breaks)
        // candidates and a paragraph can have thousands.
        let prefix = prefix_advances(&glyphs);

        let breaks = line_breaks(text);
        let mut lines: Vec<Line> = Vec::new();
        // A line ending at a mandatory break is the last of its paragraph even
        // when more lines follow, and must not be justified.
        let mut ends_hard: Vec<bool> = Vec::new();
        let (mut start, mut i) = (0usize, 0usize);
        while i < breaks.len() {
            let mut chosen = None;
            while i < breaks.len() {
                let candidate = breaks[i];
                let end = trim_end(text, start, candidate.offset);
                let width = (measure(&prefix, end) - measure(&prefix, start)) * scale;
                // The first candidate is taken however wide it is; there is no
                // narrower way to end this line.
                if chosen.is_some() && width > max_width {
                    break;
                }
                chosen = Some(candidate);
                i += 1;
                if candidate.mandatory {
                    break;
                }
            }
            let candidate = chosen.expect("the inner loop takes at least one candidate");
            lines.push(place(
                text,
                &glyphs,
                scale,
                start..candidate.offset,
                metrics.ascent + lines.len() as f64 * metrics.line_height(),
            ));
            ends_hard.push(candidate.mandatory);
            start = candidate.offset;
        }

        let last = lines.len().saturating_sub(1);
        for (index, line) in lines.iter_mut().enumerate() {
            align_line(
                line,
                text,
                align,
                max_width,
                index == last || ends_hard[index],
            );
        }
        Self {
            width: lines.iter().fold(0.0f64, |w, l| w.max(l.width)),
            height: lines.len() as f64 * metrics.line_height(),
            lines,
            metrics,
        }
    }

    /// Caret position for byte offset `byte`, as `(x, baseline)` in pixels.
    ///
    /// `byte` is snapped to a grapheme cluster boundary first, so a caret can
    /// never land between a base character and its combining marks. Inside a
    /// ligature the advance is divided evenly among the clusters the ligature
    /// covers, which is the only division available without per-component
    /// metrics the font does not carry.
    pub fn cursor(&self, text: &str, byte: usize) -> (f64, f64) {
        let byte = snap_to_grapheme(text, byte);
        let Some(line) = self
            .lines
            .iter()
            .find(|l| byte < l.range.end)
            .or_else(|| self.lines.last())
        else {
            return (0.0, self.metrics.ascent);
        };
        let mut x = line.x;
        for (index, glyph) in line.glyphs.iter().enumerate() {
            let end = line
                .glyphs
                .get(index + 1)
                .map_or(line.range.end, |next| next.cluster);
            if byte < end {
                if byte <= glyph.cluster {
                    return (glyph.x, line.baseline);
                }
                let span = text.get(glyph.cluster..end).unwrap_or("");
                let bounds = grapheme_boundaries(span);
                let steps = bounds.len().saturating_sub(1).max(1);
                let taken = bounds
                    .iter()
                    .position(|&b| b + glyph.cluster >= byte)
                    .unwrap_or(steps);
                return (
                    glyph.x + glyph.advance * taken as f64 / steps as f64,
                    line.baseline,
                );
            }
            x = glyph.x + glyph.advance;
        }
        (x, line.baseline)
    }
}

/// `(cluster, advance of everything before it)` for each glyph, plus a
/// sentinel holding the total.
fn prefix_advances(glyphs: &[Glyph]) -> Vec<(usize, i64)> {
    let mut out = Vec::with_capacity(glyphs.len() + 1);
    let mut total = 0i64;
    for glyph in glyphs {
        out.push((glyph.cluster, total));
        total += i64::from(glyph.x_advance);
    }
    out.push((usize::MAX, total));
    out
}

/// Advance of everything before byte offset `byte`, in font units.
fn measure(prefix: &[(usize, i64)], byte: usize) -> f64 {
    let index = prefix.partition_point(|&(cluster, _)| cluster < byte);
    prefix.get(index).map_or(0.0, |&(_, sum)| sum as f64)
}

/// `end` with trailing whitespace removed, but never below `start`.
fn trim_end(text: &str, start: usize, end: usize) -> usize {
    let Some(slice) = text.get(start..end) else {
        return end;
    };
    start + slice.trim_end_matches(char::is_whitespace).len()
}

/// Collect the glyphs of one byte range into a line at `x = 0`.
fn place(text: &str, glyphs: &[Glyph], scale: f64, range: Range<usize>, baseline: f64) -> Line {
    let mut placed = Vec::new();
    let mut x = 0.0;
    let mut width = 0.0;
    let trimmed = trim_end(text, range.start, range.end);
    for glyph in glyphs
        .iter()
        .filter(|g| g.cluster >= range.start && g.cluster < range.end)
    {
        let advance = f64::from(glyph.x_advance) * scale;
        placed.push(PlacedGlyph {
            id: glyph.id,
            cluster: glyph.cluster,
            x,
            advance,
            x_offset: f64::from(glyph.x_offset) * scale,
            y_offset: f64::from(glyph.y_offset) * scale,
        });
        x += advance;
        if glyph.cluster < trimmed {
            width = x;
        }
    }
    Line {
        range,
        glyphs: placed,
        baseline,
        x: 0.0,
        width,
    }
}

/// Shift, and for `Justify` stretch, one line to fit `max_width`.
fn align_line(line: &mut Line, text: &str, align: Align, max_width: f64, is_last: bool) {
    let slack = max_width - line.width;
    if align == Align::Justify && !is_last && slack > 0.0 {
        // Stretch the gaps between words, not the letters: letter-spacing a
        // justified line is a different typographic decision, and not this one.
        // Trailing spaces are excluded -- they are past `width` and stretching
        // them would push the visible text left of the margin.
        let stretched: Vec<bool> = line
            .glyphs
            .iter()
            .map(|g| {
                g.x + g.advance <= line.width
                    && text.get(g.cluster..).is_some_and(|s| s.starts_with(' '))
            })
            .collect();
        let count = stretched.iter().filter(|s| **s).count();
        if count > 0 {
            let extra = slack / count as f64;
            let mut shift = 0.0;
            for (glyph, stretch) in line.glyphs.iter_mut().zip(stretched) {
                glyph.x += shift;
                if stretch {
                    glyph.advance += extra;
                    shift += extra;
                }
            }
            line.width = max_width;
            return;
        }
    }
    let shift = match align {
        Align::Left | Align::Justify => 0.0,
        Align::Right => slack,
        Align::Center => slack / 2.0,
    };
    for glyph in &mut line.glyphs {
        glyph.x += shift;
    }
    line.x += shift;
}

// ---------------------------------------------------------------------------
// Character property tables, generated from the Unicode 16.0 character
// database. Sorted and disjoint, so `in_ranges` can binary search them.
// ---------------------------------------------------------------------------

/// `Grapheme_Extend`: categories Mn and Me plus `Other_Grapheme_Extend`.
const EXTEND: &[(u32, u32)] = &[
    (0x0300, 0x036F),
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F3),
    (0x07FD, 0x07FD),
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    (0x0859, 0x085B),
    (0x0897, 0x089F),
    (0x08CA, 0x08E1),
    (0x08E3, 0x0902),
    (0x093A, 0x093A),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0981),
    (0x09BC, 0x09BC),
    (0x09BE, 0x09BE),
    (0x09C1, 0x09C4),
    (0x09CD, 0x09CD),
    (0x09D7, 0x09D7),
    (0x09E2, 0x09E3),
    (0x09FE, 0x09FE),
    (0x0A01, 0x0A02),
    (0x0A3C, 0x0A3C),
    (0x0A41, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A82),
    (0x0ABC, 0x0ABC),
    (0x0AC1, 0x0AC5),
    (0x0AC7, 0x0AC8),
    (0x0ACD, 0x0ACD),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFF),
    (0x0B01, 0x0B01),
    (0x0B3C, 0x0B3C),
    (0x0B3E, 0x0B3F),
    (0x0B41, 0x0B44),
    (0x0B4D, 0x0B4D),
    (0x0B55, 0x0B57),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BBE, 0x0BBE),
    (0x0BC0, 0x0BC0),
    (0x0BCD, 0x0BCD),
    (0x0BD7, 0x0BD7),
    (0x0C00, 0x0C00),
    (0x0C04, 0x0C04),
    (0x0C3C, 0x0C3C),
    (0x0C3E, 0x0C40),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C81),
    (0x0CBC, 0x0CBC),
    (0x0CBF, 0x0CBF),
    (0x0CC2, 0x0CC2),
    (0x0CC6, 0x0CC6),
    (0x0CCC, 0x0CCD),
    (0x0CD5, 0x0CD6),
    (0x0CE2, 0x0CE3),
    (0x0D00, 0x0D01),
    (0x0D3B, 0x0D3C),
    (0x0D3E, 0x0D3E),
    (0x0D41, 0x0D44),
    (0x0D4D, 0x0D4D),
    (0x0D57, 0x0D57),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D81),
    (0x0DCA, 0x0DCA),
    (0x0DCF, 0x0DCF),
    (0x0DD2, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0DDF, 0x0DDF),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC8, 0x0ECE),
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F71, 0x0F7E),
    (0x0F80, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x0FC6, 0x0FC6),
    (0x102D, 0x1030),
    (0x1032, 0x1037),
    (0x1039, 0x103A),
    (0x103D, 0x103E),
    (0x1058, 0x1059),
    (0x105E, 0x1060),
    (0x1071, 0x1074),
    (0x1082, 0x1082),
    (0x1085, 0x1086),
    (0x108D, 0x108D),
    (0x109D, 0x109D),
    (0x135D, 0x135F),
    (0x1712, 0x1714),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17B5),
    (0x17B7, 0x17BD),
    (0x17C6, 0x17C6),
    (0x17C9, 0x17D3),
    (0x17DD, 0x17DD),
    (0x180B, 0x180D),
    (0x180F, 0x180F),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193B),
    (0x1A17, 0x1A18),
    (0x1A1B, 0x1A1B),
    (0x1A56, 0x1A56),
    (0x1A58, 0x1A5E),
    (0x1A60, 0x1A60),
    (0x1A62, 0x1A62),
    (0x1A65, 0x1A6C),
    (0x1A73, 0x1A7C),
    (0x1A7F, 0x1A7F),
    (0x1AB0, 0x1ACE),
    (0x1B00, 0x1B03),
    (0x1B34, 0x1B3A),
    (0x1B3C, 0x1B3C),
    (0x1B42, 0x1B42),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B81),
    (0x1BA2, 0x1BA5),
    (0x1BA8, 0x1BA9),
    (0x1BAB, 0x1BAD),
    (0x1BE6, 0x1BE6),
    (0x1BE8, 0x1BE9),
    (0x1BED, 0x1BED),
    (0x1BEF, 0x1BF1),
    (0x1C2C, 0x1C33),
    (0x1C36, 0x1C37),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE0),
    (0x1CE2, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF8, 0x1CF9),
    (0x1DC0, 0x1DFF),
    (0x200C, 0x200C),
    (0x20D0, 0x20F0),
    (0x2CEF, 0x2CF1),
    (0x2D7F, 0x2D7F),
    (0x2DE0, 0x2DFF),
    (0x302A, 0x302F),
    (0x3099, 0x309A),
    (0xA66F, 0xA672),
    (0xA674, 0xA67D),
    (0xA69E, 0xA69F),
    (0xA6F0, 0xA6F1),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA825, 0xA826),
    (0xA82C, 0xA82C),
    (0xA8C4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA951),
    (0xA980, 0xA982),
    (0xA9B3, 0xA9B3),
    (0xA9B6, 0xA9B9),
    (0xA9BC, 0xA9BD),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA2E),
    (0xAA31, 0xAA32),
    (0xAA35, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4C),
    (0xAA7C, 0xAA7C),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAAEC, 0xAAED),
    (0xAAF6, 0xAAF6),
    (0xABE5, 0xABE5),
    (0xABE8, 0xABE8),
    (0xABED, 0xABED),
    (0xFB1E, 0xFB1E),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xFF9E, 0xFF9F),
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D6D),
    (0x10EAB, 0x10EAC),
    (0x10EFC, 0x10EFF),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11001, 0x11001),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11081),
    (0x110B3, 0x110B6),
    (0x110B9, 0x110BA),
    (0x110C2, 0x110C2),
    (0x11100, 0x11102),
    (0x11127, 0x1112B),
    (0x1112D, 0x11134),
    (0x11173, 0x11173),
    (0x11180, 0x11181),
    (0x111B6, 0x111BE),
    (0x111C9, 0x111CC),
    (0x111CF, 0x111CF),
    (0x1122F, 0x11231),
    (0x11234, 0x11234),
    (0x11236, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112DF),
    (0x112E3, 0x112EA),
    (0x11300, 0x11301),
    (0x1133B, 0x1133C),
    (0x11340, 0x11340),
    (0x11366, 0x1136C),
    (0x11370, 0x11374),
    (0x113BB, 0x113C0),
    (0x113CE, 0x113CE),
    (0x113D0, 0x113D0),
    (0x113D2, 0x113D2),
    (0x113E1, 0x113E2),
    (0x11438, 0x1143F),
    (0x11442, 0x11444),
    (0x11446, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B3, 0x114B8),
    (0x114BA, 0x114BA),
    (0x114BF, 0x114C0),
    (0x114C2, 0x114C3),
    (0x115B2, 0x115B5),
    (0x115BC, 0x115BD),
    (0x115BF, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11633, 0x1163A),
    (0x1163D, 0x1163D),
    (0x1163F, 0x11640),
    (0x116AB, 0x116AB),
    (0x116AD, 0x116AD),
    (0x116B0, 0x116B5),
    (0x116B7, 0x116B7),
    (0x1171D, 0x1171D),
    (0x1171F, 0x1171F),
    (0x11722, 0x11725),
    (0x11727, 0x1172B),
    (0x1182F, 0x11837),
    (0x11839, 0x1183A),
    (0x1193B, 0x1193C),
    (0x1193E, 0x1193E),
    (0x11943, 0x11943),
    (0x119D4, 0x119D7),
    (0x119DA, 0x119DB),
    (0x119E0, 0x119E0),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A38),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A56),
    (0x11A59, 0x11A5B),
    (0x11A8A, 0x11A96),
    (0x11A98, 0x11A99),
    (0x11C30, 0x11C36),
    (0x11C38, 0x11C3D),
    (0x11C3F, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CAA, 0x11CB0),
    (0x11CB2, 0x11CB3),
    (0x11CB5, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D90, 0x11D91),
    (0x11D95, 0x11D95),
    (0x11D97, 0x11D97),
    (0x11EF3, 0x11EF4),
    (0x11F00, 0x11F01),
    (0x11F36, 0x11F3A),
    (0x11F40, 0x11F40),
    (0x11F42, 0x11F42),
    (0x11F5A, 0x11F5A),
    (0x13440, 0x13440),
    (0x13447, 0x13455),
    (0x1611E, 0x16129),
    (0x1612D, 0x1612F),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16F4F, 0x16F4F),
    (0x16F8F, 0x16F92),
    (0x16FE4, 0x16FE4),
    (0x1BC9D, 0x1BC9E),
    (0x1CF00, 0x1CF2D),
    (0x1CF30, 0x1CF46),
    (0x1D165, 0x1D165),
    (0x1D167, 0x1D169),
    (0x1D16E, 0x1D172),
    (0x1D17B, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DA9F),
    (0x1DAA1, 0x1DAAF),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E130, 0x1E136),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E4EC, 0x1E4EF),
    (0x1E5EE, 0x1E5EF),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94A),
    (0x1F3FB, 0x1F3FF),
    (0xE0100, 0xE01EF),
];

/// `SpacingMark`: category Mc minus the exceptions UAX #29 lists.
const SPACING_MARK: &[(u32, u32)] = &[
    (0x0903, 0x0903),
    (0x093B, 0x093B),
    (0x093E, 0x0940),
    (0x0949, 0x094C),
    (0x094E, 0x094F),
    (0x0982, 0x0983),
    (0x09BF, 0x09C0),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CC),
    (0x0A03, 0x0A03),
    (0x0A3E, 0x0A40),
    (0x0A83, 0x0A83),
    (0x0ABE, 0x0AC0),
    (0x0AC9, 0x0AC9),
    (0x0ACB, 0x0ACC),
    (0x0B02, 0x0B03),
    (0x0B40, 0x0B40),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4C),
    (0x0BBF, 0x0BBF),
    (0x0BC1, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCC),
    (0x0C01, 0x0C03),
    (0x0C41, 0x0C44),
    (0x0C82, 0x0C83),
    (0x0CBE, 0x0CBE),
    (0x0CC0, 0x0CC1),
    (0x0CC3, 0x0CC4),
    (0x0CC7, 0x0CC8),
    (0x0CCA, 0x0CCB),
    (0x0CF3, 0x0CF3),
    (0x0D02, 0x0D03),
    (0x0D3F, 0x0D40),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4C),
    (0x0D82, 0x0D83),
    (0x0DD0, 0x0DD1),
    (0x0DD8, 0x0DDE),
    (0x0DF2, 0x0DF3),
    (0x0F3E, 0x0F3F),
    (0x0F7F, 0x0F7F),
    (0x1031, 0x1031),
    (0x103B, 0x103C),
    (0x1056, 0x1057),
    (0x1084, 0x1084),
    (0x1715, 0x1715),
    (0x1734, 0x1734),
    (0x17B6, 0x17B6),
    (0x17BE, 0x17C5),
    (0x17C7, 0x17C8),
    (0x1923, 0x1926),
    (0x1929, 0x192B),
    (0x1930, 0x1931),
    (0x1933, 0x1938),
    (0x1A19, 0x1A1A),
    (0x1A55, 0x1A55),
    (0x1A57, 0x1A57),
    (0x1A6D, 0x1A72),
    (0x1B04, 0x1B04),
    (0x1B3B, 0x1B3B),
    (0x1B3D, 0x1B41),
    (0x1B43, 0x1B44),
    (0x1B82, 0x1B82),
    (0x1BA1, 0x1BA1),
    (0x1BA6, 0x1BA7),
    (0x1BAA, 0x1BAA),
    (0x1BE7, 0x1BE7),
    (0x1BEA, 0x1BEC),
    (0x1BEE, 0x1BEE),
    (0x1BF2, 0x1BF3),
    (0x1C24, 0x1C2B),
    (0x1C34, 0x1C35),
    (0x1CE1, 0x1CE1),
    (0x1CF7, 0x1CF7),
    (0xA823, 0xA824),
    (0xA827, 0xA827),
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C3),
    (0xA952, 0xA953),
    (0xA983, 0xA983),
    (0xA9B4, 0xA9B5),
    (0xA9BA, 0xA9BB),
    (0xA9BE, 0xA9C0),
    (0xAA2F, 0xAA30),
    (0xAA33, 0xAA34),
    (0xAA4D, 0xAA4D),
    (0xAAEB, 0xAAEB),
    (0xAAEE, 0xAAEF),
    (0xAAF5, 0xAAF5),
    (0xABE3, 0xABE4),
    (0xABE6, 0xABE7),
    (0xABE9, 0xABEA),
    (0xABEC, 0xABEC),
    (0x11000, 0x11000),
    (0x11002, 0x11002),
    (0x11082, 0x11082),
    (0x110B0, 0x110B2),
    (0x110B7, 0x110B8),
    (0x1112C, 0x1112C),
    (0x11145, 0x11146),
    (0x11182, 0x11182),
    (0x111B3, 0x111B5),
    (0x111BF, 0x111C0),
    (0x111CE, 0x111CE),
    (0x1122C, 0x1122E),
    (0x11232, 0x11233),
    (0x11235, 0x11235),
    (0x112E0, 0x112E2),
    (0x11302, 0x11303),
    (0x1133E, 0x1133F),
    (0x11341, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134D),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x113B8, 0x113BA),
    (0x113C2, 0x113C2),
    (0x113C5, 0x113C5),
    (0x113C7, 0x113CA),
    (0x113CC, 0x113CD),
    (0x113CF, 0x113CF),
    (0x11435, 0x11437),
    (0x11440, 0x11441),
    (0x11445, 0x11445),
    (0x114B0, 0x114B2),
    (0x114B9, 0x114B9),
    (0x114BB, 0x114BE),
    (0x114C1, 0x114C1),
    (0x115AF, 0x115B1),
    (0x115B8, 0x115BB),
    (0x115BE, 0x115BE),
    (0x11630, 0x11632),
    (0x1163B, 0x1163C),
    (0x1163E, 0x1163E),
    (0x116AC, 0x116AC),
    (0x116AE, 0x116AF),
    (0x116B6, 0x116B6),
    (0x1171E, 0x1171E),
    (0x11726, 0x11726),
    (0x1182C, 0x1182E),
    (0x11838, 0x11838),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193D, 0x1193D),
    (0x11940, 0x11940),
    (0x11942, 0x11942),
    (0x119D1, 0x119D3),
    (0x119DC, 0x119DF),
    (0x119E4, 0x119E4),
    (0x11A39, 0x11A39),
    (0x11A57, 0x11A58),
    (0x11A97, 0x11A97),
    (0x11C2F, 0x11C2F),
    (0x11C3E, 0x11C3E),
    (0x11CA9, 0x11CA9),
    (0x11CB1, 0x11CB1),
    (0x11CB4, 0x11CB4),
    (0x11D8A, 0x11D8E),
    (0x11D93, 0x11D94),
    (0x11D96, 0x11D96),
    (0x11EF5, 0x11EF6),
    (0x11F03, 0x11F03),
    (0x11F34, 0x11F35),
    (0x11F3E, 0x11F3F),
    (0x11F41, 0x11F41),
    (0x1612A, 0x1612C),
    (0x16F51, 0x16F87),
    (0x16FF0, 0x16FF1),
    (0x1D166, 0x1D166),
    (0x1D16D, 0x1D16D),
];

/// `Control`: categories Cc, Cf, Zl and Zp, minus CR, LF, ZWNJ, ZWJ and Prepend.
const CONTROL: &[(u32, u32)] = &[
    (0x0000, 0x0009),
    (0x000B, 0x000C),
    (0x000E, 0x001F),
    (0x007F, 0x009F),
    (0x00AD, 0x00AD),
    (0x061C, 0x061C),
    (0x180E, 0x180E),
    (0x200B, 0x200B),
    (0x200E, 0x200F),
    (0x2028, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// `Prepend`: format characters that attach to what follows them.
const PREPEND: &[(u32, u32)] = &[
    (0x0600, 0x0605),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x0D4E, 0x0D4E),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x111C2, 0x111C3),
    (0x1193F, 0x1193F),
    (0x11941, 0x11941),
    (0x11A3A, 0x11A3A),
    (0x11A84, 0x11A89),
    (0x11D46, 0x11D46),
    (0x11F02, 0x11F02),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testfont::{font_with, gsub_liga, plain};
    use hane_geom::fuzz::{Rng, check};

    fn clusters(text: &str) -> Vec<&str> {
        let bounds = grapheme_boundaries(text);
        bounds.windows(2).map(|w| &text[w[0]..w[1]]).collect()
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        assert_eq!(clusters("e\u{301}"), ["e\u{301}"]);
        assert_eq!(clusters("a\u{301}\u{308}b"), ["a\u{301}\u{308}", "b"]);
        // A mark with no base is its own cluster rather than being dropped.
        assert_eq!(clusters("\u{301}a"), ["\u{301}", "a"]);
        assert_eq!(clusters(""), Vec::<&str>::new());
    }

    #[test]
    fn newlines_hangul_flags_and_emoji() {
        assert_eq!(clusters("a\r\nb"), ["a", "\r\n", "b"]); // GB3 keeps CRLF whole
        assert_eq!(clusters("\n\r"), ["\n", "\r"]); // but not the other order
        // GB6-GB8: conjoining jamo make one syllable.
        assert_eq!(
            clusters("\u{1100}\u{1161}\u{11A8}"),
            ["\u{1100}\u{1161}\u{11A8}"]
        );
        // GB12/GB13: regional indicators pair up, so four of them are two flags.
        assert_eq!(clusters("\u{1F1FA}\u{1F1F8}\u{1F1EF}\u{1F1F5}").len(), 2);
        assert_eq!(clusters("\u{1F1FA}\u{1F1F8}\u{1F1EF}").len(), 2);
        // GB11: an emoji ZWJ sequence is one cluster; a ZWJ between letters
        // does not fuse them into one, it just attaches to the first.
        assert_eq!(
            clusters("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}").len(),
            1
        );
        // GB9b: a prepending mark attaches forwards.
        assert_eq!(clusters("\u{0600}9"), ["\u{0600}9"]);
    }

    #[test]
    fn cursor_motion_never_lands_mid_cluster() {
        // The criterion P5 depends on: no byte offset, however it was produced,
        // can put a caret between a base character and its marks.
        for text in [
            "e\u{301}fg",
            "a\u{301}\u{308}b",
            "\u{1F1FA}\u{1F1F8}!",
            "\u{1F468}\u{200D}\u{1F469}x",
            "\u{1100}\u{1161}\u{11A8}z",
            "naïve café",
        ] {
            let bounds = grapheme_boundaries(text);
            for byte in 0..=text.len() {
                let snapped = snap_to_grapheme(text, byte);
                assert!(bounds.contains(&snapped), "{text:?} at {byte}");
                assert!(snapped <= byte);
                assert!(bounds.contains(&next_grapheme(text, byte)));
                assert!(bounds.contains(&prev_grapheme(text, byte)));
                assert!(is_grapheme_boundary(text, snapped));
            }
            // Walking forwards from 0 visits every boundary, once, in order.
            let mut walked = vec![0];
            while *walked.last().unwrap() < text.len() {
                walked.push(next_grapheme(text, *walked.last().unwrap()));
            }
            assert_eq!(walked, bounds);
            // And walking back retraces it.
            let mut back = vec![text.len()];
            while *back.last().unwrap() > 0 {
                back.push(prev_grapheme(text, *back.last().unwrap()));
            }
            back.reverse();
            assert_eq!(back, bounds);
        }
    }

    fn breaks(text: &str) -> Vec<(usize, bool)> {
        line_breaks(text)
            .into_iter()
            .map(|b| (b.offset, b.mandatory))
            .collect()
    }

    #[test]
    fn spaces_and_hyphens_are_the_break_opportunities() {
        assert_eq!(breaks("hello world"), [(6, false), (11, true)]);
        // A run of spaces is one opportunity, after the last of them (LB7/LB18).
        assert_eq!(breaks("a   b"), [(4, false), (5, true)]);
        // LB21: break after a hyphen, never before it.
        assert_eq!(breaks("foo-bar"), [(4, false), (7, true)]);
        // LB13: closing punctuation stays with the word it closes.
        assert_eq!(breaks("hi, there!"), [(4, false), (10, true)]);
        assert_eq!(breaks(""), [(0, true)]);
    }

    #[test]
    fn mandatory_breaks_and_glue() {
        assert_eq!(breaks("a\nb"), [(2, true), (3, true)]);
        assert_eq!(breaks("a\r\nb"), [(3, true), (4, true)]); // LB5 keeps CRLF whole
        assert_eq!(breaks("a\u{2028}b"), [(4, true), (5, true)]);
        // LB12: a no-break space is glue, and offers no opportunity at all.
        assert_eq!(breaks("a\u{00A0}b"), [(4, true)]);
        // LB11: the word joiner suppresses the opportunity a space would give.
        assert_eq!(breaks("a\u{2060}b"), [(5, true)]);
        // LB8: a zero width space is an opportunity inside a word.
        assert_eq!(breaks("ab\u{200B}cd"), [(5, false), (7, true)]);
    }

    #[test]
    fn quotes_and_leading_hyphens_bind_to_their_word() {
        // LB15b: no break before a closing quote at the end of a line.
        assert_eq!(breaks("a \u{00BB}"), [(4, true)]);
        // LB15a: nor after an opening one, however many spaces follow.
        assert_eq!(breaks("\u{00AB} a"), [(4, true)]);
        // But an ordinary quotation mark is not Pi or Pf, so the space after
        // it is still an opportunity.
        assert_eq!(breaks("\" ("), [(2, false), (3, true)]);
        // LB20a: a hyphen that starts a word keeps the word with it.
        assert_eq!(breaks("-abc"), [(4, true)]);
        assert_eq!(breaks("a -bc"), [(2, false), (5, true)]);
    }

    #[test]
    fn numbers_and_prices_hold_together() {
        // LB25, pairwise: no opportunity anywhere inside a formatted price.
        assert_eq!(breaks("$1,234.00"), [(9, true)]);
        assert_eq!(breaks("50%"), [(3, true)]);
        // LB30/LB23: a number against a letter or a bracket does not break.
        assert_eq!(breaks("x2 (y)"), [(3, false), (6, true)]);
    }

    #[test]
    fn metrics_come_from_the_font() {
        let data = plain();
        let font = crate::opentype::Font::parse(&data).unwrap();
        // hhea: 800 / -200 / 100 over a 1000 unit em, at 20 px.
        let m = LineMetrics::from_font(&font, 20.0);
        assert_eq!(m.ascent, 16.0);
        assert_eq!(m.descent, -4.0);
        assert_eq!(m.line_gap, 2.0);
        assert_eq!(m.line_height(), 22.0);
    }

    /// `size` equal to the em keeps the arithmetic in font units, so the
    /// expected numbers below are the advances themselves.
    fn paragraph(text: &str, width: f64, align: Align) -> Paragraph {
        let data = plain();
        let font = crate::opentype::Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        Paragraph::layout(&shaper, text, 1000.0, width, align, Features::ALL)
    }

    #[test]
    fn greedy_breaking_fills_lines() {
        // Advances: A 700, B 800, C 800, space 800.
        let p = paragraph("AB CA BC", 4000.0, Align::Left);
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.lines[0].range, 0..6);
        assert_eq!(p.lines[1].range, 6..8);
        // Trailing whitespace is excluded from the measured width.
        assert_eq!(p.lines[0].width, 3800.0);
        assert_eq!(p.lines[1].width, 1600.0);
        assert_eq!(p.height, 2.0 * p.metrics.line_height());
        // A word wider than the line overflows rather than being chopped.
        let p = paragraph("AAAA B", 100.0, Align::Left);
        assert_eq!(p.lines.len(), 2);
        assert!(p.lines[0].width > 100.0);
    }

    #[test]
    fn alignment_positions_the_lines() {
        for (align, expected) in [
            (Align::Left, 0.0),
            (Align::Right, 4000.0 - 3800.0),
            (Align::Center, (4000.0 - 3800.0) / 2.0),
        ] {
            let p = paragraph("AB CA BC", 4000.0, align);
            assert_eq!(p.lines[0].x, expected, "{align:?}");
            assert_eq!(p.lines[0].glyphs[0].x, expected, "{align:?}");
        }
    }

    #[test]
    fn justification_stretches_interior_spaces_only() {
        let p = paragraph("AB CA BC", 4000.0, Align::Justify);
        let line = &p.lines[0];
        assert_eq!(line.width, 4000.0);
        // One interior space absorbs all 200 units of slack.
        assert_eq!(line.glyphs[2].advance, 1000.0);
        assert_eq!(line.glyphs[3].x, 2500.0); // the 'C', pushed right by 200
        // The trailing space is past the measured width and is not stretched.
        assert_eq!(line.glyphs[5].advance, 800.0);
        // The last line stays flush left, which is the whole point.
        assert_eq!(p.lines[1].x, 0.0);
        assert_eq!(p.lines[1].width, 1600.0);
    }

    #[test]
    fn a_line_ending_at_a_newline_is_not_justified() {
        let p = paragraph("AB\nCA BC", 4000.0, Align::Justify);
        assert_eq!(p.lines.len(), 2);
        assert_eq!(p.lines[0].width, 1500.0); // untouched, not stretched to 4000
    }

    #[test]
    fn the_caret_walks_grapheme_boundaries() {
        let text = "AB CA";
        let p = paragraph(text, 10_000.0, Align::Left);
        assert_eq!(p.cursor(text, 0).0, 0.0);
        assert_eq!(p.cursor(text, 1).0, 700.0);
        assert_eq!(p.cursor(text, 2).0, 1500.0);
        assert_eq!(p.cursor(text, 5).0, 3800.0);
        assert_eq!(p.cursor(text, 99).0, 3800.0); // clamped past the end
        assert_eq!(p.cursor(text, 0).1, p.metrics.ascent);
    }

    #[test]
    fn the_caret_splits_a_ligature_by_cluster() {
        // Glyphs 1,1,2 become the single ligature glyph 21, which covers three
        // characters. A caret between them must land inside the ligature, not
        // jump to one end of it: an editor that snaps to the edges makes the
        // second character of "fi" unreachable.
        let data = font_with(&[(b"GSUB", gsub_liga(false))]);
        let font = crate::opentype::Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        let text = "AAB";
        let p = Paragraph::layout(&shaper, text, 1000.0, 9000.0, Align::Left, Features::ALL);
        assert_eq!(p.lines[0].glyphs.len(), 1);
        let advance = p.lines[0].glyphs[0].advance;
        assert_eq!(p.cursor(text, 0).0, 0.0);
        assert_eq!(p.cursor(text, 1).0, advance / 3.0);
        assert_eq!(p.cursor(text, 2).0, advance * 2.0 / 3.0);
        assert_eq!(p.cursor(text, 3).0, advance);
    }

    #[test]
    fn a_caret_is_never_placed_inside_a_cluster() {
        let text = "e\u{301}A B\u{301}C";
        let p = paragraph(text, 10_000.0, Align::Left);
        for byte in 0..=text.len() {
            let here = p.cursor(text, byte);
            let snapped = p.cursor(text, snap_to_grapheme(text, byte));
            assert_eq!(here, snapped, "byte {byte}");
        }
    }

    /// Arbitrary text through every entry point: the invariants hold or the
    /// editor built on this corrupts a document.
    #[test]
    fn arbitrary_text_keeps_the_invariants() {
        let data = plain();
        let font = crate::opentype::Font::parse(&data).unwrap();
        let shaper = Shaper::new(&font);
        let alphabet: Vec<char> = "AB C\r\n\t-,.0$%\u{301}\u{00A0}\u{200B}\u{200D}\u{1F1FA}\u{1F468}\u{1100}\u{1161}\u{2014}("
            .chars()
            .collect();
        check(
            "arbitrary text layout",
            3_000,
            |r: &mut Rng| {
                (0..r.below(24))
                    .map(|_| alphabet[r.below(alphabet.len() as u64) as usize])
                    .collect::<String>()
            },
            |text| {
                let bounds = grapheme_boundaries(text);
                assert!(bounds.windows(2).all(|w| w[0] < w[1]));
                assert!(bounds.iter().all(|&b| text.is_char_boundary(b)));
                assert_eq!(bounds[0], 0);
                assert_eq!(*bounds.last().unwrap(), text.len());

                let breaks = line_breaks(text);
                assert!(breaks.windows(2).all(|w| w[0].offset < w[1].offset));
                assert!(breaks.iter().all(|b| text.is_char_boundary(b.offset)));
                assert_eq!(breaks.last().unwrap().offset, text.len());

                // Lines tile the text exactly, in order, with no gaps.
                let p =
                    Paragraph::layout(&shaper, text, 1000.0, 3000.0, Align::Justify, Features::ALL);
                let mut at = 0;
                for line in &p.lines {
                    assert_eq!(line.range.start, at);
                    at = line.range.end;
                    assert!(line.width.is_finite() && line.width >= 0.0);
                }
                assert_eq!(at, text.len());
                for byte in 0..=text.len() {
                    let (x, y) = p.cursor(text, byte);
                    assert!(x.is_finite() && y.is_finite());
                }
                true
            },
        );
    }
}
