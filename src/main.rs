// main.rs — AeroSync-Lite entry point and message loop.
//
// Threading model (from design doc):
//   • ONE UI thread owns all state.  No locks needed.
//   • WinRT callbacks / WinHTTP completions arrive on thread-pool threads.
//     Each does the minimum (snapshot → Box → PostMessage) and returns.
//   • The UI thread runs MsgWaitForMultipleObjectsEx([hi_res_timer], ...)
//     so it sleeps with ZERO active timers while paused, consuming 0 CPU.
//   • The hi-res timer is armed to fire exactly at the timestamp of the NEXT
//     lyric line.  On wake, the only critical-path work is UpdateLayeredWindow.

#![windows_subsystem = "windows"]  // No console window

mod art;
mod lyrics;
mod render;
mod smtc;
mod state;
mod taskbar;

use windows::{
    core::{w, Result},
    Win32::{
        Foundation::{HWND, WPARAM, LPARAM, LRESULT, HINSTANCE},
        System::{
            Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            Threading::{
                CreateWaitableTimerExW, SetWaitableTimerEx, CancelWaitableTimer,
                TIMER_ALL_ACCESS, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                MsgWaitForMultipleObjectsEx, MWMO_ALERTABLE, MWMO_INPUTAVAILABLE,
                QS_ALLINPUT, WAIT_OBJECT_0, WAIT_TIMEOUT, WAIT_FAILED,
            },
            SystemInformation::GetSystemTimePreciseAsFileTime,
        },
        UI::{
            WindowsAndMessaging::*,
            HiDpi::SetProcessDpiAwarenessContext,
        },
        Graphics::Gdi::POINT,
    },
};

use render::{GdiBitmap, FontCache, widget_width, WIDGET_H, present};
use state::{
    AppState, TimeAnchor, TrackData, LyricsPayload,
    WM_SMTC_TRACK, WM_SMTC_PLAYBACK, WM_SMTC_TIMELINE,
    WM_LYRICS_READY, WM_SHELL_CHANGED,
};
use smtc::SmtcTimelinePayload;
use taskbar::{OverlayWindow, TaskbarGeometry, DockMode};
use art::ArtBuffer;

// ── Global state (UI thread only) ─────────────────────────────────────────────

static mut G_HWND:  HWND           = HWND(std::ptr::null_mut());
static mut G_STATE: *mut AppState  = std::ptr::null_mut();
static mut G_FONTS: *mut FontCache = std::ptr::null_mut();
static mut G_GEO:   *mut TaskbarGeometry = std::ptr::null_mut();
static mut G_ART:   *mut Option<ArtBuffer> = std::ptr::null_mut();
static mut G_BUF:   *mut Option<GdiBitmap> = std::ptr::null_mut();
static mut G_TIMER: windows::Win32::Foundation::HANDLE =
    windows::Win32::Foundation::HANDLE(std::ptr::null_mut());
// Registered message ID for TaskbarCreated
static mut G_TASKBAR_CREATED_MSG: u32 = 0;
// Debounce: lyrics fetch only after metadata is stable (300 ms)
static mut G_META_DEBOUNCE_GEN: u64 = 0;

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    // 1. Per-monitor DPI v2 (also set in manifest; belt + suspenders)
    unsafe {
        SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
        ).ok();
    }

    // 2. COM / WinRT on the main (STA) thread
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?; }

    // 3. Allocate owned state on the heap (we pass raw pointers to the WNDPROC)
    let mut state    = Box::new(AppState::new());
    let mut fonts    = Box::new(FontCache::create()?);
    let mut geo      = Box::new(TaskbarGeometry::default());
    let mut art_slot = Box::new(Option::<ArtBuffer>::None);
    let mut buf_slot = Box::new(Option::<GdiBitmap>::None);

    unsafe {
        G_STATE = Box::as_mut(&mut state)   as *mut _;
        G_FONTS = Box::as_mut(&mut fonts)   as *mut _;
        G_GEO   = Box::as_mut(&mut geo)     as *mut _;
        G_ART   = Box::as_mut(&mut art_slot) as *mut _;
        G_BUF   = Box::as_mut(&mut buf_slot) as *mut _;
    }

    // 4. Create high-resolution waitable timer
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )?
    };
    unsafe { G_TIMER = timer; }

    // 5. Measure taskbar and create the overlay window
    let hinstance: HINSTANCE = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.into() };
    *geo = taskbar::measure(true /* right anchor */);

    let overlay = OverlayWindow::create(hinstance, &geo, Some(wnd_proc))?;
    unsafe { G_HWND = overlay.hwnd; }

    // 6. Register TaskbarCreated message so we can re-attach when Explorer restarts
    unsafe { G_TASKBAR_CREATED_MSG = taskbar::register_taskbar_created(); }

    // 7. Init SMTC (blocks briefly on RequestAsync, before message loop)
    smtc::init_smtc(overlay.hwnd)?;

    // 8. Initial blank render
    refresh_render(&state, &fonts, &art_slot);
    if let Some(buf) = buf_slot.as_ref() {
        let pos = compute_window_pos(&geo);
        present(overlay.hwnd, buf, pos);
    }
    overlay.show();

    // 9. Message loop — the heart of the system
    run_message_loop(&overlay, &timer);

    Ok(())
}

// ── Message loop ──────────────────────────────────────────────────────────────

fn run_message_loop(overlay: &OverlayWindow, timer: &windows::Win32::Foundation::HANDLE) {
    let mut msg = MSG::default();
    let handles = [*timer];

    loop {
        // Sleep until timer fires OR a message arrives — zero CPU while paused
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                &handles,
                INFINITE,
                QS_ALLINPUT,
                MWMO_ALERTABLE | MWMO_INPUTAVAILABLE,
            )
        };

        // Drain all pending messages first
        loop {
            let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() };
            if !got { break; }
            if msg.message == WM_QUIT { return; }
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // If the timer fired, advance lyrics and re-arm
        if wait == WAIT_OBJECT_0 {
            on_timer_tick(overlay);
        }
    }
}

// ── Timer tick (deadline reached = next lyric line is due) ────────────────────

fn on_timer_tick(overlay: &OverlayWindow) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &*G_FONTS };
    let art   = unsafe { &*G_ART };
    let geo   = unsafe { &*G_GEO };

    let pos_ms = state.position_ms();

    if let Some(lyrics) = &state.lyrics {
        let new_idx = lyrics.index_at(pos_ms, state.shown_idx);
        if new_idx != state.shown_idx {
            state.shown_idx = new_idx;
            refresh_render(state, fonts, art);
            let buf = unsafe { &*G_BUF };
            if let Some(b) = buf.as_ref() {
                let pos = compute_window_pos(geo);
                present(overlay.hwnd, b, pos);
            }
        }
        // Re-arm timer for the NEXT line deadline
        arm_timer_for_next(state);
    }
}

/// Arm the hi-res timer to fire when the next lyric line starts.
/// If paused or no lyrics, leave the timer cancelled (zero wakeups).
fn arm_timer_for_next(state: &AppState) {
    let timer = unsafe { G_TIMER };

    if !state.is_playing() {
        unsafe { CancelWaitableTimer(timer).ok(); }
        return;
    }

    let lyrics = match &state.lyrics {
        Some(l) => l,
        None    => { unsafe { CancelWaitableTimer(timer).ok(); } return; }
    };

    let next_ms = match lyrics.next_start_ms(state.shown_idx) {
        Some(t) => t,
        None    => { unsafe { CancelWaitableTimer(timer).ok(); } return; }
    };

    let pos_ms  = state.position_ms();
    if next_ms <= pos_ms { return; } // Already past

    let due_100ns = -(((next_ms - pos_ms) as i64) * 10_000); // Negative = relative
    let tolerance_ms = 10i32; // 10 ms tolerance — good enough for lyric sync

    unsafe {
        SetWaitableTimerEx(
            timer,
            &due_100ns as *const i64 as *const _,
            0,          // No period (one-shot)
            None,
            std::ptr::null_mut(),
            None,
            tolerance_ms as u32,
        ).ok();
    }
}

// ── WNDPROC ───────────────────────────────────────────────────────────────────

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // ── Cross-thread messages from SMTC / WinHTTP ──────────────────────
        WM_SMTC_TRACK => {
            let data = unsafe { Box::from_raw(wp.0 as *mut TrackData) };
            handle_track_change(*data);
            LRESULT(0)
        }
        WM_SMTC_PLAYBACK => {
            let is_playing = wp.0 != 0;
            let rate = f64::from_bits(lp.0 as u64);
            handle_playback_change(is_playing, rate);
            LRESULT(0)
        }
        WM_SMTC_TIMELINE => {
            let payload = unsafe { Box::from_raw(wp.0 as *mut SmtcTimelinePayload) };
            handle_timeline_change(*payload);
            LRESULT(0)
        }
        WM_LYRICS_READY => {
            let payload = unsafe { Box::from_raw(wp.0 as *mut LyricsPayload) };
            handle_lyrics_ready(*payload);
            LRESULT(0)
        }
        WM_SHELL_CHANGED => {
            handle_shell_changed(hwnd);
            LRESULT(0)
        }

        // ── Hit-test for album art click ───────────────────────────────────
        WM_LBUTTONUP => {
            let x = (lp.0 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 >> 16) & 0xFFFF) as i16 as i32;
            if x >= 0 && x <= art::ART_SIZE as i32 && y >= 0 && y <= WIDGET_H {
                smtc::toggle_play_pause(hwnd);
            }
            LRESULT(0)
        }

        // ── Right-click context menu ───────────────────────────────────────
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }

        // ── DPI changed ───────────────────────────────────────────────────
        WM_DPICHANGED => {
            handle_shell_changed(hwnd);
            LRESULT(0)
        }

        // ── Settings/display change ───────────────────────────────────────
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE => {
            unsafe { PostMessageW(hwnd, WM_SHELL_CHANGED, WPARAM(0), LPARAM(0)).ok(); }
            LRESULT(0)
        }

        WM_DESTROY => {
            unsafe { PostQuitMessage(0); }
            LRESULT(0)
        }

        _ => {
            // Handle TaskbarCreated (Explorer restart)
            if msg == unsafe { G_TASKBAR_CREATED_MSG } {
                handle_shell_changed(hwnd);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

// ── Message handlers (all on UI thread) ───────────────────────────────────────

fn handle_track_change(mut data: TrackData) {
    let state = unsafe { &mut *G_STATE };

    // Filter to browser sessions only (optional — allows all media apps if desired)
    // if !data.is_browser_session() { return; }

    let key = data.key();
    if key == state.last_track_key { return; } // Duplicate event from multiple SMTC fires
    state.last_track_key = key;
    state.generation += 1;
    state.shown_idx   = -1;
    state.lyrics      = None;

    // Update art
    let art = unsafe { &mut *G_ART };
    *art = data.art_bytes.as_deref().and_then(|b| ArtBuffer::from_bytes(b).ok());

    // Store duration for lyrics fetch (may arrive before timeline event)
    // Schedule debounced lyrics fetch (300 ms)
    let hwnd       = unsafe { G_HWND };
    let generation = state.generation;
    let title      = data.title.clone();
    let artist     = data.artist.clone();
    let album      = data.album.clone();
    let dur_ms     = state.track.as_ref().map_or(0, |t| t.duration_ms);

    // Stash track so timeline handler can fill duration_ms
    state.track = Some(data);

    // Debounce: use a thread that sleeps 300 ms then posts the fetch trigger
    std::thread::Builder::new()
        .name("lyric-debounce".into())
        .stack_size(32 * 1024)
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            // If generation still matches, kick the fetch
            lyrics::fetch_lyrics_async(hwnd, generation, &title, &artist, &album, dur_ms);
        })
        .ok();

    // Render immediately with art + "Loading…" text
    let fonts = unsafe { &*G_FONTS };
    let art   = unsafe { &*G_ART };
    refresh_render(state, fonts, art);
    flush_render(hwnd);
}

fn handle_playback_change(is_playing: bool, rate: f64) {
    let state = unsafe { &mut *G_STATE };
    if let Some(anchor) = &mut state.anchor {
        // Re-anchor position from SMTC timeline position + now
        anchor.is_playing = is_playing;
        anchor.rate       = rate;
    }
    // Cancel or re-arm timer based on new play state
    arm_timer_for_next(state);
}

fn handle_timeline_change(payload: SmtcTimelinePayload) {
    let state = unsafe { &mut *G_STATE };

    let SmtcTimelinePayload { mut anchor, duration_ms } = payload;

    // Carry over play state and rate from previous anchor if we have one
    if let Some(prev) = &state.anchor {
        anchor.is_playing = prev.is_playing;
        anchor.rate       = prev.rate;
    }

    // Ignore spurious updates within 50 ms of current extrapolated position
    if let Some(prev) = &state.anchor {
        let prev_pos = prev.position_ms_now();
        let new_pos  = anchor.position_ms_now();
        let delta    = (prev_pos as i64 - new_pos as i64).unsigned_abs() as u32;
        if delta < 50 && anchor.is_playing == prev.is_playing {
            return;
        }
    }

    state.anchor = Some(anchor);

    // If track duration wasn't set yet (track metadata arrived before timeline), fill it
    if let Some(track) = &mut state.track {
        if track.duration_ms == 0 { track.duration_ms = duration_ms; }
    }

    // After a seek, reset the shown index to force binary search
    state.shown_idx = -1;

    arm_timer_for_next(state);
}

fn handle_lyrics_ready(payload: LyricsPayload) {
    let state = unsafe { &mut *G_STATE };

    // Drop stale responses
    if payload.generation != state.generation { return; }

    state.lyrics    = if payload.lyrics.lines.is_empty() { None } else { Some(payload.lyrics) };
    state.shown_idx = -1;

    // Force a render immediately with the first lyric line
    let pos_ms = state.position_ms();
    if let Some(lyrics) = &state.lyrics {
        state.shown_idx = lyrics.index_at(pos_ms, -1);
    }

    let fonts = unsafe { &*G_FONTS };
    let art   = unsafe { &*G_ART };
    refresh_render(state, fonts, art);
    flush_render(unsafe { G_HWND });
    arm_timer_for_next(state);
}

fn handle_shell_changed(hwnd: HWND) {
    let geo = unsafe { &mut *G_GEO };
    *geo = taskbar::measure(true);
    // Re-position the window
    let new_pos = compute_window_pos(geo);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            new_pos.x, new_pos.y,
            widget_width(), WIDGET_H,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        ).ok();
    }
}

// ── Render helpers ────────────────────────────────────────────────────────────

fn refresh_render(state: &AppState, fonts: &FontCache, art: &Option<ArtBuffer>) {
    let buf_slot = unsafe { &mut *G_BUF };

    let w = widget_width();
    let h = WIDGET_H;

    // Create or reuse buffer
    let buf = match buf_slot {
        Some(b) if b.width == w && b.height == h => b,
        _ => {
            *buf_slot = GdiBitmap::new(w, h).ok();
            match buf_slot.as_mut() {
                Some(b) => b,
                None    => return,
            }
        }
    };

    let lyric_text = match &state.lyrics {
        Some(ls) => ls.line_text(state.shown_idx).to_owned(),
        None     => {
            if state.track.is_some() { "♪ Loading lyrics…".to_owned() }
            else                      { String::new() }
        }
    };

    render::render_frame(buf, art.as_ref(), &lyric_text, fonts);
}

fn flush_render(hwnd: HWND) {
    let buf = unsafe { &*G_BUF };
    let geo = unsafe { &*G_GEO };
    if let Some(b) = buf.as_ref() {
        let pos = compute_window_pos(geo);
        present(hwnd, b, pos);
    }
}

fn compute_window_pos(geo: &TaskbarGeometry) -> POINT {
    POINT {
        x: geo.free_rect.left,
        y: geo.taskbar_rect.top + (geo.taskbar_rect.bottom - geo.taskbar_rect.top - WIDGET_H) / 2,
    }
}

// ── Right-click context menu ──────────────────────────────────────────────────

const IDM_OFFSET_PLUS:  u32 = 1001;
const IDM_OFFSET_MINUS: u32 = 1002;
const IDM_QUIT:         u32 = 1003;

fn show_context_menu(hwnd: HWND) {
    unsafe {
        let hmenu = CreatePopupMenu().unwrap();
        let state = &*G_STATE;
        let offset_label = format!(
            "Sync offset: {:+} ms\0", state.offset_ms
        );
        let ol: Vec<u16> = offset_label.encode_utf16().collect();

        AppendMenuW(hmenu, MF_STRING | MF_GRAYED, 0, windows::core::PCWSTR(ol.as_ptr())).ok();
        AppendMenuW(hmenu, MF_SEPARATOR, 0, None).ok();
        AppendMenuW(hmenu, MF_STRING, IDM_OFFSET_PLUS as usize,  w!("+100 ms (later)")).ok();
        AppendMenuW(hmenu, MF_STRING, IDM_OFFSET_MINUS as usize, w!("-100 ms (earlier)")).ok();
        AppendMenuW(hmenu, MF_SEPARATOR, 0, None).ok();
        AppendMenuW(hmenu, MF_STRING, IDM_QUIT as usize, w!("Quit AeroSync-Lite")).ok();

        let mut pt = windows::Win32::Foundation::POINT::default();
        windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).ok();

        SetForegroundWindow(hwnd).ok();
        let cmd = TrackPopupMenu(
            hmenu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x, pt.y,
            0,
            hwnd,
            None,
        );
        DestroyMenu(hmenu).ok();

        match cmd.0 as u32 {
            IDM_OFFSET_PLUS  => {
                let s = &mut *G_STATE;
                s.offset_ms += 100;
                s.shown_idx = -1;
                arm_timer_for_next(s);
            }
            IDM_OFFSET_MINUS => {
                let s = &mut *G_STATE;
                s.offset_ms -= 100;
                s.shown_idx = -1;
                arm_timer_for_next(s);
            }
            IDM_QUIT => { PostQuitMessage(0); }
            _ => {}
        }
    }
}
