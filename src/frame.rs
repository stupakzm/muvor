//! Turning a captured PNG into something the detector can read (§5.4b).
//!
//! The compositor can only hand back PNG — GJS cannot carry raw pixels and
//! two of the three routes that look like they can fail *silently*, which is
//! the whole of §5.4b. So the bytes arrive encoded and are decoded here,
//! where a decoder is an ordinary dependency rather than a marshalling
//! accident.
//!
//! Luma rather than RGB because everything downstream is a gradient: §5.4's
//! measured pipeline is grayscale → Sobel → threshold, and carrying three
//! channels to throw two away costs 3× the memory for nothing.

use muvor_core::Rect;

/// One decoded frame: single-channel luma, one byte per pixel, `w` bytes per
/// row.
pub struct Luma {
    pub pixels: Vec<u8>,
    pub w: usize,
    pub h: usize,
    /// Where this image's `0,0` sits in screen coordinates — the capture's
    /// own origin, so a rectangle found in image space comes back in the
    /// space every other rectangle in muvor already uses (D13).
    pub origin: (i32, i32),
}

impl Luma {
    pub const fn bounds(&self) -> Rect {
        Rect::new(self.origin.0, self.origin.1, self.w as i32, self.h as i32)
    }
}

/// Decode a captured PNG to luma.
///
/// Rec. 601 weights, in integers: the coefficients differ between 601 and
/// 709 by a few percent and nothing downstream is sensitive to it, but
/// picking one and saying so is cheaper than wondering later.
pub fn decode(png: &[u8], origin: (i32, i32)) -> Result<Luma, String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().map_err(|e| format!("capture is not a PNG: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG decode failed: {e}"))?;
    let (w, h) = (info.width as usize, info.height as usize);
    let channels = info.color_type.samples();

    // The compositor writes 8-bit RGBA today. Anything else is not a
    // failure to hide — a 16-bit or palette frame decoded as if it were
    // 8-bit RGBA would produce a plausible, wrong image, and §5.4b is a
    // whole section about plausible wrong images.
    if info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "capture is {:?}-bit; muvor's decoder handles 8-bit only",
            info.bit_depth
        ));
    }
    if channels < 3 {
        return Err(format!(
            "capture has {channels} channel(s); muvor's decoder expects RGB or RGBA"
        ));
    }

    let mut pixels = vec![0u8; w * h];
    for (out, px) in pixels.iter_mut().zip(buf.chunks_exact(channels)) {
        let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
        *out = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
    }
    Ok(Luma { pixels, w, h, origin })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a tiny RGBA PNG so the decoder can be tested without a
    /// compositor. `png` is already a dependency; using its encoder here
    /// costs nothing and keeps this test hermetic.
    fn rgba_png(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(std::io::Cursor::new(&mut out), w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().expect("header");
            writer.write_image_data(px).expect("data");
        }
        out
    }

    #[test]
    fn decodes_to_luma_of_the_right_shape() {
        let px = vec![255u8; 4 * 4 * 4];
        let got = decode(&rgba_png(4, 4, &px), (0, 0)).expect("decodes");
        assert_eq!((got.w, got.h), (4, 4));
        assert_eq!(got.pixels.len(), 16);
        assert!(got.pixels.iter().all(|&p| p >= 254), "white should stay white");
    }

    #[test]
    fn luma_weights_green_most() {
        // One pixel of each primary at full strength. Rec. 601 puts green
        // well above red and red above blue; a decoder that averaged the
        // channels would make these equal and nothing downstream would
        // notice until a green-on-grey theme stopped producing edges.
        let px = [255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255];
        let got = decode(&rgba_png(3, 1, &px), (0, 0)).expect("decodes");
        let (r, g, b) = (got.pixels[0], got.pixels[1], got.pixels[2]);
        assert!(g > r && r > b, "expected green > red > blue, got {g} {r} {b}");
    }

    #[test]
    fn bounds_are_in_screen_space() {
        let px = vec![0u8; 2 * 2 * 4];
        let got = decode(&rgba_png(2, 2, &px), (458, 293)).expect("decodes");
        assert_eq!(got.bounds(), Rect::new(458, 293, 2, 2));
    }

    #[test]
    fn refuses_something_that_is_not_a_png() {
        assert!(decode(b"not a png at all", (0, 0)).is_err());
    }
}
