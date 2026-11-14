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

/// 48 × 48 BGRA buffer with pre-multiplied alpha.
pub struct ArtBuffer {
    /// Row-major BGRA pixels, ART_SIZE * ART_SIZE * 4 bytes.
    pub pixels: Vec<u8>,
}

impl ArtBuffer {
    /// Decode raw image bytes (JPEG/PNG/etc.) and produce a scaled ArtBuffer.
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

            // ── 4. Scale ──────────────────────────────────────────────────
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(
                &frame,
                ART_SIZE,
                ART_SIZE,
                WICBitmapInterpolationModeHighQualityCubic,
            )?;

            // ── 5. Convert to BGRA pre-multiplied ─────────────────────────
            let converter = factory.CreateFormatConverter()?;
            converter.Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;

            // ── 6. Copy pixels ────────────────────────────────────────────
            let stride = ART_SIZE * 4;
            let buf_size = stride * ART_SIZE;
            let mut pixels = vec![0u8; buf_size as usize];
            converter.CopyPixels(std::ptr::null(), stride, &mut pixels)?;

            // ── 7. Apply rounded-corner mask ──────────────────────────────
            apply_corner_mask(&mut pixels, ART_SIZE, 6);

            Ok(ArtBuffer { pixels })
        }
    }

    /// Return a grey placeholder buffer (used when no art is available).
    pub fn placeholder() -> Self {
        let size = (ART_SIZE * ART_SIZE * 4) as usize;
        // Mid-grey (60, 60, 60) with full alpha.
        let mut pixels = vec![0u8; size];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[0] = 60;  // B
            chunk[1] = 60;  // G
            chunk[2] = 60;  // R
            chunk[3] = 255; // A
        }
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
