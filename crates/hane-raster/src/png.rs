//! A hand-rolled PNG encoder and decoder for golden images (D-001).
//!
//! # Why by hand
//!
//! D-001 forbids `png` and `flate2`, so the golden-image harness has to carry
//! its own codec. That is cheaper than it sounds, because deflate has a *stored*
//! block type that compresses nothing: a zlib stream is then a two-byte header,
//! a length-prefixed copy of the input, and an Adler-32. What still has to be
//! exactly right is the framing -- chunk CRCs, the zlib header check bits, the
//! Adler-32 -- because a viewer rejects the file outright if any of them is
//! wrong, and the bytes do not tell you which one you got wrong.
//!
//! ponytail: stored blocks make a 64x64 golden 16 KB instead of about 400
//! bytes. If the corpus grows past a few hundred fixtures, add fixed-Huffman
//! blocks (deflate block type 1, a static code table) before reaching for a
//! real deflate -- roughly 80 lines, and still no dependency.
//!
//! # Premultiplied bytes go out verbatim
//!
//! [`Pixmap`](crate::Pixmap) is premultiplied and PNG is not, and
//! un-premultiplying is lossy at low alpha -- a golden has to round-trip
//! bit-exactly, so the bytes are written as they are. On an opaque pixel the
//! two forms are identical, which is why most fixtures draw onto an opaque
//! background: what a viewer shows is then exactly what the rasterizer
//! produced. A fixture that keeps transparency still round-trips; it just looks
//! darker in a viewer than it composites.
//!
//! # The decoder reads what this encoder writes, and no more
//!
//! 8-bit RGBA, no interlace, filter 0 on every row, stored deflate blocks.
//! Anything else returns `None` rather than being half-supported: goldens are
//! written by this file, so a golden this decoder rejects is a golden something
//! else rewrote, and that should fail loudly rather than quietly.

/// The eight bytes every PNG starts with.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The largest payload a single stored deflate block can hold.
const STORED_MAX: usize = 0xffff;

/// Encodes `width` by `height` RGBA bytes as an 8-bit truecolour-with-alpha PNG.
///
/// `data` is row-major from the top left, four bytes per pixel, and is written
/// unchanged -- see the module documentation on premultiplied alpha. Panics if
/// `data` is not exactly `width * height * 4` bytes, which is a caller bug
/// rather than a runtime condition.
pub fn encode(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
    let stride = width as usize * 4;
    assert_eq!(
        data.len(),
        stride * height as usize,
        "pixel buffer does not match {width}x{height}"
    );

    // Filter byte 0 (None) ahead of every row. Filtering exists to help the
    // compressor, and this one does not compress.
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for y in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&data[y * stride..(y + 1) * stride]);
    }

    let mut out = Vec::from(SIGNATURE);
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    // Bit depth 8, colour type 6 (RGBA), deflate, adaptive filtering, no interlace.
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Decodes a PNG written by [`encode`] into `(width, height, rgba)`.
///
/// Returns `None` for anything outside that subset, for a chunk whose CRC does
/// not match, and for a truncated file.
pub fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut rest = bytes.strip_prefix(&SIGNATURE)?;
    let (mut width, mut height) = (0u32, 0u32);
    let mut idat = Vec::new();
    let mut seen_ihdr = false;

    while !rest.is_empty() {
        let len = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let data = rest.get(8..8 + len)?;
        let crc = u32::from_be_bytes(rest.get(8 + len..12 + len)?.try_into().ok()?);
        // The CRC covers the type and the data but not the length field. A
        // golden that fails this is corrupt on disk, which is worth telling
        // apart from a golden that merely disagrees with the render.
        if crc != crc32(&[&kind[..], data]) {
            return None;
        }
        rest = &rest[12 + len..];

        match &kind {
            b"IHDR" => {
                width = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
                height = u32::from_be_bytes(data.get(4..8)?.try_into().ok()?);
                if data.get(8..13)? != [8, 6, 0, 0, 0] {
                    return None;
                }
                seen_ihdr = true;
            }
            // IDAT may be split across any number of chunks; the zlib stream is
            // their concatenation, and a split can fall mid-block.
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
    }
    if !seen_ihdr {
        return None;
    }

    let raw = inflate_stored(&idat)?;
    let stride = width as usize * 4;
    if raw.len() != (stride + 1) * height as usize {
        return None;
    }

    let mut out = Vec::with_capacity(stride * height as usize);
    for row in raw.chunks_exact(stride + 1) {
        if row[0] != 0 {
            return None; // a filter this decoder does not undo
        }
        out.extend_from_slice(&row[1..]);
    }
    Some((width, height, out))
}

/// Appends one PNG chunk: big-endian length, four-byte type, payload, CRC.
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// Wraps `data` in a zlib stream of stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    // 0x78: deflate, 32 KB window. 0x01: no preset dictionary, fastest level.
    // The pair is also the header check -- 0x7801 is divisible by 31, and a
    // decoder is required to reject the stream when it is not.
    let mut out = vec![0x78, 0x01];
    // `chunks` yields nothing for an empty input, but the stream still needs
    // one final, empty block or it is truncated rather than empty.
    let mut blocks = data.chunks(STORED_MAX);
    let mut cur: &[u8] = blocks.next().unwrap_or(&[]);
    loop {
        let next = blocks.next();
        out.push(u8::from(next.is_none())); // BFINAL, then BTYPE 00 = stored
        let len = cur.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes()); // NLEN, the ones' complement check
        out.extend_from_slice(cur);
        match next {
            Some(n) => cur = n,
            None => break,
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Undoes [`zlib_stored`]. Returns `None` for any non-stored block.
fn inflate_stored(z: &[u8]) -> Option<Vec<u8>> {
    let (&cmf, rest) = z.split_first()?;
    let (&flg, mut rest) = rest.split_first()?;
    // FDICT (bit 5 of FLG) would put a dictionary id in front of the data, so
    // refuse it rather than misread the next four bytes as a block header.
    if cmf != 0x78 || flg & 0x20 != 0 || (u16::from(cmf) << 8 | u16::from(flg)) % 31 != 0 {
        return None;
    }

    let mut out = Vec::new();
    loop {
        let (&hdr, tail) = rest.split_first()?;
        if hdr & 0x06 != 0 {
            return None; // a Huffman-coded block: not written by this encoder
        }
        let len = u16::from_le_bytes(tail.get(..2)?.try_into().ok()?) as usize;
        let nlen = u16::from_le_bytes(tail.get(2..4)?.try_into().ok()?);
        if nlen != !(len as u16) {
            return None;
        }
        out.extend_from_slice(tail.get(4..4 + len)?);
        rest = &tail[4 + len..];
        if hdr & 1 != 0 {
            break; // BFINAL
        }
    }

    if u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) != adler32(&out) {
        return None;
    }
    Some(out)
}

/// CRC-32 of the concatenation of `parts`, as PNG defines it.
///
/// Computed a bit at a time rather than from a 256-entry table: a golden is a
/// few tens of kilobytes and this runs once per file, so the table would buy
/// nothing but a `static` to get wrong.
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for part in parts {
        for &b in *part {
            crc ^= u32::from(b);
            for _ in 0..8 {
                // The reflected polynomial 0x04c11db7. The mask is 0 or !0
                // depending on the low bit, which keeps this branchless and,
                // more to the point, keeps it one line.
                crc = (crc >> 1) ^ (0xedb8_8320 & (!(crc & 1)).wrapping_add(1));
            }
        }
    }
    !crc
}

/// Adler-32 of `data`, the zlib stream check value.
fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 is the most bytes that can be summed before `b` could overflow u32.
    for block in data.chunks(5552) {
        for &byte in block {
            a += u32::from(byte);
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Color, Pixmap};
    use hane_geom::{PathEl, Point};

    fn sample(w: u32, h: u32) -> Pixmap {
        let mut pm = Pixmap::new(w, h);
        pm.fill_path(
            &[
                PathEl::MoveTo(Point::new(0.5, 0.5)),
                PathEl::LineTo(Point::new(f64::from(w) - 0.75, 1.0)),
                PathEl::LineTo(Point::new(3.0, f64::from(h) - 0.25)),
                PathEl::ClosePath,
            ],
            Color {
                r: 200,
                g: 40,
                b: 90,
                a: 255,
            },
        );
        pm
    }

    fn roundtrip(pm: &Pixmap) {
        let png = encode(pm.width(), pm.height(), pm.data());
        let (w, h, data) = decode(&png).expect("decodes");
        assert_eq!((w, h), (pm.width(), pm.height()));
        assert_eq!(data, pm.data());
    }

    #[test]
    fn round_trips() {
        roundtrip(&sample(9, 5));
    }

    /// More bytes than one stored block holds, so the multi-block path and its
    /// final-block flag are exercised.
    #[test]
    fn round_trips_across_stored_blocks() {
        let pm = sample(200, 200);
        assert!(
            encode(pm.width(), pm.height(), pm.data()).len() > STORED_MAX,
            "should span several stored blocks"
        );
        roundtrip(&pm);
    }

    #[test]
    fn round_trips_when_empty() {
        roundtrip(&Pixmap::new(0, 0));
    }

    #[test]
    fn header_and_chunk_order() {
        let pm = sample(9, 5);
        let png = encode(pm.width(), pm.height(), pm.data());
        assert_eq!(png[..8], SIGNATURE);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 9);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 5);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Known-good values from the CRC-32, Adler-32 and PNG specifications. If
    /// either checksum drifts, every file this module writes is rejected by
    /// every viewer, and no other test in the workspace would notice.
    #[test]
    fn checksums_match_the_specs() {
        assert_eq!(crc32(&[b"123456789"]), 0xcbf4_3926); // the standard CRC-32 check value
        assert_eq!(crc32(&[b"IEND"]), 0xae42_6082); // constant in every PNG ever written
        assert_eq!(crc32(&[b"1234", b"56789"]), crc32(&[b"123456789"]));
        assert_eq!(adler32(b"abc"), 0x024d_0127);
        assert_eq!(adler32(b""), 1);
    }

    #[test]
    fn rejects_corruption() {
        let pm = sample(9, 5);
        let png = encode(pm.width(), pm.height(), pm.data());
        assert!(decode(&png[..png.len() - 4]).is_none(), "truncated");
        assert!(decode(&png[1..]).is_none(), "no signature");
        let mut flipped = png.clone();
        let last = flipped.len() - 10; // a pixel byte, inside IDAT
        flipped[last] ^= 0xff;
        assert!(decode(&flipped).is_none(), "flipped payload byte");
    }
}
