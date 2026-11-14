// smtc.rs — Windows System Media Transport Controls bridge.
//
// Architecture (from design doc):
//   • RequestAsync() is called once during init (blocking on a wait event).
//   • Session change / media / playback / timeline events use typed completion
//     handlers — no executor, no .await.
//   • Each handler does the minimum: snapshot data onto the heap, then
//     PostMessage to the UI thread. The WinRT thread pool returns immediately.
//   • SMTC position is NEVER polled. We only capture the (position, lastUpdated,
//     rate) triple from TimelinePropertiesChanged and extrapolate.

use windows::{
    core::{Interface, Result, HRESULT},
    Foundation::{TypedEventHandler, IReference, IAsyncOperation},
    Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as MediaManager,
        GlobalSystemMediaTransportControlsSession     as Session,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
        GlobalSystemMediaTransportControlsSessionMediaProperties,
    },
    Storage::Streams::{DataReader, IRandomAccessStreamReference},
    Win32::{
        Foundation::{HWND, WPARAM, LPARAM, HANDLE},
        UI::WindowsAndMessaging::PostMessageW,
        System::Threading::{CreateEventW, SetEvent, WaitForSingleObject, INFINITE},
    },
};

use crate::state::{
    TrackData, TimeAnchor, WM_SMTC_TRACK, WM_SMTC_PLAYBACK, WM_SMTC_TIMELINE,
    precise_filetime_now,
};

// ── SMTC manager init ─────────────────────────────────────────────────────────

/// Initialise the SMTC session manager and register event handlers.
/// Blocks briefly (< 200 ms) waiting for `RequestAsync()` to complete,
/// but does NOT block the message loop (called before it starts).
pub fn init_smtc(hwnd: HWND) -> Result<()> {
    // Block on RequestAsync with an event (no message pump needed here)
    let event = unsafe { CreateEventW(None, true, false, None)? };
    let ev_raw = event.0 as isize;

    let op  = MediaManager::RequestAsync()?;

    op.SetCompleted(&windows::Foundation::AsyncOperationCompletedHandler::new(
        move |_, _| {
            unsafe { SetEvent(HANDLE(ev_raw as *mut _)).ok(); }
            Ok(())
        }
    ))?;

    unsafe { WaitForSingleObject(event, INFINITE); }
    let manager = op.GetResults()?;

    // Subscribe to session-list changes so we pick up new browser windows
    let hwnd_raw = hwnd.0 as isize;
    manager.SessionsChanged(&TypedEventHandler::new(
        move |mgr: &Option<MediaManager>, _| {
            let mgr = mgr.as_ref().ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?;
            adopt_session(mgr.GetCurrentSession().ok(), HWND(hwnd_raw as *mut _));
            Ok(())
        }
    ))?;

    let hwnd_raw2 = hwnd.0 as isize;
    manager.CurrentSessionChanged(&TypedEventHandler::new(
        move |mgr: &Option<MediaManager>, _| {
            let mgr = mgr.as_ref().ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?;
            adopt_session(mgr.GetCurrentSession().ok(), HWND(hwnd_raw2 as *mut _));
            Ok(())
        }
    ))?;

    // Seed with current session
    adopt_session(manager.GetCurrentSession().ok(), hwnd);

    // Keep manager alive by leaking it — it's a singleton for the process lifetime.
    std::mem::forget(manager);

    Ok(())
}

// ── Session adoption ──────────────────────────────────────────────────────────

fn adopt_session(session: Option<Session>, hwnd: HWND) {
    let session = match session {
        Some(s) => s,
        None    => return,
    };

    let hwnd_raw = hwnd.0 as isize;

    // Clone for each closure
    let s1 = session.clone();
    let s2 = session.clone();
    let s3 = session.clone();

    // Media properties changed → pull title/artist/thumbnail
    session.MediaPropertiesChanged(&TypedEventHandler::new(
        move |_: &Option<Session>, _| {
            pull_track(&s1, HWND(hwnd_raw as *mut _));
            Ok(())
        }
    )).ok();

    // Playback info changed → pull is_playing / rate
    session.PlaybackInfoChanged(&TypedEventHandler::new(
        move |_: &Option<Session>, _| {
            pull_playback(&s2, HWND(hwnd_raw as *mut _));
            Ok(())
        }
    )).ok();

    // Timeline changed → pull new anchor
    session.TimelinePropertiesChanged(&TypedEventHandler::new(
        move |_: &Option<Session>, _| {
            pull_timeline(&s3, HWND(hwnd_raw as *mut _));
            Ok(())
        }
    )).ok();

    // Pull immediately so we don't wait for the first event
    pull_track(&session, hwnd);
    pull_playback(&session, hwnd);
    pull_timeline(&session, hwnd);
}

// ── Track metadata pull ───────────────────────────────────────────────────────

fn pull_track(session: &Session, hwnd: HWND) {
    let op = match session.TryGetMediaPropertiesAsync() {
        Ok(o)  => o,
        Err(_) => return,
    };

    // SourceAppUserModelId for browser detection
    let app_id = session.SourceAppUserModelId().unwrap_or_default().to_string();
    let hwnd_raw = hwnd.0 as isize;

    // Completion handler (fires on thread pool)
    op.SetCompleted(&windows::Foundation::AsyncOperationCompletedHandler::new(
        move |op, _| {
            let op: &IAsyncOperation<GlobalSystemMediaTransportControlsSessionMediaProperties> =
                op.ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?;
            let props = op.GetResults()?;

            let mut data = TrackData {
                title:       props.Title().unwrap_or_default().to_string(),
                artist:      props.Artist().unwrap_or_default().to_string(),
                album:       props.AlbumTitle().unwrap_or_default().to_string(),
                duration_ms: 0,  // Filled from timeline
                art_bytes:   None,
                source_app:  app_id.clone(),
            };
            data.normalise();

            // Fetch thumbnail synchronously on this thread-pool thread
            if let Ok(thumb_ref) = props.Thumbnail() {
                data.art_bytes = fetch_thumbnail_bytes(&thumb_ref).ok();
            }

            post_boxed(HWND(hwnd_raw as *mut _), WM_SMTC_TRACK, data);
            Ok(())
        }
    )).ok();
}

// ── Playback info pull ────────────────────────────────────────────────────────

fn pull_playback(session: &Session, hwnd: HWND) {
    let info = match session.GetPlaybackInfo() {
        Ok(i)  => i,
        Err(_) => return,
    };
    let status = info.PlaybackStatus().unwrap_or(PlaybackStatus::Stopped);
    let playing = status == PlaybackStatus::Playing;
    let rate    = info.PlaybackRate()
        .and_then(|r| r.cast::<IReference<f64>>())
        .and_then(|r| r.Value())
        .unwrap_or(1.0);

    // Post playback state to UI thread
    unsafe {
        PostMessageW(
            hwnd,
            WM_SMTC_PLAYBACK,
            WPARAM(if playing { 1 } else { 0 }),
            LPARAM(rate.to_bits() as isize),
        ).ok();
    }
}

// ── Timeline anchor pull ──────────────────────────────────────────────────────

fn pull_timeline(session: &Session, hwnd: HWND) {
    let tl = match session.GetTimelineProperties() {
        Ok(t)  => t,
        Err(_) => return,
    };

    let position_100ns = tl.Position()
        .map(|ts| ts.Duration)
        .unwrap_or(0);

    let last_updated_100ns = tl.LastUpdatedTime()
        .map(|ft| ft.UniversalTime as i64)
        .unwrap_or(0);

    // If LastUpdatedTime is 0 or negative (not supplied by provider), fall back to now
    let last_updated_100ns = if last_updated_100ns > 0 { last_updated_100ns } else { precise_filetime_now() };

    let duration_ms = tl.EndTime()
        .map(|ts| (ts.Duration / 10_000) as u32)
        .unwrap_or(0);

    // Rate and play status can be queried immediately from playback info
    let (is_playing, rate) = session.GetPlaybackInfo().ok().map(|info| {
        let playing = info.PlaybackStatus().unwrap_or(PlaybackStatus::Stopped) == PlaybackStatus::Playing;
        let r = info.PlaybackRate()
            .and_then(|r| r.cast::<IReference<f64>>())
            .and_then(|r| r.Value())
            .unwrap_or(1.0);
        (playing, r)
    }).unwrap_or((false, 1.0));

    let anchor = TimeAnchor {
        position_100ns,
        last_updated_100ns,
        rate,
        is_playing,
    };

    // Also carry duration in the anchor (overloaded slightly)
    let payload = SmtcTimelinePayload { anchor, duration_ms };
    post_boxed(hwnd, WM_SMTC_TIMELINE, payload);
}

pub struct SmtcTimelinePayload {
    pub anchor:      TimeAnchor,
    pub duration_ms: u32,
}

// ── Thumbnail fetch ───────────────────────────────────────────────────────────

fn fetch_thumbnail_bytes(thumb_ref: &IRandomAccessStreamReference) -> Result<Vec<u8>> {
    // Open stream — this is a blocking call on the thread-pool thread, which is fine
    let op = thumb_ref.OpenReadAsync()?;

    // Block with a wait event
    let event = unsafe { CreateEventW(None, true, false, None)? };
    let ev2_raw = event.0 as isize;
    op.SetCompleted(&windows::Foundation::AsyncOperationCompletedHandler::new(
        move |_, _| {
            unsafe { SetEvent(HANDLE(ev2_raw as *mut _)).ok(); }
            Ok(())
        }
    ))?;
    unsafe { WaitForSingleObject(event, 5000); } // 5 s timeout

    let stream = op.GetResults()?;
    let size   = stream.Size()? as u32;
    if size == 0 { return Ok(Vec::new()); }

    let reader = DataReader::CreateDataReader(&stream)?;
    let load_op = reader.LoadAsync(size)?;

    // Block on load
    let event2 = unsafe { CreateEventW(None, true, false, None)? };
    let ev3_raw = event2.0 as isize;
    load_op.SetCompleted(&windows::Foundation::AsyncOperationCompletedHandler::new(
        move |_, _| {
            unsafe { SetEvent(HANDLE(ev3_raw as *mut _)).ok(); }
            Ok(())
        }
    ))?;
    unsafe { WaitForSingleObject(event2, 5000); }
    load_op.GetResults()?;

    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf)?;
    reader.DetachStream().ok();

    Ok(buf)
}

// ── PostMessage helper ────────────────────────────────────────────────────────

/// Box `data`, convert to raw pointer, PostMessage as WPARAM.
/// The UI thread must receive it and call Box::from_raw.
fn post_boxed<T: 'static>(hwnd: HWND, msg: u32, data: T) {
    let ptr = Box::into_raw(Box::new(data)) as usize;
    unsafe {
        PostMessageW(hwnd, msg, WPARAM(ptr), LPARAM(0)).ok();
    }
}

// ── Toggle play/pause (fire and forget) ───────────────────────────────────────

/// Call from the UI thread when the user clicks the album art.
pub fn toggle_play_pause(_hwnd: HWND) {
    // We need the current session. Since we can't hold it after adoption
    // (without a Mutex), we look it up fresh each time.
    // This is a low-frequency action; the overhead is acceptable.
    if let Ok(manager_op) = MediaManager::RequestAsync() {
        // Non-blocking: fire the completion and let it handle itself
        manager_op.SetCompleted(&windows::Foundation::AsyncOperationCompletedHandler::new(
            move |op, _| {
                let op: &IAsyncOperation<MediaManager> = op.ok_or_else(|| windows::core::Error::from(HRESULT(-1)))?;
                let manager = op.GetResults()?;
                if let Ok(session) = manager.GetCurrentSession() {
                    session.TryTogglePlayPauseAsync()?.SetCompleted(
                        &windows::Foundation::AsyncOperationCompletedHandler::new(|_, _| Ok(()))
                    )?;
                }
                Ok(())
            }
        )).ok();
    }
}
