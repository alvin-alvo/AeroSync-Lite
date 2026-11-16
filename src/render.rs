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
    Foundation::{HWND, POINT, SIZE, COLORREF, RECT},
    Graphics::Gdi::*,
    UI::WindowsAndMessaging::*,
};

use crate::art::{ArtBuffer, ART_SIZE};
use crate::theme::Palette;

// ── Layout constants ──────────────────────────────────────────────────────────

pub const WIDGET_H:     i32 = 48;      // Taskbar height (physical px)
pub const TEXT_PAD_X:   i32 = 8;       // Gap between art and text
pub const TEXT_PAD_Y:   i32 = 2;       // Vertical padding inside text area
pub const TEXT_AREA_W:  i32 = 480;     // Max text columns
pub const FONT_HEIGHT:  i32 = -14;     // lfHeight (negative = character height, not cell)
pub const SHADOW_OFF:   i32 = 1;       // Drop-shadow pixel offset

// ── Multi-script classification ───────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Script {
    Latin = 0,
    Indic = 1,
    Thai = 2,
    Han = 3,
    Kana = 4,
    Hangul = 5,
}

const FACES: [&str; 6] = [
    "Segoe UI",           // Latin, Cyrillic, Greek, Arabic, Hebrew
    "Nirmala UI",         // Devanagari, Malayalam (0D00-0D7F), Tamil, Telugu, Kannada, Bengali, Gurmukhi, Sinhala
    "Leelawadee UI",      // Thai, Lao, Khmer
    "Microsoft YaHei UI", // Simplified Chinese
    "Yu Gothic UI",       // Japanese Kana & Kanji
    "Malgun Gothic",      // Korean Hangul
];

pub fn classify_script(s: &str) -> Script {
    for c in s.chars() {
        match c as u32 {
            0x0900..=0x0DFF => return Script::Indic,
            0x0E00..=0x0E7F => return Script::Thai,
            0x3040..=0x30FF => return Script::Kana,
            0xAC00..=0xD7AF => return Script::Hangul,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF => return Script::Han,
            _ => {}
        }
    }
    Script::Latin
}

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
    height: i32,
    fonts: [(Option<HFONT>, Option<HFONT>); 6],
}

impl FontCache {
    pub fn create() -> windows::core::Result<Self> {
        Ok(FontCache {
            height: FONT_HEIGHT,
            fonts: [
                (None, None), (None, None), (None, None),
                (None, None), (None, None), (None, None),
            ],
        })
    }

    pub fn get(&mut self, script: Script, shrunk: bool) -> HFONT {
        let idx = script as usize;
        let slot = if shrunk { &mut self.fonts[idx].1 } else { &mut self.fonts[idx].0 };
        *slot.get_or_insert_with(|| {
            let h = if shrunk { (self.height as f32 * 0.85) as i32 } else { self.height };
            create_font(FACES[idx], h).unwrap_or_else(|_| create_font("Segoe UI", h).unwrap())
        })
    }
}

impl Drop for FontCache {
    fn drop(&mut self) {
        for (f1, f2) in &mut self.fonts {
            if let Some(h) = f1.take() { unsafe { let _ = DeleteObject(h); } }
            if let Some(h) = f2.take() { unsafe { let _ = DeleteObject(h); } }
        }
    }
}

fn create_font(face_name: &str, height: i32) -> windows::core::Result<HFONT> {
    let face: Vec<u16> = face_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut lf_face_name = [0u16; 32];
    let copy_len = face.len().min(31);
    lf_face_name[..copy_len].copy_from_slice(&face[..copy_len]);

    let lf = LOGFONTW {
        lfHeight:         height,
        lfWeight:         FW_SEMIBOLD.0 as i32,
        lfQuality:        FONT_QUALITY(ANTIALIASED_QUALITY.0 as u8),  // Grayscale AA, not ClearType
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
    fonts:    &mut FontCache,
    palette:  &Palette,
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
        render_text(bmp, lyric, fonts, palette);
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

/// Render lyric text with contrast halo and per-script font shaping.
fn render_text(bmp: &GdiBitmap, text: &str, fonts: &mut FontCache, palette: &Palette) {
    let text_x = ART_SIZE as i32 + TEXT_PAD_X;
    let text_w  = TEXT_AREA_W;
    let text_h  = WIDGET_H;

    // Create a temporary black DIB for GDI text rendering
    let tmp = match GdiBitmap::new(text_w, text_h) {
        Ok(b)  => b,
        Err(_) => return,
    };
    tmp.clear(); // All black (R=G=B=0, A=0)

    let script = classify_script(text);
    let mut text_w16: Vec<u16> = text.encode_utf16().collect();

    // Measure text with normal font first
    let normal_font = fonts.get(script, false);
    let mut sz = SIZE::default();
    unsafe {
        SelectObject(tmp.hdc, normal_font);
        let _ = GetTextExtentPoint32W(tmp.hdc, &text_w16, &mut sz);
    }

    let is_shrunk = sz.cx > (text_w - SHADOW_OFF);
    let font = if is_shrunk { fonts.get(script, true) } else { normal_font };

    unsafe {
        SelectObject(tmp.hdc, font);
        SetBkMode(tmp.hdc, TRANSPARENT);
        SetTextColor(tmp.hdc, COLORREF(0x00FFFFFF)); // White glyphs = coverage mask

        let mut rc = RECT {
            left: 0,
            top: 0,
            right: text_w - SHADOW_OFF,
            bottom: text_h,
        };
        DrawTextW(
            tmp.hdc,
            &mut text_w16,
            &mut rc,
            DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS | DT_VCENTER,
        );
    }

    // Compose using coverage mask + contrast halo
    for y in 0..text_h {
        for x in 0..text_w {
            let i = (y * text_w + x) as usize;
            let raw = unsafe { *tmp.pixels.add(i) };
            let c = (raw & 0xFF) as u32; // Luminance

            // 1-px down-right halo
            let hc = if x > 0 && y > 0 {
                let prev_raw = unsafe { *tmp.pixels.add(i - text_w as usize - 1) };
                (prev_raw & 0xFF) as u32 * palette.halo_a as u32 / 255
            } else {
                0
            };
            let hc = hc * (255 - c) / 255;
            let a = c + hc;
            if a == 0 { continue; }

            let ch = |f: u8, o: u8| (f as u32 * c + o as u32 * hc) / 255;
            let pr = ch(palette.fg[0], palette.halo[0]);
            let pg = ch(palette.fg[1], palette.halo[1]);
            let pb = ch(palette.fg[2], palette.halo[2]);

            let dst_x = text_x + x;
            let dst_y = y;
            if dst_x >= bmp.width || dst_y >= bmp.height { continue; }
            let dst_idx = (dst_y * bmp.width + dst_x) as usize;
            unsafe {
                *bmp.pixels.add(dst_idx) =
                    (a << 24) | (pr << 16) | (pg << 8) | pb;
            }
        }
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
