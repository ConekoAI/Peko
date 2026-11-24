//! Pictures sized for the model (ADR-123 D1): cropped to the asked-for
//! area, scaled so the long edge is at most [`MAX_EDGE`] pixels and the
//! whole at most [`MAX_PIXELS`], and encoded as JPEG.

use anyhow::Result;

/// The longest edge a picture is sent at.
pub const MAX_EDGE: u32 = 1_568;

/// The most pixels a picture is sent with (1.2 megapixels).
pub const MAX_PIXELS: u64 = 1_200_000;

/// JPEG quality.
const QUALITY: u8 = 80;

/// Pixels the platform captured: RGBA, 8 bits a channel, rows top down.
#[derive(Clone)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

impl Image {
    /// A plain image of one colour (tests and placeholders).
    #[must_use]
    pub fn filled(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let mut data = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..width * height {
            data.extend_from_slice(&rgba);
        }
        Self {
            width,
            height,
            rgba: data,
        }
    }

    /// The part of the image at `(x, y, w, h)`, clamped to the image.
    #[must_use]
    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Self {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        let w = w.clamp(1, self.width - x);
        let h = h.clamp(1, self.height - y);
        if (x, y, w, h) == (0, 0, self.width, self.height) {
            return self.clone();
        }
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        let stride = self.width as usize * 4;
        for row in y..y + h {
            let start = row as usize * stride + x as usize * 4;
            out.extend_from_slice(&self.rgba[start..start + w as usize * 4]);
        }
        Self {
            width: w,
            height: h,
            rgba: out,
        }
    }
}

/// The size a `width × height` picture is sent at.
#[must_use]
pub fn fitted(width: u32, height: u32) -> (u32, u32) {
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let mut scale = 1.0_f64;
    let edge = w.max(h);
    if edge > f64::from(MAX_EDGE) {
        scale = f64::from(MAX_EDGE) / edge;
    }
    #[allow(clippy::cast_precision_loss)]
    let pixels = (w * scale) * (h * scale);
    #[allow(clippy::cast_precision_loss)]
    if pixels > MAX_PIXELS as f64 {
        scale *= (MAX_PIXELS as f64 / pixels).sqrt();
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let fit = |v: f64| ((v * scale).floor() as u32).max(1);
    (fit(w), fit(h))
}

/// Scale `image` to `width × height` by averaging the source pixels each
/// target pixel covers (a box filter: sharp enough for text, no ringing).
#[must_use]
pub fn resize(image: &Image, width: u32, height: u32) -> Image {
    if (width, height) == (image.width, image.height) {
        return image.clone();
    }
    let (sw, sh) = (image.width as usize, image.height as usize);
    let (tw, th) = (width as usize, height as usize);
    let mut out = vec![0u8; tw * th * 4];
    for ty in 0..th {
        let y0 = ty * sh / th;
        let y1 = ((ty + 1) * sh / th).max(y0 + 1).min(sh);
        for tx in 0..tw {
            let x0 = tx * sw / tw;
            let x1 = ((tx + 1) * sw / tw).max(x0 + 1).min(sw);
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                let row = sy * sw * 4;
                for sx in x0..x1 {
                    let p = row + sx * 4;
                    for (c, s) in sum.iter_mut().enumerate() {
                        *s += u32::from(image.rgba[p + c]);
                    }
                }
            }
            #[allow(clippy::cast_possible_truncation)]
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (ty * tw + tx) * 4;
            for c in 0..4 {
                #[allow(clippy::cast_possible_truncation)]
                {
                    out[o + c] = ((sum[c] + n / 2) / n) as u8;
                }
            }
        }
    }
    Image {
        width,
        height,
        rgba: out,
    }
}

/// `image` as JPEG, at the size the model gets.
pub fn encode(image: &Image) -> Result<Vec<u8>> {
    let w = u16::try_from(image.width).map_err(|_| anyhow::anyhow!("picture too wide"))?;
    let h = u16::try_from(image.height).map_err(|_| anyhow::anyhow!("picture too tall"))?;
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, QUALITY);
    encoder
        .encode(&image.rgba, w, h, jpeg_encoder::ColorType::Rgba)
        .map_err(|e| anyhow::anyhow!("could not encode the picture: {e}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_fit_the_model() {
        assert_eq!(fitted(800, 600), (800, 600));
        let (w, h) = fitted(5120, 2880);
        assert!(w <= MAX_EDGE && h <= MAX_EDGE);
        assert!(u64::from(w) * u64::from(h) <= MAX_PIXELS);
        // A tall narrow window keeps its shape.
        let (w, h) = fitted(400, 4000);
        assert_eq!(h, MAX_EDGE);
        assert!(w < 200);
    }

    #[test]
    fn resize_averages_and_encode_writes_a_jpeg() {
        let image = Image::filled(100, 50, [10, 20, 30, 255]);
        let small = resize(&image, 10, 5);
        assert_eq!((small.width, small.height), (10, 5));
        assert_eq!(&small.rgba[..4], &[10, 20, 30, 255]);
        let jpeg = encode(&small).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    }

    #[test]
    fn crop_is_clamped() {
        let image = Image::filled(10, 10, [1, 2, 3, 4]);
        let part = image.crop(8, 8, 5, 5);
        assert_eq!((part.width, part.height), (2, 2));
    }
}
