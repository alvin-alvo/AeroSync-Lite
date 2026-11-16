// theme.rs — Windows Light/Dark theme detection and contrast palette.

use windows::{core::w, Win32::System::Registry::*};

/// Query whether the Windows shell/taskbar is currently using Light Theme.
/// Reads SystemUsesLightTheme (not AppsUseLightTheme) because the taskbar
/// follows the system theme.
pub fn system_uses_light_theme() -> bool {
    let mut v: u32 = 0;
    let mut sz = 4u32;
    unsafe {
        let _ = RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut _ as *mut _),
            Some(&mut sz),
        );
    }
    v != 0
}

/// Dynamic color palette ensuring high contrast against any taskbar background.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub fg: [u8; 3],       // RGB foreground color
    pub halo: [u8; 3],     // RGB halo/shadow color
    pub halo_a: u8,        // Alpha intensity for halo
}

impl Palette {
    pub fn for_theme(light: bool) -> Self {
        if light {
            // Light taskbar: dark charcoal glyphs with soft light halo
            Self {
                fg: [0x1A, 0x1A, 0x1A],
                halo: [0xFF, 0xFF, 0xFF],
                halo_a: 90,
            }
        } else {
            // Dark taskbar: clean white glyphs with subtle dark halo
            Self {
                fg: [0xFF, 0xFF, 0xFF],
                halo: [0x00, 0x00, 0x00],
                halo_a: 110,
            }
        }
    }
}
