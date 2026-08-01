//! A synthetic font for tests, so shaping and layout do not depend on which
//! fonts happen to be installed.
//!
//! Glyph 1 is `A`, 2 is `B`, 3 is `C`, 4 is `x`, 5 is `y` and 6 is the space.
//! Advances are 600 for glyph 0, 700 for glyph 1 and 800 for everything else,
//! which are distinct enough that a wrong glyph shows up as a wrong width.

/// Append a big-endian `u16`.
pub fn be16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Append a big-endian `u32`.
pub fn be32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// `head`, `hhea`, `maxp`, `hmtx`, `cmap`, `OS/2` and `name`.
pub fn base_tables() -> Vec<(&'static [u8; 4], Vec<u8>)> {
    vec![
        (b"OS/2", os2()),
        (b"cmap", cmap()),
        (b"head", head()),
        (b"hhea", hhea()),
        (b"hmtx", hmtx()),
        (b"maxp", maxp()),
    ]
}

/// The whole synthetic font, with no shaping tables.
pub fn plain() -> Vec<u8> {
    assemble(&base_tables())
}

/// Assemble a table directory plus the tables into an sfnt file.
pub fn assemble(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    be32(&mut out, 0x0001_0000);
    be16(&mut out, tables.len() as u16);
    for _ in 0..3 {
        be16(&mut out, 0); // searchRange, entrySelector, rangeShift
    }
    let mut offset = 12 + tables.len() as u32 * 16;
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

fn head() -> Vec<u8> {
    let mut t = Vec::new();
    be32(&mut t, 0x0001_0000);
    be32(&mut t, 0);
    be32(&mut t, 0);
    be32(&mut t, 0x5F0F_3CF5);
    be16(&mut t, 0); // flags
    be16(&mut t, 1000); // unitsPerEm
    t.extend_from_slice(&[0; 16]); // created, modified
    be16(&mut t, (-100i16) as u16);
    be16(&mut t, (-200i16) as u16);
    be16(&mut t, 1000);
    be16(&mut t, 1800);
    be16(&mut t, 0); // macStyle
    be16(&mut t, 8);
    be16(&mut t, 2);
    be16(&mut t, 0); // indexToLocFormat
    be16(&mut t, 0);
    t
}

fn hhea() -> Vec<u8> {
    let mut t = Vec::new();
    be32(&mut t, 0x0001_0000);
    be16(&mut t, 800); // ascender
    be16(&mut t, (-200i16) as u16); // descender
    be16(&mut t, 100); // lineGap
    t.extend_from_slice(&[0; 24]);
    be16(&mut t, 3); // numberOfHMetrics
    t
}

fn maxp() -> Vec<u8> {
    let mut t = Vec::new();
    be32(&mut t, 0x0001_0000);
    be16(&mut t, 64); // numGlyphs, room for the synthetic ligature glyphs
    t.extend_from_slice(&[0; 26]);
    t
}

fn os2() -> Vec<u8> {
    let mut t = vec![0u8; 96];
    t[0..2].copy_from_slice(&4u16.to_be_bytes());
    t[4..6].copy_from_slice(&400u16.to_be_bytes());
    t[6..8].copy_from_slice(&5u16.to_be_bytes());
    t[62..64].copy_from_slice(&0x40u16.to_be_bytes()); // fsSelection: regular
    t[68..70].copy_from_slice(&750u16.to_be_bytes()); // sTypoAscender
    t[70..72].copy_from_slice(&((-250i16) as u16).to_be_bytes());
    t[72..74].copy_from_slice(&200u16.to_be_bytes()); // sTypoLineGap
    t[74..76].copy_from_slice(&900u16.to_be_bytes()); // usWinAscent
    t[76..78].copy_from_slice(&300u16.to_be_bytes()); // usWinDescent
    t[86..88].copy_from_slice(&500u16.to_be_bytes());
    t[88..90].copy_from_slice(&700u16.to_be_bytes());
    t
}

/// Three paired metrics, then bare bearings: glyphs past the third share 800.
fn hmtx() -> Vec<u8> {
    let mut t = Vec::new();
    for (advance, bearing) in [(600u16, 40i16), (700, 50), (800, 60)] {
        be16(&mut t, advance);
        be16(&mut t, bearing as u16);
    }
    for _ in 3..64 {
        be16(&mut t, 70);
    }
    t
}

/// A format 4 subtable mapping `A`..=`C` to 1..=3, `x`..=`y` to 4..=5 and the
/// space to 6.
fn cmap() -> Vec<u8> {
    let segs: [(u16, u16, u16); 4] = [
        (0x20, 0x20, 6u16.wrapping_sub(0x20)),
        (0x41, 0x43, 1u16.wrapping_sub(0x41)),
        (0x78, 0x79, 4u16.wrapping_sub(0x78)),
        (0xFFFF, 0xFFFF, 1),
    ];
    let seg_count = segs.len() as u16;
    let mut sub = Vec::new();
    be16(&mut sub, 4);
    be16(&mut sub, 16 + seg_count * 8);
    be16(&mut sub, 0);
    be16(&mut sub, seg_count * 2);
    be16(&mut sub, 4);
    be16(&mut sub, 1);
    be16(&mut sub, 0);
    for (_, end, _) in segs {
        be16(&mut sub, end);
    }
    be16(&mut sub, 0); // reservedPad
    for (start, _, _) in segs {
        be16(&mut sub, start);
    }
    for (_, _, delta) in segs {
        be16(&mut sub, delta);
    }
    for _ in segs {
        be16(&mut sub, 0); // idRangeOffset: arithmetic mapping throughout
    }

    let mut t = Vec::new();
    be16(&mut t, 0);
    be16(&mut t, 1);
    be16(&mut t, 3);
    be16(&mut t, 1);
    be32(&mut t, 12);
    t.extend_from_slice(&sub);
    t
}

/// A GSUB/GPOS table with one script, one feature and one lookup.
pub fn layout_table(kind: u16, subtables: &[Vec<u8>], tag: &[u8; 4]) -> Vec<u8> {
    let mut lookup = Vec::new();
    be16(&mut lookup, kind);
    be16(&mut lookup, 0); // lookupFlag
    be16(&mut lookup, subtables.len() as u16);
    let mut offset = 6 + subtables.len() as u16 * 2;
    for sub in subtables {
        be16(&mut lookup, offset);
        offset += sub.len() as u16;
    }
    for sub in subtables {
        lookup.extend_from_slice(sub);
    }

    let mut lookup_list = Vec::new();
    be16(&mut lookup_list, 1);
    be16(&mut lookup_list, 4);
    lookup_list.extend_from_slice(&lookup);

    let mut feature_list = Vec::new();
    be16(&mut feature_list, 1);
    feature_list.extend_from_slice(tag);
    be16(&mut feature_list, 8);
    be16(&mut feature_list, 0); // featureParams
    be16(&mut feature_list, 1); // lookupIndexCount
    be16(&mut feature_list, 0);

    let mut script_list = Vec::new();
    be16(&mut script_list, 1);
    script_list.extend_from_slice(b"latn");
    be16(&mut script_list, 8);
    be16(&mut script_list, 4); // defaultLangSys offset
    be16(&mut script_list, 0); // langSysCount
    be16(&mut script_list, 0); // lookupOrder
    be16(&mut script_list, 0xFFFF); // requiredFeatureIndex
    be16(&mut script_list, 1); // featureIndexCount
    be16(&mut script_list, 0);

    let mut t = Vec::new();
    be16(&mut t, 1);
    be16(&mut t, 0);
    let script_off = 10u16;
    let feature_off = script_off + script_list.len() as u16;
    be16(&mut t, script_off);
    be16(&mut t, feature_off);
    be16(&mut t, feature_off + feature_list.len() as u16);
    t.extend_from_slice(&script_list);
    t.extend_from_slice(&feature_list);
    t.extend_from_slice(&lookup_list);
    t
}

/// GSUB with one ligature lookup: glyphs 1,1 -> 20 and 1,1,2 -> 21.
///
/// The three-part form comes first so file order, not length, decides.
pub fn gsub_liga(extension: bool) -> Vec<u8> {
    let mut lig_ffi = Vec::new();
    be16(&mut lig_ffi, 21);
    be16(&mut lig_ffi, 3);
    be16(&mut lig_ffi, 1);
    be16(&mut lig_ffi, 2);

    let mut lig_ff = Vec::new();
    be16(&mut lig_ff, 20);
    be16(&mut lig_ff, 2);
    be16(&mut lig_ff, 1);

    let mut set = Vec::new();
    be16(&mut set, 2);
    be16(&mut set, 6); // ffi first
    be16(&mut set, 6 + lig_ffi.len() as u16);
    set.extend_from_slice(&lig_ffi);
    set.extend_from_slice(&lig_ff);

    // Coverage format 1 over glyph 1.
    let mut coverage = Vec::new();
    be16(&mut coverage, 1);
    be16(&mut coverage, 1);
    be16(&mut coverage, 1);

    let mut sub = Vec::new();
    be16(&mut sub, 1); // substFormat
    be16(&mut sub, 8); // coverageOffset
    be16(&mut sub, 1); // ligatureSetCount
    be16(&mut sub, 8 + coverage.len() as u16);
    sub.extend_from_slice(&coverage);
    sub.extend_from_slice(&set);

    if !extension {
        return layout_table(4, &[sub], b"liga");
    }
    // Two extension subtables, the real one second: a parser that resolves
    // only the first leaves this one wrapped and forms no ligature, which is
    // exactly how a large font hides its kerning from a broken unwrapper.
    let wrap = |body: &[u8]| {
        let mut wrapper = Vec::new();
        be16(&mut wrapper, 1);
        be16(&mut wrapper, 4);
        be32(&mut wrapper, 8); // past this 8-byte extension header
        wrapper.extend_from_slice(body);
        wrapper
    };
    // A LigatureSubst covering nothing, so it never matches.
    let mut decoy = Vec::new();
    be16(&mut decoy, 1);
    be16(&mut decoy, 6);
    be16(&mut decoy, 0);
    be16(&mut decoy, 1);
    be16(&mut decoy, 0);
    layout_table(7, &[wrap(&decoy), wrap(&sub)], b"liga")
}

/// The shared synthetic font plus whatever shaping tables a test needs.
pub fn font_with(extra: &[(&'static [u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut tables = base_tables();
    tables.extend(extra.iter().map(|(t, d)| (*t, d.clone())));
    tables.sort_by_key(|(tag, _)| *tag);
    assemble(&tables)
}

/// Every font file installed on this machine, for the local smoke tests.
///
/// Empty on a machine with no font directory, which is what makes those tests
/// skip themselves on CI rather than fail.
pub fn installed_fonts() -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("ttf" | "otf")
            ) {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(std::path::Path::new("/usr/share/fonts"), &mut out);
    out.sort();
    out
}
