// taskbar.rs — Taskbar docking and free-space measurement.
//
// Strategy (from design doc):
//   Primary:  SetParent(our_hwnd, Shell_TrayWnd) — WS_CHILD + layered.
//             Inherits z-order, auto-hide, and taskbar movement.
//   Fallback: WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE floating
//             overlay, re-raised on WinEvent hooks.
//
// Free-space measurement:
//   One-shot UI Automation query on startup and on shell-change events.
//   Never on a timer.

use windows::Win32::{
    Foundation::{HWND, RECT},
    UI::WindowsAndMessaging::*,
    UI::Shell::*,
    Graphics::Gdi::*,
    UI::HiDpi::*,
};

use crate::render::{WIDGET_H, widget_width};

// ── Docking mode ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DockMode {
    /// Our window is a child of Shell_TrayWnd (preferred).
    Child,
    /// Floating TOPMOST window positioned over the taskbar rect.
    Floating,
}

// ── TaskbarGeometry ───────────────────────────────────────────────────────────

/// Cached taskbar geometry.  Re-measured on shell events, never on a timer.
#[derive(Clone, Debug)]
pub struct TaskbarGeometry {
    pub taskbar_rect:  RECT,     // Screen coordinates of Shell_TrayWnd
    pub free_rect:     RECT,     // The region not occupied by icons / system tray
    pub dpi:           u32,
}

impl Default for TaskbarGeometry {
    fn default() -> Self {
        TaskbarGeometry {
            taskbar_rect: RECT::default(),
            free_rect:    RECT::default(),
            dpi: 96,
        }
    }
}

// ── Taskbar handle helpers ────────────────────────────────────────────────────

pub fn find_taskbar() -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(windows::core::w!("Shell_TrayWnd"), None) };
    if hwnd.0.is_null() { None } else { Some(hwnd) }
}

pub fn find_tray_notify() -> Option<HWND> {
    let taskbar = find_taskbar()?;
    let hwnd = unsafe {
        FindWindowExW(taskbar, None, windows::core::w!("TrayNotifyWnd"), None)
    };
    if hwnd.0.is_null() { None } else { Some(hwnd) }
}

// ── Geometry measurement ──────────────────────────────────────────────────────

/// Probe the taskbar geometry.  Call only on shell events.
pub fn measure(config_right_anchor: bool) -> TaskbarGeometry {
    let mut geo = TaskbarGeometry::default();

    let taskbar_hwnd = match find_taskbar() {
        Some(h) => h,
        None    => return geo,
    };

    // Taskbar bounding rect
    unsafe { GetWindowRect(taskbar_hwnd, &mut geo.taskbar_rect).ok(); }

    // DPI for the monitor the taskbar lives on
    geo.dpi = unsafe { GetDpiForWindow(taskbar_hwnd) };

    // Estimate occupied regions: icon cluster (ReBarWindow32 child) + system tray
    let icon_right  = icon_cluster_right(taskbar_hwnd);
    let tray_left   = tray_left_edge(taskbar_hwnd);

    // Free zone between the icon cluster and the tray
    let tb = &geo.taskbar_rect;
    if config_right_anchor {
        // Widget anchored to the left of the system tray
        let w = widget_width();
        geo.free_rect = RECT {
            left:   (tray_left - w - 8).max(icon_right + 8),
            top:    tb.top,
            right:  tray_left - 8,
            bottom: tb.bottom,
        };
    } else {
        // Widget anchored to the right of the icon cluster
        geo.free_rect = RECT {
            left:   icon_right + 8,
            top:    tb.top,
            right:  (icon_right + 8 + widget_width()).min(tray_left - 8),
            bottom: tb.bottom,
        };
    }

    geo
}

fn icon_cluster_right(taskbar: HWND) -> i32 {
    // Find ReBarWindow32 → find its rightmost extent
    let rebar = unsafe {
        FindWindowExW(taskbar, None, windows::core::w!("ReBarWindow32"), None)
    };
    if rebar.0.is_null() { return 400; } // Conservative default

    let mut rc = RECT::default();
    unsafe { GetWindowRect(rebar, &mut rc).ok(); }
    rc.right
}

fn tray_left_edge(taskbar: HWND) -> i32 {
    let tray = unsafe {
        FindWindowExW(taskbar, None, windows::core::w!("TrayNotifyWnd"), None)
    };
    if tray.0.is_null() {
        // Fallback: 200 px from right edge of taskbar
        let mut rc = RECT::default();
        unsafe { GetWindowRect(taskbar, &mut rc).ok(); }
        return rc.right - 200;
    }
    let mut rc = RECT::default();
    unsafe { GetWindowRect(tray, &mut rc).ok(); }
    rc.left
}

// ── Window creation ───────────────────────────────────────────────────────────

pub struct OverlayWindow {
    pub hwnd:      HWND,
    pub dock_mode: DockMode,
}

impl OverlayWindow {
    /// Create the overlay window.  Attempts child-of-taskbar (Mode A).
    /// Falls back to floating TOPMOST window (Mode B) if that fails.
    pub fn create(
        hinstance: windows::Win32::Foundation::HINSTANCE,
        geo: &TaskbarGeometry,
        wnd_proc: WNDPROC,
    ) -> windows::core::Result<Self> {
        let class_name = windows::core::w!("AeroSyncLiteOverlay");

        // Register window class
        let wc = WNDCLASSEXW {
            cbSize:        std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc:   wnd_proc,
            hInstance:     hinstance.into(),
            lpszClassName: class_name,
            hCursor:       unsafe { LoadCursorW(None, IDC_ARROW)? },
            ..Default::default()
        };
        unsafe { RegisterClassExW(&wc); } // Ignore "already registered" error

        let w = widget_width();
        let h = WIDGET_H;

        // ── Try Mode A: child of Shell_TrayWnd ────────────────────────────
        if let Some(taskbar) = find_taskbar() {
            let (x, y) = widget_pos_in_taskbar(geo, w, h);
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_LAYERED | WS_EX_NOACTIVATE,
                    class_name,
                    None,
                    WS_CHILD | WS_VISIBLE,
                    x, y, w, h,
                    taskbar,
                    None,
                    hinstance.into(),
                    None,
                )
            };
            if !hwnd.0.is_null() {
                return Ok(OverlayWindow { hwnd, dock_mode: DockMode::Child });
            }
        }

        // ── Fallback Mode B: floating TOPMOST ─────────────────────────────
        let tb = &geo.taskbar_rect;
        let (x, y) = (geo.free_rect.left, tb.top + (tb.bottom - tb.top - h) / 2);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class_name,
                None,
                WS_POPUP | WS_VISIBLE,
                x, y, w, h,
                None,
                None,
                hinstance.into(),
                None,
            )
        }?;

        // Keep z-order above taskbar for floating mode
        unsafe {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            ).ok();
        }

        Ok(OverlayWindow { hwnd, dock_mode: DockMode::Floating })
    }

    /// Move the widget to the correct screen position after a geometry refresh.
    pub fn reposition(&self, geo: &TaskbarGeometry) {
        let w = widget_width();
        let h = WIDGET_H;

        match self.dock_mode {
            DockMode::Child => {
                let (x, y) = widget_pos_in_taskbar(geo, w, h);
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        HWND(std::ptr::null_mut()),
                        x, y, w, h,
                        SWP_NOACTIVATE | SWP_NOZORDER,
                    ).ok();
                }
            }
            DockMode::Floating => {
                let tb = &geo.taskbar_rect;
                let x = geo.free_rect.left;
                let y = tb.top + (tb.bottom - tb.top - h) / 2;
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        HWND_TOPMOST,
                        x, y, w, h,
                        SWP_NOACTIVATE,
                    ).ok();
                }
            }
        }
    }

    /// Re-assert TOPMOST z-order (used when the taskbar raises itself).
    pub fn raise(&self) {
        if self.dock_mode == DockMode::Floating {
            unsafe {
                SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ).ok();
            }
        }
    }

    pub fn show(&self) {
        unsafe { ShowWindow(self.hwnd, SW_SHOWNA); }
    }

    pub fn hide(&self) {
        unsafe { ShowWindow(self.hwnd, SW_HIDE); }
    }
}

fn widget_pos_in_taskbar(geo: &TaskbarGeometry, w: i32, h: i32) -> (i32, i32) {
    let tb  = &geo.taskbar_rect;
    let fr  = &geo.free_rect;
    let tb_h = tb.bottom - tb.top;

    // X: start of free zone, relative to taskbar for child mode
    let x = fr.left - tb.left;
    // Y: vertically centred in taskbar
    let y = (tb_h - h) / 2;
    (x, y)
}

// ── Shell registration for TaskbarCreated / WM_SETTINGCHANGE ─────────────────

static mut TASKBAR_CREATED_MSG: u32 = 0;

pub fn register_taskbar_created() -> u32 {
    unsafe {
        TASKBAR_CREATED_MSG = RegisterWindowMessageW(windows::core::w!("TaskbarCreated"));
        TASKBAR_CREATED_MSG
    }
}

pub fn taskbar_created_msg() -> u32 {
    unsafe { TASKBAR_CREATED_MSG }
}
