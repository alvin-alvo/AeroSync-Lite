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
#![allow(dead_code)]

mod art;
mod lyrics;
mod render;
mod smtc;
mod state;
mod taskbar;
mod theme;

use windows::{
    core::{w, Result},
    Win32::{
        Foundation::{HWND, WPARAM, LPARAM, LRESULT, HINSTANCE, POINT, WAIT_OBJECT_0},
        System::{
            Com::{CoInitializeEx, COINIT_APARTMENTTHREADED},
            Threading::{
                CreateWaitableTimerExW, SetWaitableTimerEx, CancelWaitableTimer,
                TIMER_ALL_ACCESS, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE,
                GetCurrentProcess, SetProcessInformation, ProcessPowerThrottling,
                PROCESS_POWER_THROTTLING_STATE, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            },
        },
        UI::{
            WindowsAndMessaging::*,
            HiDpi::SetProcessDpiAwarenessContext,
            Accessibility::{
                SetWinEventHook, HWINEVENTHOOK,
            },
        },
    },
};

use render::{GdiBitmap, FontCache, widget_width, WIDGET_H, present};
use state::{
    AppState, TrackData, LyricsPayload, ArtResult,
    WM_SMTC_TRACK, WM_SMTC_PLAYBACK, WM_SMTC_TIMELINE,
    WM_LYRICS_READY, WM_SHELL_CHANGED, WM_ART_READY,
    TIMER_ART, TIMER_THEME, ART_BACKOFF_MS,
};
use smtc::SmtcTimelinePayload;
use taskbar::{OverlayWindow, TaskbarGeometry};
use art::ArtBuffer;
use theme::Palette;

// ── Global state (UI thread only) ─────────────────────────────────────────────

static mut G_HWND:  HWND           = HWND(std::ptr::null_mut());
static mut G_STATE: *mut AppState  = std::ptr::null_mut();
static mut G_FONTS: *mut FontCache = std::ptr::null_mut();
static mut G_GEO:   *mut TaskbarGeometry = std::ptr::null_mut();
static mut G_BUF:   *mut Option<GdiBitmap> = std::ptr::null_mut();
static mut G_TIMER: windows::Win32::Foundation::HANDLE =
    windows::Win32::Foundation::HANDLE(std::ptr::null_mut());
// Registered message ID for TaskbarCreated
static mut G_TASKBAR_CREATED_MSG: u32 = 0;
static mut G_SHELLHOOK_MSG: u32 = 0;
static mut G_WINEVENT_HOOK: HWINEVENTHOOK = HWINEVENTHOOK(std::ptr::null_mut());

unsafe extern "system" fn winevent_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event == EVENT_SYSTEM_FOREGROUND && !G_HWND.0.is_null() {
        // Re-assert topmost when foreground changes (zero polling required)
        SetWindowPos(
            G_HWND,
            HWND_TOPMOST,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        ).ok();
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    // 1. Per-monitor DPI v2 (also set in manifest; belt + suspenders)
    unsafe {
        SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2
        ).ok();
    }

    // 2. Opt out of Windows 11 timer-resolution throttling for background tasks
    let power_throttling = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0, // 0 = do NOT ignore -> honor our timer resolution
    };
    unsafe {
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &power_throttling as *const _ as *const _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }

    // 3. COM / WinRT on the main (STA) thread
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?; }

    // 4. Allocate owned state on the heap (we pass raw pointers to the WNDPROC)
    let mut state    = Box::new(AppState::new());
    let mut fonts    = Box::new(FontCache::create()?);
    let mut geo      = Box::new(TaskbarGeometry::default());
    let mut buf_slot = Box::new(Option::<GdiBitmap>::None);

    unsafe {
        G_STATE = Box::as_mut(&mut state)   as *mut _;
        G_FONTS = Box::as_mut(&mut fonts)   as *mut _;
        G_GEO   = Box::as_mut(&mut geo)     as *mut _;
        G_BUF   = Box::as_mut(&mut buf_slot) as *mut _;
    }

    // 5. Create high-resolution waitable timer
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            None,
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )?
    };
    unsafe { G_TIMER = timer; }

    // 6. Measure taskbar and create the overlay window
    let hinstance: HINSTANCE = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)?.into() };
    *geo = taskbar::measure(true /* right anchor */);

    let overlay = OverlayWindow::create(hinstance, &geo, Some(wnd_proc))?;
    unsafe { G_HWND = overlay.hwnd; }

    // 7. Register TaskbarCreated message, ShellHook, and WinEvent hook
    unsafe {
        G_TASKBAR_CREATED_MSG = taskbar::register_taskbar_created();
        G_SHELLHOOK_MSG = RegisterWindowMessageW(windows::core::w!("SHELLHOOK"));
        let _ = RegisterShellHookWindow(overlay.hwnd);

        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(winevent_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        G_WINEVENT_HOOK = hook;
    }

    // 8. Init SMTC (blocks briefly on RequestAsync, before message loop)
    smtc::init_smtc(overlay.hwnd)?;

    // 9. Initial blank render
    refresh_render(&state, &mut fonts);
    if let Some(buf) = buf_slot.as_ref() {
        let pos = compute_window_pos(&geo);
        present(overlay.hwnd, buf, pos);
    }
    overlay.show();

    // 10. Message loop — true event-driven, zero polling
    run_message_loop(&overlay, &timer);

    Ok(())
}

// ── Message loop ──────────────────────────────────────────────────────────────

fn run_message_loop(overlay: &OverlayWindow, timer: &windows::Win32::Foundation::HANDLE) {
    let mut msg = MSG::default();
    let handles = [*timer];

    loop {
        // Sleep until waitable timer fires at lyric deadline OR a window message arrives.
        // Zero polling watchdog. Sleeps with INFINITE timeout (0% CPU while paused).
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&handles),
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
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Check if deadline timer fired or if extrapolated position passed the next line
        let state = unsafe { &*G_STATE };
        let is_due = state.is_playing()
            && state.lyrics.as_ref()
                .and_then(|l| l.next_start_ms(state.shown_idx))
                .map_or(false, |next| state.position_ms() >= next);

        if wait == WAIT_OBJECT_0 || is_due {
            on_timer_tick(overlay);
        }
    }
}

// ── Timer tick (deadline reached = next lyric line is due) ────────────────────

fn on_timer_tick(overlay: &OverlayWindow) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };
    let geo   = unsafe { &*G_GEO };

    let pos_ms = state.position_ms();

    if let Some(lyrics) = &state.lyrics {
        let new_idx = lyrics.index_at(pos_ms, state.shown_idx);
        if new_idx != state.shown_idx {
            state.shown_idx = new_idx;
            refresh_render(state, fonts);
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
    let delta_ms = if next_ms > pos_ms { (next_ms - pos_ms).max(1) } else { 1 };

    let due_100ns = -((delta_ms as i64) * 10_000); // Negative = relative
    let tolerance_ms = 10i32.min(delta_ms as i32);

    unsafe {
        SetWaitableTimerEx(
            timer,
            &due_100ns as *const i64 as *const _,
            0,          // No period (one-shot)
            None,
            None,
            None,
            tolerance_ms.max(0) as u32,
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
        WM_ART_READY => {
            let gen = wp.0 as u32;
            let payload = unsafe { Box::from_raw(lp.0 as *mut ArtResult) };
            handle_art_ready(hwnd, gen, *payload);
            LRESULT(0)
        }
        WM_TIMER => {
            let timer_id = wp.0;
            if timer_id == TIMER_ART {
                unsafe { KillTimer(hwnd, TIMER_ART).ok(); }
                handle_art_timer(hwnd);
            } else if timer_id == TIMER_THEME {
                unsafe { KillTimer(hwnd, TIMER_THEME).ok(); }
                handle_theme_change(hwnd);
            }
            LRESULT(0)
        }
        WM_WINDOWPOSCHANGING => {
            let wp_struct = unsafe { &mut *(lp.0 as *mut WINDOWPOS) };
            wp_struct.flags &= !SWP_NOZORDER;
            wp_struct.hwndInsertAfter = HWND_TOPMOST;
            LRESULT(0)
        }
        WM_ACTIVATE => {
            unsafe {
                SetWindowPos(
                    hwnd,
                    HWND_TOPMOST,
                    0, 0, 0, 0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                ).ok();
            }
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => {
            LRESULT(MA_NOACTIVATE as isize)
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
        WM_SETTINGCHANGE => {
            if lp.0 != 0 {
                let ptr = lp.0 as *const u16;
                let s = unsafe {
                    let mut len = 0;
                    while *ptr.add(len) != 0 { len += 1; }
                    std::slice::from_raw_parts(ptr, len)
                };
                if let Ok(setting) = String::from_utf16(s) {
                    if setting == "ImmersiveColorSet" {
                        unsafe { SetTimer(hwnd, TIMER_THEME, 120, None); }
                    }
                }
            }
            unsafe { PostMessageW(hwnd, WM_SHELL_CHANGED, WPARAM(0), LPARAM(0)).ok(); }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
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
                return LRESULT(0);
            }
            if msg == unsafe { G_SHELLHOOK_MSG } {
                unsafe {
                    SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        0, 0, 0, 0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    ).ok();
                }
                return LRESULT(0);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
    }
}

// ── Message handlers (all on UI thread) ───────────────────────────────────────

fn handle_track_change(data: TrackData) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };
    let hwnd  = unsafe { G_HWND };

    let key = data.key();
    if key == state.last_track_key { return; } // Duplicate event from multiple SMTC fires
    state.last_track_key = key;
    state.generation += 1;
    state.shown_idx   = -1;
    state.lyrics      = None;

    // Increment art generation & reset backoff
    state.art_state.gen = state.art_state.gen.wrapping_add(1);
    state.art_state.attempt = 0;
    let cur_gen = state.art_state.gen;
    unsafe { KillTimer(hwnd, TIMER_ART).ok(); }

    // If art bytes provided directly, decode and store if new
    if let Some(bytes) = &data.art_bytes {
        if let Ok(buf) = ArtBuffer::from_bytes(bytes) {
            let hash = buf.hash();
            if hash != state.art_state.good_hash {
                state.art_state.prev_hash = state.art_state.good_hash;
                state.art_state.good_hash = hash;
                state.art_state.good = Some(buf);
            }
        } else {
            let title = data.title.clone();
            smtc::spawn_art_retry(hwnd, cur_gen, title);
        }
    } else {
        let title = data.title.clone();
        smtc::spawn_art_retry(hwnd, cur_gen, title);
    }

    // Store duration for lyrics fetch (may arrive before timeline event)
    let generation = state.generation;
    let title      = data.title.clone();
    let artist     = data.artist.clone();
    let album      = data.album.clone();
    state.track = Some(data);
    let dur_ms  = state.track.as_ref().map_or(0, |t| t.duration_ms);

    // Debounce: use a thread that sleeps 300 ms then posts the fetch trigger
    let hwnd_raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("lyric-debounce".into())
        .stack_size(32 * 1024)
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            lyrics::fetch_lyrics_async(HWND(hwnd_raw as *mut _), generation, &title, &artist, &album, dur_ms);
        })
        .ok();

    // Render immediately with art + "Loading…" text
    refresh_render(state, fonts);
    flush_render(hwnd);
}

fn handle_art_ready(hwnd: HWND, gen: u32, res: ArtResult) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };
    if gen != state.art_state.gen {
        return; // Stale result from previous track
    }
    match res {
        ArtResult::Ok { title, hash, buffer } => {
            if state.track.as_ref().map_or(false, |t| t.title == title) {
                state.art_state.prev_hash = state.art_state.good_hash;
                state.art_state.good_hash = hash;
                state.art_state.good = Some(buffer);
                refresh_render(state, fonts);
                flush_render(hwnd);
            }
        }
        ArtResult::Missing => {
            let attempt = state.art_state.attempt as usize;
            if attempt < ART_BACKOFF_MS.len() {
                let delay = ART_BACKOFF_MS[attempt];
                state.art_state.attempt += 1;
                unsafe {
                    SetTimer(hwnd, TIMER_ART, delay, None);
                }
            }
        }
    }
}

fn handle_art_timer(hwnd: HWND) {
    let state = unsafe { &*G_STATE };
    let title = state.track.as_ref().map_or(String::new(), |t| t.title.clone());
    smtc::spawn_art_retry(hwnd, state.art_state.gen, title);
}

fn handle_theme_change(hwnd: HWND) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };
    let light = theme::system_uses_light_theme();
    if state.light_theme != light {
        state.light_theme = light;
        state.palette = Palette::for_theme(light);
        refresh_render(state, fonts);
        flush_render(hwnd);
    }
}

fn handle_playback_change(is_playing: bool, rate: f64) {
    let state = unsafe { &mut *G_STATE };
    state.is_playing = is_playing;
    state.playback_rate = rate;
    if let Some(anchor) = &mut state.anchor {
        // If transitioning to playing, re-sync last_updated to now to prevent jump
        if is_playing && !anchor.is_playing {
            anchor.position_100ns = (anchor.position_ms_now() as i64) * 10_000;
            anchor.last_updated_100ns = state::precise_filetime_now();
        }
        anchor.is_playing = is_playing;
        anchor.rate       = rate;
    }
    // Cancel or re-arm timer based on new play state
    arm_timer_for_next(state);
}

fn handle_timeline_change(payload: SmtcTimelinePayload) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };

    let SmtcTimelinePayload { mut anchor, duration_ms } = payload;

    // Carry over play state and rate
    if state.is_playing {
        anchor.is_playing = true;
        anchor.rate       = state.playback_rate;
    } else if let Some(prev) = &state.anchor {
        anchor.is_playing = prev.is_playing;
        anchor.rate       = prev.rate;
    }
    state.is_playing = anchor.is_playing;
    state.playback_rate = anchor.rate;

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

    // Update rendered lyric immediately if timeline changed
    let pos_ms = state.position_ms();
    if let Some(lyrics) = &state.lyrics {
        let new_idx = lyrics.index_at(pos_ms, -1);
        if new_idx != state.shown_idx {
            state.shown_idx = new_idx;
            refresh_render(state, fonts);
            flush_render(unsafe { G_HWND });
        }
    }

    arm_timer_for_next(state);
}

fn handle_lyrics_ready(payload: LyricsPayload) {
    let state = unsafe { &mut *G_STATE };
    let fonts = unsafe { &mut *G_FONTS };

    // Drop stale responses
    if payload.generation != state.generation { return; }

    state.lyrics    = if payload.lyrics.lines.is_empty() { None } else { Some(payload.lyrics) };
    state.shown_idx = -1;

    // Force a render immediately with the first lyric line
    let pos_ms = state.position_ms();
    if let Some(lyrics) = &state.lyrics {
        state.shown_idx = lyrics.index_at(pos_ms, -1);
    }

    refresh_render(state, fonts);
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

fn refresh_render(state: &AppState, fonts: &mut FontCache) {
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

    render::render_frame(buf, state.art_state.good.as_ref(), &lyric_text, fonts, &state.palette);
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

        let _ = SetForegroundWindow(hwnd);
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
