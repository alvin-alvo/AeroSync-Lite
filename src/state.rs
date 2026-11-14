// state.rs — Single-threaded application state.
//
// Everything lives here: lyrics, playback anchor, art buffer, SMTC session
// handles, and generation counters. The UI thread is the sole writer; no
// synchronisation is needed.

use crate::lyrics::LyricSet;
use crate::render::GdiBitmap;

// ── Custom WM_APP message IDs ─────────────────────────────────────────────────
// Cross-thread messages from WinRT / WinHTTP callbacks → UI thread.
pub const WM_SMTC_TRACK:    u32 = 0x8001; // wparam = Box<TrackData> as usize
pub const WM_SMTC_PLAYBACK: u32 = 0x8002; // wparam = 1 (playing) | 0 (paused/stopped)
pub const WM_SMTC_TIMELINE: u32 = 0x8003; // wparam = Box<TimeAnchor> as usize
pub const WM_LYRICS_READY:  u32 = 0x8004; // wparam = Box<LyricsPayload> as usize, lparam = generation
pub const WM_SHELL_CHANGED: u32 = 0x8005; // re-probe taskbar geometry

// ── Playback anchor ───────────────────────────────────────────────────────────

/// Snapshot of SMTC timeline used for dead-reckoning position extrapolation.
/// Both timestamps are FILETIME (100-ns intervals since 1601-01-01 UTC).
#[derive(Clone, Debug)]
pub struct TimeAnchor {
    /// `TimelineProperties.Position` at the moment of the snapshot.
    pub position_100ns: i64,
    /// `TimelineProperties.LastUpdatedTime` from SMTC (wall-clock FILETIME).
    pub last_updated_100ns: i64,
    /// `PlaybackInfo.PlaybackRate` (1.0 for normal speed).
    pub rate: f64,
    pub is_playing: bool,
}

impl TimeAnchor {
    /// Extrapolated position in milliseconds right now.
    /// Uses `GetSystemTimePreciseAsFileTime` so the wall-clock base matches
    /// `LastUpdatedTime`, avoiding QPC-to-FILETIME mapping error.
    pub fn position_ms_now(&self) -> u32 {
        if !self.is_playing {
            return (self.position_100ns / 10_000) as u32;
        }
        let now = precise_filetime_now();
        let elapsed_100ns = (now - self.last_updated_100ns).max(0);
        let pos_100ns = self.position_100ns + (elapsed_100ns as f64 * self.rate) as i64;
        (pos_100ns / 10_000) as u32
    }
}

pub fn precise_filetime_now() -> i64 {
    use windows::Win32::System::SystemInformation::GetSystemTimePreciseAsFileTime;
    use windows::Win32::Foundation::FILETIME;
    let mut ft = FILETIME::default();
    unsafe { GetSystemTimePreciseAsFileTime(&mut ft) };
    ((ft.dwHighDateTime as i64) << 32) | ft.dwLowDateTime as i64
}

// ── Track metadata (posted cross-thread) ──────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct TrackData {
    pub title:        String,
    pub artist:       String,
    pub album:        String,
    pub duration_ms:  u32,
    /// Raw JPEG/PNG bytes from SMTC thumbnail stream.
    pub art_bytes:    Option<Vec<u8>>,
    /// SMTC SourceAppUserModelId — used to filter browser sessions.
    pub source_app:   String,
}

impl TrackData {
    /// Stable key for change detection (avoids full equality check).
    pub fn key(&self) -> String {
        format!("{}::{}::{}", self.artist, self.title, self.duration_ms / 1000)
    }

    /// Normalise strings: strip YouTube noise like " - Topic", " (Official Video)" etc.
    pub fn normalise(&mut self) {
        static TITLE_NOISE: &[&str] = &[
            " (Official Video)", " (Official Music Video)", " (Official Audio)",
            " (Lyric Video)", " (Lyrics)", " [Official Video]",
            " (Audio)", " [Audio]", " (HQ)", " (HD)",
        ];
        static ARTIST_NOISE: &[&str] = &[" - Topic", "VEVO"];

        for pat in TITLE_NOISE {
            if let Some(i) = self.title.find(pat) { self.title.truncate(i); }
        }
        for pat in ARTIST_NOISE {
            if let Some(i) = self.artist.find(pat) { self.artist.truncate(i); }
        }
        self.title  = self.title.trim().to_owned();
        self.artist = self.artist.trim().to_owned();
    }

    /// Return true if this looks like a browser / YouTube Music PWA session.
    pub fn is_browser_session(&self) -> bool {
        let id = self.source_app.to_lowercase();
        id.contains("chrome") || id.contains("msedge") || id.contains("firefox")
            || id.contains("opera") || id.contains("brave") || id.is_empty()
    }
}

// ── Lyrics payload (posted cross-thread) ──────────────────────────────────────

pub struct LyricsPayload {
    pub lyrics:     LyricSet,
    pub generation: u64,
}

// ── Application state (single-threaded, owned by UI thread) ───────────────────

pub struct AppState {
    pub anchor:       Option<TimeAnchor>,
    pub track:        Option<TrackData>,
    pub lyrics:       Option<LyricSet>,
    pub shown_idx:    i32,          // index of currently displayed lyric line
    pub generation:   u64,          // incremented on each track change; stale HTTP drops checked here
    pub last_track_key: String,

    /// Back buffer: pre-rendered next lyric line, ready for instant `UpdateLayeredWindow`.
    pub back_buf:     Option<GdiBitmap>,
    /// Front buffer: currently displayed.
    pub front_buf:    Option<GdiBitmap>,

    /// User-configured sync offset in milliseconds (can be negative).
    pub offset_ms:    i32,

    /// Whether the widget window is currently visible.
    pub visible:      bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            anchor:         None,
            track:          None,
            lyrics:         None,
            shown_idx:      -1,
            generation:     0,
            last_track_key: String::new(),
            back_buf:       None,
            front_buf:      None,
            offset_ms:      0,
            visible:        false,
        }
    }

    /// Current position in ms (with user offset applied).
    pub fn position_ms(&self) -> u32 {
        let raw = self.anchor.as_ref().map_or(0, |a| a.position_ms_now());
        (raw as i64 + self.offset_ms as i64).max(0) as u32
    }

    pub fn is_playing(&self) -> bool {
        self.anchor.as_ref().map_or(false, |a| a.is_playing)
    }
}
