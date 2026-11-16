// art.rs — Album art decode via WIC, scaled to ART_SIZE × ART_SIZE.
//
// WIC is a system-DLL codec stack. We use it here to:
//   1. Decode any JPEG/PNG/WEBP the browser provides.
//   2. Scale to ART_SIZE × ART_SIZE using WIC's high-quality scaler.
//   3. Convert to BGRA (32-bit, pre-multiplied alpha) for direct DIB upload.
//   4. Apply a subtle rounded-corner mask (pre-computed once).
//
// The source stream is freed immediately after decode — no full-res copy lingers.

use windows::Win32::{
    Graphics::Imaging::*,
    System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
};

pub const ART_SIZE: u32 = 48;

pub fn fnv1a(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf29ce484222325, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
}

/// 48 × 48 BGRA buffer with pre-multiplied alpha.
#[derive(Clone)]
pub struct ArtBuffer {
    /// Row-major BGRA pixels, ART_SIZE * ART_SIZE * 4 bytes.
    pub pixels: Vec<u8>,
}

impl ArtBuffer {
    /// Decode raw image bytes (JPEG/PNG/etc.), center-crop 16:9 thumbnails, and produce a scaled ArtBuffer.
    pub fn from_bytes(raw: &[u8]) -> windows::core::Result<Self> {
        unsafe {
            // ── 1. Create WIC factory ─────────────────────────────────────
            let factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

            // ── 2. Create WIC Stream from memory directly ─────────────────
            let stream = factory.CreateStream()?;
            stream.InitializeFromMemory(raw)?;

            // ── 3. Decode ─────────────────────────────────────────────────
            let decoder = factory.CreateDecoderFromStream(
                &stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )?;
            let frame = decoder.GetFrame(0)?;

            let (mut orig_w, mut orig_h) = (0u32, 0u32);
            frame.GetSize(&mut orig_w, &mut orig_h)?;
            if orig_w == 0 || orig_h == 0 {
                return Err(windows::core::Error::from_win32());
            }

            // ── 4. Scale preserving aspect ratio (short side -> 48) ───────
            let scale = ART_SIZE as f32 / (orig_w.min(orig_h) as f32);
            let scaled_w = ((orig_w as f32 * scale).round() as u32).max(ART_SIZE);
            let scaled_h = ((orig_h as f32 * scale).round() as u32).max(ART_SIZE);

            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(
                &frame,
                scaled_w,
                scaled_h,
                WICBitmapInterpolationModeHighQualityCubic,
            )?;

            // ── 5. Center-crop to 48x48 (eliminates 16:9 pillar/letterbox) 
            let crop_x = (scaled_w - ART_SIZE) / 2;
            let crop_y = (scaled_h - ART_SIZE) / 2;

            let clipper = factory.CreateBitmapClipper()?;
            let rect = WICRect {
                X: crop_x as i32,
                Y: crop_y as i32,
                Width: ART_SIZE as i32,
                Height: ART_SIZE as i32,
            };
            clipper.Initialize(&scaler, &rect)?;

            // ── 6. Convert to BGRA pre-multiplied ─────────────────────────
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &clipper,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;

            // ── 7. Copy pixels ────────────────────────────────────────────
            let stride = ART_SIZE * 4;
            let buf_size = stride * ART_SIZE;
            let mut pixels = vec![0u8; buf_size as usize];
            converter.CopyPixels(std::ptr::null(), stride, &mut pixels)?;

            // ── 8. Apply rounded-corner mask ──────────────────────────────
            apply_corner_mask(&mut pixels, ART_SIZE, 6);

            Ok(ArtBuffer { pixels })
        }
    }

    pub fn hash(&self) -> u64 {
        fnv1a(&self.pixels)
    }

    /// Return a neutral music glyph placeholder buffer (never browser icons).
    pub fn placeholder() -> Self {
        let size = (ART_SIZE * ART_SIZE * 4) as usize;
        let mut pixels = vec![0u8; size];
        let a = 200u8;
        let rgb = (40u32 * a as u32 / 255) as u8;
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = rgb;
            chunk[1] = rgb;
            chunk[2] = rgb;
            chunk[3] = a;
        }

        // Draw clean minimalist double musical note icon
        let draw_rect = |px: &mut [u8], x0: i32, y0: i32, w: i32, h: i32| {
            for cy in y0..(y0 + h) {
                for cx in x0..(x0 + w) {
                    if cx >= 0 && cx < ART_SIZE as i32 && cy >= 0 && cy < ART_SIZE as i32 {
                        let idx = ((cy * ART_SIZE as i32 + cx) * 4) as usize;
                        let alpha = 240u8;
                        let val = (220u32 * alpha as u32 / 255) as u8;
                        px[idx]     = val;
                        px[idx + 1] = val;
                        px[idx + 2] = val;
                        px[idx + 3] = alpha;
                    }
                }
            }
        };

        draw_rect(&mut pixels, 28, 16, 2, 14); // Right stem
        draw_rect(&mut pixels, 20, 20, 2, 10); // Left stem
        draw_rect(&mut pixels, 20, 16, 10, 3); // Slanted beam
        draw_rect(&mut pixels, 17, 28, 5, 4);  // Left head
        draw_rect(&mut pixels, 25, 27, 5, 4);  // Right head

        apply_corner_mask(&mut pixels, ART_SIZE, 6);
        ArtBuffer { pixels }
    }
}

// ── Corner-rounding mask ──────────────────────────────────────────────────────

/// Pre-multiply alpha to 0 for pixels outside a rounded rectangle.
/// `radius` is in pixels.
fn apply_corner_mask(pixels: &mut [u8], size: u32, radius: u32) {
    let n = size as i32;
    let r = radius as i32;
    for y in 0..n {
        for x in 0..n {
            if !inside_rounded_rect(x, y, n, r) {
                let idx = ((y * n + x) * 4) as usize;
                pixels[idx]     = 0;
                pixels[idx + 1] = 0;
                pixels[idx + 2] = 0;
                pixels[idx + 3] = 0;
            }
        }
    }
}

#[inline]
fn inside_rounded_rect(x: i32, y: i32, size: i32, radius: i32) -> bool {
    let (cx, cy) = if x < radius && y < radius {
        (radius, radius)
    } else if x >= size - radius && y < radius {
        (size - 1 - radius, radius)
    } else if x < radius && y >= size - radius {
        (radius, size - 1 - radius)
    } else if x >= size - radius && y >= size - radius {
        (size - 1 - radius, size - 1 - radius)
    } else {
        return true; // Not in a corner zone
    };
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy <= radius * radius
}
