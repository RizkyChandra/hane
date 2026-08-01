//! Per-pixel comparison of two RGBA buffers, and the diff image that explains
//! the number.
//!
//! Lives in the library rather than in `tests/golden.rs` because two harnesses
//! now need the same verdict: the golden corpus, which compares this crate
//! against its committed PNGs, and the GPU harness (#20), which compares the
//! GPU renderer against those same PNGs. Two comparators would drift, and the
//! whole value of D-002 is that both sides are judged by one rule.
//!
//! Buffers are premultiplied RGBA, row-major, four bytes per pixel -- the
//! [`Pixmap`](crate::Pixmap) contract. Nothing here un-premultiplies: that is
//! lossy at low alpha and would let a genuine difference hide in the rounding.

/// The largest and the mean absolute per-channel difference between two RGBA
/// buffers of the same length.
///
/// The pair is deliberate. `max` catches one badly wrong pixel, which a mean
/// over 16 KB dilutes to nothing; `mean` catches a small error smeared over
/// every pixel, which `max` alone waves through. A renderer has to pass both.
///
/// Panics if the buffers differ in length -- that is a size mismatch, not a
/// pixel difference, and the caller has to report it as such.
pub fn stats(a: &[u8], b: &[u8]) -> (u8, f64) {
    assert_eq!(a.len(), b.len(), "diff of buffers with different lengths");
    let mut max = 0;
    let mut sum = 0u64;
    for (x, y) in a.iter().zip(b) {
        let d = x.abs_diff(*y);
        max = max.max(d);
        sum += u64::from(d);
    }
    let mean = if a.is_empty() {
        0.0
    } else {
        sum as f64 / a.len() as f64
    };
    (max, mean)
}

/// Renders the difference between `reference` and `actual` as an opaque image.
///
/// Matching pixels keep a dimmed grey of the reference, so the shape stays
/// recognisable and a reviewer can see *where* on the shape the difference is.
/// Differing pixels go yellow for a hair and red as the difference grows -- the
/// 8x gain means a one-byte rounding difference is still visible rather than
/// being a black pixel nobody notices.
///
/// A mean of 0.4 tells you a renderer is wrong. Only the picture tells you it
/// is wrong along the left edge of every tile.
pub fn image(reference: &[u8], actual: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(reference.len());
    for (g, a) in reference.chunks_exact(4).zip(actual.chunks_exact(4)) {
        let d = (0..4).map(|i| g[i].abs_diff(a[i])).max().unwrap_or(0);
        if d == 0 {
            // Rec. 601 luma of the reference, compressed into the dark half.
            let luma = (0.299 * f64::from(g[0]) + 0.587 * f64::from(g[1]) + 0.114 * f64::from(g[2]))
                as u32;
            let grey = (luma / 4 + 24) as u8;
            out.extend_from_slice(&[grey, grey, grey, 255]);
        } else {
            let amp = u8::try_from(u32::from(d) * 8).unwrap_or(255);
            out.extend_from_slice(&[255, 255 - amp, 0, 255]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_buffers_are_zero() {
        let a = [12u8, 34, 56, 255, 0, 0, 0, 0];
        assert_eq!(stats(&a, &a), (0, 0.0));
    }

    /// One channel out by 4 over eight channels: max is the 4, mean is 0.5.
    #[test]
    fn max_and_mean_measure_different_things() {
        let a = [0u8; 8];
        let mut b = a;
        b[3] = 4;
        assert_eq!(stats(&a, &b), (4, 0.5));
    }

    #[test]
    fn empty_buffers_do_not_divide_by_zero() {
        assert_eq!(stats(&[], &[]), (0, 0.0));
    }

    /// Matching pixels go grey and opaque; differing ones go red-to-yellow with
    /// the red channel pinned, which is what makes a diff image scannable.
    #[test]
    fn diff_image_marks_only_the_differing_pixel() {
        let a = [255u8, 255, 255, 255, 255, 255, 255, 255];
        let b = [255u8, 255, 255, 255, 255, 255, 250, 255];
        let img = image(&a, &b);
        assert_eq!(img.len(), a.len());
        assert_eq!(img[0], img[1], "a matching pixel is grey");
        assert_eq!(img[3], 255, "the diff image is opaque");
        assert_eq!(&img[4..8], &[255, 255 - 40, 0, 255], "5 * 8 gain");
    }

    /// The gain saturates rather than wrapping, so a large difference stays red
    /// instead of coming back round to yellow.
    #[test]
    fn diff_image_gain_saturates() {
        let img = image(&[0, 0, 0, 255], &[200, 0, 0, 255]);
        assert_eq!(&img[..4], &[255, 0, 0, 255]);
    }
}
