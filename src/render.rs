// render.rs — GDI software rendering into an UpdateLayeredWindow surface.
//
// Architecture brief (from design doc):
//   • A 32-bit DIB is the sole drawing surface — no Direct2D, no D3D context.
//   • Text is drawn with GDI TextOut (white on black background) and then
//     the alpha channel is derived from luminance (R channel of white text).
//     This gives per-pixel alpha from grayscale antialiasing with zero GPU cost.
//   • UpdateLayeredWindow (ULW_ALPHA) hands the composited bitmap to DWM,
//     which is already running. Marginal cost per lyric change ≈ 70 KB upload.
//   • A back buffer is pre-rendered after each present, so at the deadline
//     the only critical-path work is the UpdateLayeredWindow call itself.

#![allow(dead_code)]

use windows::Win32::{
    Foundation::{HWND, POINT, SIZE, COLORREF},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};

use crate::art::{ArtBuffer, ART_SIZE};

// ── Layout constants ──────────────────────────────────────────────────────────

pub const WIDGET_H:     i32 = 48;      // Taskbar height (physical px)
pub const TEXT_PAD_X:   i32 = 8;       // Gap between art and text
pub const TEXT_PAD_Y:   i32 = 2;       // Vertical padding inside text area
pub const TEXT_AREA_W:  i32 = 480;     // Max text columns
pub const FONT_HEIGHT:  i32 = -14;     // lfHeight (negative = character height, not cell)
pub const SHADOW_OFF:   i32 = 1;       // Drop-shadow pixel offset

// ── GdiBitmap ─────────────────────────────────────────────────────────────────

/// A GDI-compatible DIB section usable with UpdateLayeredWindow.
pub struct GdiBitmap {
    pub hdc:    HDC,
    pub hbm:    HBITMAP,
    pub pixels: *mut u32,   // Raw BGRA pixel pointer from CreateDIBSection
    pub width:  i32,
    pub height: i32,
}

// SAFETY: We only access `pixels` from the UI thread.
unsafe impl Send for GdiBitmap {}

impl GdiBitmap {
    pub fn new(width: i32, height: i32) -> windows::core::Result<Self> {
        unsafe {
            let hdc_screen = GetDC(HWND(std::ptr::null_mut()));
            let hdc = CreateCompatibleDC(hdc_screen);
            ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize:        std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth:       width,
                    biHeight:      -height,  // Top-down DIB
                    biPlanes:      1,
                    biBitCount:    32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut pixels: *mut u32 = std::ptr::null_mut();
            let hbm = CreateDIBSection(
                hdc,
                &bmi,
                DIB_RGB_COLORS,
                &mut pixels as *mut *mut u32 as *mut *mut _,
                None,
                0,
            )?;

            SelectObject(hdc, hbm);

            Ok(GdiBitmap { hdc, hbm, pixels, width, height })
        }
    }

    /// Fill all pixels to fully transparent (alpha = 0).
    pub fn clear(&self) {
        let n = (self.width * self.height) as usize;
        unsafe { std::ptr::write_bytes(self.pixels, 0, n); }
    }

    /// Set a premultiplied BGRA pixel (B, G, R, A).
    #[allow(dead_code)]
    pub fn put_pixel(&self, x: i32, y: i32, b: u8, g: u8, r: u8, a: u8) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height { return; }
        let idx = (y * self.width + x) as usize;
        unsafe {
            *self.pixels.add(idx) = ((a as u32) << 24)
                | ((r as u32) << 16)
                | ((g as u32) << 8)
                | b as u32;
        }
    }
}

impl Drop for GdiBitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.hdc);
            let _ = DeleteObject(self.hbm);
        }
    }
}

// ── Font cache ────────────────────────────────────────────────────────────────

pub struct FontCache {
    pub normal_font:  HFONT,
    pub shrunk_font:  HFONT,  // For long lines
}

impl FontCache {
    pub fn create() -> windows::core::Result<Self> {
        let normal_font = create_font(FONT_HEIGHT)?;
        let shrunk_font = create_font((FONT_HEIGHT as f32 * 0.85) as i32)?;
        Ok(FontCache { normal_font, shrunk_font })
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.normal_font);
            let _ = DeleteObject(self.shrunk_font);
        }
    }
}

fn create_font(height: i32) -> windows::core::Result<HFONT> {
    let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
    let mut lf_face_name = [0u16; 32];
    let copy_len = face.len().min(31);
    lf_face_name[..copy_len].copy_from_slice(&face[..copy_len]);

    let lf = LOGFONTW {
        lfHeight:         height,
        lfWeight:         FW_REGULAR.0 as i32,
        lfQuality:        FONT_QUALITY(ANTIALIASED_QUALITY.0 as u8),  // Grayscale AA, no ClearType
        lfCharSet:        FONT_CHARSET(DEFAULT_CHARSET.0 as u8),
        lfFaceName:       lf_face_name,
        ..Default::default()
    };
    let hf = unsafe { CreateFontIndirectW(&lf) };
    if hf.is_invalid() { return Err(windows::core::Error::from_win32()); }
    Ok(hf)
}

// ── Rendering ─────────────────────────────────────────────────────────────────

/// Full widget width = album art + padding + text area.
pub fn widget_width() -> i32 {
    ART_SIZE as i32 + TEXT_PAD_X + TEXT_AREA_W
}

/// Render one complete frame into `bmp`.
/// Art is blit from `art_buf` (pre-decoded, corner-rounded).
/// `lyric` is the text string to display.
pub fn render_frame(
    bmp:      &GdiBitmap,
    art_buf:  Option<&ArtBuffer>,
    lyric:    &str,
    fonts:    &FontCache,
) {
    bmp.clear();

    // ── Album art ─────────────────────────────────────────────────────────────
    let art = match art_buf {
        Some(a) => a,
        None    => &ArtBuffer::placeholder(),
    };
    blit_art(bmp, art);

    // ── Lyric text ────────────────────────────────────────────────────────────
    if !lyric.is_empty() {
        render_text(bmp, lyric, fonts);
    }
}

/// Blit the ArtBuffer (premultiplied BGRA) into the left part of the DIB.
fn blit_art(bmp: &GdiBitmap, art: &ArtBuffer) {
    let w = ART_SIZE as i32;
    let h = ART_SIZE as i32;
    let src = art.pixels.as_slice();

    for y in 0..h {
        for x in 0..w {
            let src_idx = (y * w + x) as usize;
            let pix = &src[src_idx * 4 .. src_idx * 4 + 4];
            let (b, g, r, a) = (pix[0], pix[1], pix[2], pix[3]);
            let dst_idx = (y * bmp.width + x) as usize;
            unsafe {
                *bmp.pixels.add(dst_idx) =
                    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32;
            }
        }
    }
}

/// Render lyric text with a 1 px drop shadow using GDI into a temporary
/// black-background strip, then derive alpha from luminance and blit into `bmp`.
fn render_text(bmp: &GdiBitmap, text: &str, fonts: &FontCache) {
    let text_x = ART_SIZE as i32 + TEXT_PAD_X;
    let text_w  = TEXT_AREA_W;
    let text_h  = WIDGET_H;

    // Create a temporary black DIB for GDI text rendering
    let tmp = match GdiBitmap::new(text_w, text_h) {
        Ok(b)  => b,
        Err(_) => return,
    };
    tmp.clear(); // All black (R=G=B=0, A=0)

    // Choose font based on text width
    let text_w16: Vec<u16> = text.encode_utf16().collect();
    let font = choose_font(&tmp.hdc, &text_w16, text_w - SHADOW_OFF, fonts);

    unsafe {
        SelectObject(tmp.hdc, font);
        SetBkMode(tmp.hdc, TRANSPARENT);

        // Draw shadow (dark grey so it shows through the alpha derivation)
        SetTextColor(tmp.hdc, COLORREF(0x00404040));
        let _ = TextOutW(
            tmp.hdc,
            SHADOW_OFF,
            (text_h - (-FONT_HEIGHT)) / 2 + SHADOW_OFF,
            &text_w16,
        );

        // Draw primary text (white → luminance = alpha after derivation)
        SetTextColor(tmp.hdc, COLORREF(0x00FFFFFF));
        let _ = TextOutW(
            tmp.hdc,
            0,
            (text_h - (-FONT_HEIGHT)) / 2,
            &text_w16,
        );
    }

    // Derive alpha from R channel (white text: R = luminance = desired alpha)
    // Then blit into the main bitmap at offset (text_x, 0).
    let n_pixels = (text_w * text_h) as usize;
    for i in 0..n_pixels {
        let raw = unsafe { *tmp.pixels.add(i) };
        let r = (raw >> 16) as u8;
        let g = (raw >> 8)  as u8;
        let b = raw         as u8;

        // Alpha from max channel (handles both grey shadow and white text)
        let alpha = r.max(g).max(b);
        if alpha == 0 { continue; } // Transparent pixel — skip

        // Premultiply
        let pr = (r as u32 * alpha as u32 / 255) as u8;
        let pg = (g as u32 * alpha as u32 / 255) as u8;
        let pb = (b as u32 * alpha as u32 / 255) as u8;

        let src_x  = i as i32 % text_w;
        let src_y  = i as i32 / text_w;
        let dst_x  = text_x + src_x;
        let dst_y  = src_y;
        if dst_x >= bmp.width || dst_y >= bmp.height { continue; }
        let dst_idx = (dst_y * bmp.width + dst_x) as usize;
        unsafe {
            *bmp.pixels.add(dst_idx) =
                ((alpha as u32) << 24) | ((pr as u32) << 16) | ((pg as u32) << 8) | pb as u32;
        }
    }
}

fn choose_font(hdc: &HDC, text_w16: &[u16], max_w: i32, fonts: &FontCache) -> HFONT {
    unsafe {
        SelectObject(*hdc, fonts.normal_font);
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(*hdc, text_w16, &mut sz);
        if sz.cx <= max_w { fonts.normal_font } else { fonts.shrunk_font }
    }
}

// ── Present via UpdateLayeredWindow ───────────────────────────────────────────

pub fn present(hwnd: HWND, bmp: &GdiBitmap, pos: POINT) {
    unsafe {
        let hdc_screen = GetDC(HWND(std::ptr::null_mut()));

        let size    = SIZE { cx: bmp.width, cy: bmp.height };
        let src_pt  = POINT { x: 0, y: 0 };
        let blend   = BLENDFUNCTION {
            BlendOp:             AC_SRC_OVER as u8,
            BlendFlags:          0,
            SourceConstantAlpha: 255,
            AlphaFormat:         AC_SRC_ALPHA as u8,
        };

        UpdateLayeredWindow(
            hwnd,
            hdc_screen,
            Some(&pos),
            Some(&size),
            bmp.hdc,
            Some(&src_pt),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
        .ok();

        ReleaseDC(HWND(std::ptr::null_mut()), hdc_screen);
    }
}
