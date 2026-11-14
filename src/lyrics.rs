// lyrics.rs — LRC fetch (via WinHTTP) and parse engine.
//
// Design:
//   • fetch() runs on a WinHTTP async callback chain, entirely on thread-pool
//     threads. When done it PostMessage(WM_LYRICS_READY) with a Box<LyricsPayload>.
//   • LyricSet stores ONE String (all lines concatenated) plus a Vec of
//     (timestamp_ms, byte_offset) pairs — about 5 KB for a typical song.
//   • index_at() uses a sequential pointer (just increment) during normal
//     playback, and binary search only after seeks.

#![allow(dead_code)]

use windows::Win32::Foundation::HWND;
use windows::core::PCWSTR;

use crate::state::{LyricsPayload, WM_LYRICS_READY};

// ── LRC line entry ────────────────────────────────────────────────────────────

/// A single lyric line with its start timestamp.
#[derive(Clone, Debug)]
pub struct LyricLine {
    /// Start time in milliseconds.
    pub start_ms:  u32,
    /// Byte offset into `LyricSet::text`.
    pub offset:    u32,
    /// Byte length in `LyricSet::text`.
    pub len:       u32,
}

// ── LyricSet ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct LyricSet {
    /// All line texts concatenated (no separator needed; we use offset+len).
    pub text:  String,
    /// Sorted by `start_ms`.
    pub lines: Vec<LyricLine>,
    /// Whether this is an instrumental placeholder.
    pub instrumental: bool,
}

impl LyricSet {
    /// Return the index of the active line at `pos_ms`.
    /// Uses `hint` as a starting point for sequential scan to avoid binary
    /// search overhead during normal forward playback.
    pub fn index_at(&self, pos_ms: u32, hint: i32) -> i32 {
        if self.lines.is_empty() { return -1; }

        let lines = &self.lines;
        let n = lines.len();

        // After seek, hint may be stale — fall back to binary search.
        let use_binary = hint < 0
            || hint as usize >= n
            || pos_ms < lines[hint.max(0) as usize].start_ms;

        if use_binary {
            // Binary search: largest index where start_ms ≤ pos_ms
            let idx = lines.partition_point(|l| l.start_ms <= pos_ms);
            return if idx == 0 { -1 } else { (idx - 1) as i32 };
        }

        // Sequential scan forward from hint (typical case)
        let mut i = hint as usize;
        while i + 1 < n && lines[i + 1].start_ms <= pos_ms {
            i += 1;
        }
        i as i32
    }

    pub fn line_text(&self, idx: i32) -> &str {
        if idx < 0 || idx as usize >= self.lines.len() { return ""; }
        let l = &self.lines[idx as usize];
        &self.text[l.offset as usize .. (l.offset + l.len) as usize]
    }

    /// Timestamp of the NEXT line after `idx` (for deadline timer calculation).
    pub fn next_start_ms(&self, idx: i32) -> Option<u32> {
        let next = idx + 1;
        self.lines.get(next as usize).map(|l| l.start_ms)
    }
}

// ── LRC parser ────────────────────────────────────────────────────────────────

/// Parse a raw LRC string into a `LyricSet`.
///
/// Handles:
/// - Standard timestamps `[mm:ss.cc]` and `[mm:ss.ccc]`
/// - Multiple timestamps per line `[00:12.00][01:23.45]Line text`
/// - `[offset:N]` adjustment tag (ms)
/// - Enhanced word tags `<mm:ss.cc>` stripped
/// - Metadata tags `[ar:]`, `[ti:]`, `[al:]`, `[by:]` ignored
pub fn parse_lrc(raw: &str) -> LyricSet {
    let mut global_offset_ms: i32 = 0;
    let mut entries: Vec<(u32, String)> = Vec::new(); // (timestamp_ms, line_text)

    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.is_empty() { continue; }

        // Collect all bracket groups at the start
        let mut rest = line;
        let mut timestamps: Vec<u32> = Vec::new();

        loop {
            if !rest.starts_with('[') { break; }
            let close = match rest.find(']') {
                Some(i) => i,
                None    => break,
            };
            let tag = &rest[1..close];
            rest = &rest[close + 1..];

            // Check for offset tag
            if let Some(val) = tag.strip_prefix("offset:") {
                global_offset_ms = val.trim().parse().unwrap_or(0);
                continue;
            }

            // Try to parse as timestamp mm:ss.cc or mm:ss.ccc
            if let Some(ts) = parse_timestamp(tag) {
                timestamps.push(ts);
            }
            // Otherwise it's a metadata tag; ignore it.
        }

        if timestamps.is_empty() { continue; }

        // Strip enhanced word-level tags like <00:00.00>
        let clean = strip_enhanced_tags(rest.trim());
        if clean.is_empty() && timestamps.len() == 1 { continue; } // blank line

        for ts in timestamps {
            let adjusted = (ts as i32 + global_offset_ms).max(0) as u32;
            entries.push((adjusted, clean.clone()));
        }
    }

    // Sort by timestamp
    entries.sort_unstable_by_key(|(t, _)| *t);

    // Build LyricSet
    let mut text = String::new();
    let mut lines: Vec<LyricLine> = Vec::with_capacity(entries.len());
    for (ts, txt) in entries {
        let offset = text.len() as u32;
        let len    = txt.len() as u32;
        text.push_str(&txt);
        lines.push(LyricLine { start_ms: ts, offset, len });
    }

    LyricSet { text, lines, instrumental: false }
}

fn parse_timestamp(s: &str) -> Option<u32> {
    // Expected: "mm:ss.cc" or "mm:ss.ccc"
    let colon = s.find(':')?;
    let dot   = s.find('.')?;
    if dot <= colon { return None; }

    let mins: u32 = s[..colon].trim().parse().ok()?;
    let secs: u32 = s[colon+1..dot].trim().parse().ok()?;
    let frac_str  = &s[dot+1..];
    // Normalise to milliseconds
    let frac_ms: u32 = match frac_str.len() {
        2 => frac_str.parse::<u32>().ok()? * 10,  // centiseconds → ms
        3 => frac_str.parse::<u32>().ok()?,        // already ms
        _ => return None,
    };

    Some(mins * 60_000 + secs * 1_000 + frac_ms)
}

fn strip_enhanced_tags(s: &str) -> String {
    // Remove <mm:ss.cc> style word tags
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for ch in s.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _   => if !inside { out.push(ch); }
        }
    }
    out
}

// ── WinHTTP async fetch ───────────────────────────────────────────────────────
//
// Fetches from LRCLIB on the WinHTTP thread pool.
// On completion, posts WM_LYRICS_READY to the UI thread.

const USER_AGENT: &str = "AeroSync-Lite/1.0 (https://github.com/aerosync/aerosync-lite)";

// Primary: LRCLIB (lrclib.net)
const LRCLIB_HOST: &str = "lrclib.net";
const LRCLIB_PORT: u16  = 443;

// Backup: lrcmux aggregator (api.lrcmux.dev) - fans out to Musixmatch, Apple Music, NetEase, etc.
const LRCMUX_HOST: &str = "api.lrcmux.dev";
const LRCMUX_PORT: u16  = 443;

/// Encapsulates a fetch context — lives in a Box for the callback.
struct FetchCtx {
    hwnd_raw:    isize,
    generation:  u64,
    title:       String,
    artist:      String,
    album:       String,
    duration_ms: u32,
    body:        Vec<u8>,
    status:      u32,
}

/// Kick off an async lyrics lookup with automated multi-tier fallback:
/// 1. LRCLIB exact get (/api/get)
/// 2. LRCLIB search (/api/search)
/// 3. LRCMUX aggregator fallback (/get?title=...&artist=...)
pub fn fetch_lyrics_async(
    hwnd: HWND,
    generation: u64,
    title: &str,
    artist: &str,
    album: &str,
    duration_ms: u32,
) {
    let ctx = Box::new(FetchCtx {
        hwnd_raw: hwnd.0 as isize,
        generation,
        title: title.to_owned(),
        artist: artist.to_owned(),
        album: album.to_owned(),
        duration_ms,
        body: Vec::new(),
        status: 0,
    });

    // Spawn on a thread to keep the WinHTTP flow simple and synchronous.
    std::thread::Builder::new()
        .name(format!("lyrics-fetch-{generation}"))
        .stack_size(64 * 1024)
        .spawn(move || fetch_thread(ctx))
        .ok();
}

fn fetch_thread(ctx: Box<FetchCtx>) {
    let mut synced: Option<String> = None;
    let mut is_instrumental = false;

    // ── Tier 1: LRCLIB /api/get ──────────────────────────────────────────────
    let lrclib_get_path = build_get_path(&ctx.title, &ctx.artist, &ctx.album, ctx.duration_ms);
    if let Ok((status, body)) = winhttp_get(LRCLIB_HOST, LRCLIB_PORT, &lrclib_get_path) {
        if status == 200 && !body.is_empty() {
            let body_str = String::from_utf8_lossy(&body);
            if let Some(lrc) = extract_synced_lyrics(&body_str) {
                synced = Some(lrc);
            } else if body_str.contains("\"instrumental\":true") {
                is_instrumental = true;
            }
        }
    }

    // ── Tier 2: LRCLIB /api/search (if Tier 1 yielded no synced lyrics) ──────
    if synced.is_none() && !is_instrumental {
        let lrclib_search_path = build_search_path(&ctx.title, &ctx.artist);
        if let Ok((status, body)) = winhttp_get(LRCLIB_HOST, LRCLIB_PORT, &lrclib_search_path) {
            if status == 200 && !body.is_empty() {
                let body_str = String::from_utf8_lossy(&body);
                if let Some(lrc) = extract_synced_lyrics(&body_str) {
                    synced = Some(lrc);
                } else if body_str.contains("\"instrumental\":true") {
                    is_instrumental = true;
                }
            }
        }
    }

    // ── Tier 3: Backup Multi-Source Aggregator (api.lrcmux.dev) ──────────────
    if synced.is_none() && !is_instrumental {
        let lrcmux_path = format!(
            "/get?title={}&artist={}",
            url_encode(&ctx.title),
            url_encode(&ctx.artist),
        );
        if let Ok((status, body)) = winhttp_get(LRCMUX_HOST, LRCMUX_PORT, &lrcmux_path) {
            if status == 200 && !body.is_empty() {
                let body_str = String::from_utf8_lossy(&body);
                if let Some(lrc) = extract_lrcmux_lines(&body_str) {
                    synced = Some(lrc);
                }
            }
        }
    }

    let hwnd = HWND(ctx.hwnd_raw as *mut _);

    let lyrics = if let Some(lrc_text) = synced {
        parse_lrc(&lrc_text)
    } else if is_instrumental {
        let mut ls = LyricSet::default();
        let placeholder = "♪ Instrumental";
        ls.text = placeholder.to_owned();
        ls.lines.push(crate::lyrics::LyricLine { start_ms: 0, offset: 0, len: placeholder.len() as u32 });
        ls.instrumental = true;
        ls
    } else {
        LyricSet::default()
    };

    let payload = Box::new(LyricsPayload {
        lyrics,
        generation: ctx.generation,
    });
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd,
            WM_LYRICS_READY,
            windows::Win32::Foundation::WPARAM(Box::into_raw(payload) as usize),
            windows::Win32::Foundation::LPARAM(ctx.generation as isize),
        ).ok();
    }
}

// ── Minimal WinHTTP synchronous GET (runs on its own thread) ─────────────────

fn winhttp_get(host: &str, port: u16, path: &str) -> windows::core::Result<(u32, Vec<u8>)> {
    use windows::Win32::Networking::WinHttp::*;
    use windows::core::w;

    let host_w: Vec<u16> = host.encode_utf16().chain(std::iter::once(0)).collect();
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let ua_w:   Vec<u16> = USER_AGENT.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        // Session handle
        let hsession = WinHttpOpen(
            PCWSTR(ua_w.as_ptr()),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if hsession.is_null() { return Err(windows::core::Error::from_win32()); }
        let _session_guard = HHandleGuard(hsession);

        // Set timeouts: 5s resolve, 5s connect, 5s send, 5s receive
        WinHttpSetTimeouts(hsession, 5000, 5000, 5000, 5000).ok();

        // Connect
        let hconnect = WinHttpConnect(hsession, PCWSTR(host_w.as_ptr()), port, 0);
        if hconnect.is_null() { return Err(windows::core::Error::from_win32()); }
        let _connect_guard = HHandleGuard(hconnect);

        // Request (HTTPS)
        let hreq = WinHttpOpenRequest(
            hconnect,
            w!("GET"),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null_mut(),
            WINHTTP_FLAG_SECURE,
        );
        if hreq.is_null() { return Err(windows::core::Error::from_win32()); }
        let _req_guard = HHandleGuard(hreq);

        // Add Content-Type header
        let accept_hdr: Vec<u16> = "Accept: application/json\r\n".encode_utf16().collect();
        WinHttpAddRequestHeaders(
            hreq,
            &accept_hdr,
            WINHTTP_ADDREQ_FLAG_ADD,
        )?;

        // Send
        WinHttpSendRequest(hreq, None, None, 0, 0, 0)?;
        WinHttpReceiveResponse(hreq, std::ptr::null_mut())?;

        // HTTP status code
        let mut status_code: u32 = 0;
        let mut buf_size: u32 = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            hreq,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status_code as *mut u32 as *mut _),
            &mut buf_size,
            std::ptr::null_mut(),
        )?;

        // Read body
        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut avail: u32 = 0;
            WinHttpQueryDataAvailable(hreq, &mut avail)?;
            if avail == 0 { break; }
            let start = body.len();
            body.resize(start + avail as usize, 0u8);
            let mut read: u32 = 0;
            WinHttpReadData(
                hreq,
                body[start..].as_mut_ptr() as *mut _,
                avail,
                &mut read,
            )?;
            body.truncate(start + read as usize);
        }

        Ok((status_code, body))
    }
}

/// RAII guard that calls WinHttpCloseHandle on drop.
struct HHandleGuard(*mut core::ffi::c_void);
impl Drop for HHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { let _ = windows::Win32::Networking::WinHttp::WinHttpCloseHandle(self.0); }
        }
    }
}

// ── URL builders ──────────────────────────────────────────────────────────────

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
            | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => { out.push('%'); out.push_str(&format!("{b:02X}")); }
        }
    }
    out
}

fn build_get_path(title: &str, artist: &str, album: &str, duration_ms: u32) -> String {
    let mut p = format!(
        "/api/get?track_name={}&artist_name={}",
        url_encode(title),
        url_encode(artist),
    );
    if !album.is_empty() { p.push_str(&format!("&album_name={}", url_encode(album))); }
    if duration_ms > 0   { p.push_str(&format!("&duration={}", duration_ms / 1000)); }
    p
}

fn build_search_path(title: &str, artist: &str) -> String {
    format!(
        "/api/search?track_name={}&artist_name={}",
        url_encode(title),
        url_encode(artist),
    )
}

// ── JSON extraction (hand-rolled — avoids serde dependency) ──────────────────

/// Extract `syncedLyrics` string value from LRCLIB JSON (object or array).
fn extract_synced_lyrics(json: &str) -> Option<String> {
    // Try direct object first, then first element of an array.
    extract_string_field(json, "syncedLyrics")
        .or_else(|| {
            // If it's an array, find the first item with syncedLyrics
            let mut depth = 0i32;
            let mut item_start = None;
            for (i, ch) in json.char_indices() {
                match ch {
                    '{' => { depth += 1; if depth == 2 { item_start = Some(i); } }
                    '}' => {
                        if depth == 2 {
                            if let Some(s) = item_start {
                                let item = &json[s..=i];
                                if let Some(lrc) = extract_string_field(item, "syncedLyrics") {
                                    if !lrc.is_empty() { return Some(lrc); }
                                }
                            }
                            item_start = None;
                        }
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            None
        })
}

/// Extract and format lines array from lrcmux JSON response into standard LRC string.
/// Format: "lines":[{"text":"...","start":30862,"end":37762},...]
fn extract_lrcmux_lines(json: &str) -> Option<String> {
    let needle = "\"lines\":";
    let start_idx = json.find(needle)? + needle.len();
    let rest = json[start_idx..].trim_start();
    if !rest.starts_with('[') { return None; }

    let mut lrc_out = String::new();
    let mut search_str = rest;

    // Scan through items with "text": and "start":
    while let Some(start_kw) = search_str.find("\"start\":") {
        let after_start = &search_str[start_kw + 8..];
        let num_end = after_start.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        let start_ms: u32 = after_start[..num_end].parse().unwrap_or(0);

        // Find associated "text":"..."
        let text = extract_string_field(search_str, "text").unwrap_or_default();

        let mm = start_ms / 60_000;
        let ss = (start_ms % 60_000) / 1000;
        let cs = (start_ms % 1000) / 10;
        lrc_out.push_str(&format!("[{:02}:{:02}.{:02}]{}\n", mm, ss, cs, text));

        // Advance search past this element
        if let Some(brace) = search_str[start_kw..].find('}') {
            search_str = &search_str[start_kw + brace..];
        } else {
            break;
        }
    }

    if lrc_out.is_empty() { None } else { Some(lrc_out) }
}
fn extract_string_field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\":", key);
    let start  = json.find(&needle)? + needle.len();
    let rest   = json[start..].trim_start();
    if !rest.starts_with('"') { return None; }
    let inner  = &rest[1..];
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    loop {
        match chars.next()? {
            '"'  => break,
            '\\' => match chars.next()? {
                'n'  => out.push('\n'),
                'r'  => out.push('\r'),
                't'  => out.push('\t'),
                '"'  => out.push('"'),
                '\\' => out.push('\\'),
                '/'  => out.push('/'),
                _    => {}
            },
            ch => out.push(ch),
        }
    }
    Some(out)
}
