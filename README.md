<div align="center">

# AeroSync-Lite

**Hyper-lightweight, zero-lag synced lyrics directly on your Windows 11 taskbar.**

[![Platform](https://img.shields.io/badge/Platform-Windows%2011%20%7C%2010-0078D6?logo=windows&logoColor=white)](https://github.com/alvin-alvo/AeroSync-Lite)
[![Language](https://img.shields.io/badge/Language-Rust-dea584?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![Memory Footprint](https://img.shields.io/badge/Private%20RAM-~6.5%20MB-brightgreen)](#ram-budget-vs-the-competition)
[![Binary Size](https://img.shields.io/badge/Binary%20Size-273%20KB-blue)](https://github.com/alvin-alvo/AeroSync-Lite/releases)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)

*Seamlessly intercepts YouTube Music audio in any web browser via native Windows System Media Transport Controls (SMTC) — no browser extensions, no Electron bloat, and zero background polling.*

[Quickstart](#quickstart) • [Features](#key-features) • [Controls](#usage--controls) • [Architecture Docs](docs/ARCHITECTURE.md) • [Contributing](#contributing)

</div>

---

## Overview

Existing taskbar lyric utilities are notoriously resource-heavy — running bloated Electron runtimes (consuming 200+ MB RAM), spawning heavy .NET runtimes, or constantly polling at 60 Hz to redraw single lines of text.

**AeroSync-Lite** is written from scratch in bare-metal **Rust** using low-level Win32 APIs, native WinRT media hooks, and CPU-rendered GDI surfaces composited directly by the Desktop Window Manager (DWM).

It docks directly in the empty space of your Windows taskbar, presenting the current album thumbnail and synchronized lyric line with pixel-perfect Segoe UI typography and zero system latency.

---

## RAM Budget vs. The Competition

| Tool | Technology Stack | Idle / Active RAM (Private Working Set) | CPU Utilization |
|---|---|---|---|
| **Spicy Taskbar** | Electron / Node.js / Chromium | ~180 – 300 MB | 1.5 – 5.0% (Continuous RAF) |
| **MusicBar** | C# / .NET WPF / MilCore | ~40 – 90 MB | 0.8 – 2.0% (DispatcherTimer) |
| **TaskbarLyrics** | Native C++ / Direct2D | ~15 – 30 MB | 0.2 – 0.8% (Polling timer) |
| **AeroSync-Lite** | **Native Rust + GDI DIB + WinRT** | **~5 – 7 MB** | **0.00% (Event & Deadline driven)** |

> **Note:** Measurements reflect **Private Working Set** (true application memory consumption), rather than Task Manager's total working set which includes shared system DLL caches.

---

## Key Features

- ⚡ **Zero-Lag Dead-Reckoning Sync:** Extrapolates audio timestamps against high-precision wall-clock FILETIME. Lyric lines advance with millisecond precision without audio lag.
- 🪶 **True Zero-Polling Architecture:** Uses Windows Waitable High-Resolution Timers scheduled to the exact microsecond deadline of the *next* lyric line. While paused or idle, CPU consumption is strictly **0.00%**.
- 🌐 **Zero Browser Extensions Required:** Hooks directly into the Windows System Media Transport Controls (SMTC) API. Automatically detects playback from YouTube Music running in Google Chrome, Microsoft Edge, Mozilla Firefox, Brave, or Opera.
- 🎨 **Direct DWM Software Rendering:** Renders text and album art into a 32-bit ARGB Device-Independent Bitmap (DIB) with alpha derived from luminance, uploaded directly via `UpdateLayeredWindow`. Avoids GPU driver allocations entirely.
- 🛡️ **Multi-Tier Resilient Lyric Sourcing:** Automatically fetches synced `.lrc` lyrics with multi-tier fallback:
  1. *Tier 1:* Direct exact match from **LRCLIB** (`/api/get`).
  2. *Tier 2:* Fuzzy duration search from **LRCLIB** (`/api/search`).
  3. *Tier 3:* Automatic fallback to **LRCMUX** aggregator (`api.lrcmux.dev`).
- 📌 **Windows 11 Z-Order Pinning:** Employs `RegisterShellHookWindow` activation tracking and `WM_WINDOWPOSCHANGING` enforcement so the widget never disappears behind the taskbar when clicking other applications or Start menu.
- ⏯️ **Interactive Media Control:** Left-clicking the square album thumbnail instantly toggles Play/Pause.
- ⏱️ **Bluetooth & Audio Latency Compensation:** Right-click context menu enables adjusting lyric sync offset in real-time in $\pm 100\text{ ms}$ steps.

---

## Quickstart

### 1. Download
Download the latest pre-compiled standalone executable from the [Releases](https://github.com/alvin-alvo/AeroSync-Lite/releases) page:
- **`aerosync-lite.exe`** (~273 KB standalone, no runtime required)

### 2. Run
Place the binary in any folder and run it:
```powershell
.\aerosync-lite.exe
```

### 3. Play Music
Start playing any track in **YouTube Music** on your preferred browser. AeroSync-Lite will instantly dock onto your Windows 11 taskbar next to the system tray and display auto-synced lyrics.

---

## Usage & Controls

The widget sits seamlessly in the free space of your Windows taskbar:

| Action | Control | Description |
|---|---|---|
| **Toggle Play / Pause** | Left-click album art | Toggles browser audio playback via SMTC |
| **Adjust Offset (+100 ms)** | Right-click → `Offset +100ms` | Delays lyrics by 100 ms (useful for Bluetooth lag) |
| **Adjust Offset (-100 ms)** | Right-click → `Offset -100ms` | Advances lyrics by 100 ms |
| **Reset Offset** | Right-click → `Reset Offset` | Restores default 0 ms sync |
| **Exit Application** | Right-click → `Quit` | Gracefully closes widget and unregisters hooks |

---

## Building from Source

### Prerequisites
- Windows 10 (Build 19041+) or Windows 11
- [Rust toolchain](https://www.rust-lang.org/tools/install) (MSVC ABI: `x86_64-pc-windows-msvc`)
- Visual Studio Build Tools (C++ development tools)

### Build Steps
```powershell
# 1. Clone the repository
git clone https://github.com/alvin-alvo/AeroSync-Lite.git
cd AeroSync-Lite

# 2. Build optimized release binary
cargo build --release

# 3. Executable will be generated at:
# .\target\release\aerosync-lite.exe
```

---

## Project Structure

```
AeroSync-Lite/
├── docs/
│   └── ARCHITECTURE.md     # In-depth system design, memory breakdown & math
├── src/
│   ├── main.rs             # Application entry point, message loop & WNDPROC
│   ├── state.rs            # AppState, TimeAnchor & dead-reckoning extrapolation
│   ├── smtc.rs             # Windows WinRT SMTC session event handlers
│   ├── lyrics.rs           # Multi-tier WinHTTP client & fast LRC parser
│   ├── render.rs           # 32-bit GDI software renderer & alpha composition
│   ├── taskbar.rs          # Taskbar geometry measurement & docking logic
│   └── art.rs              # WIC thumbnail decoder & corner-rounding mask
├── Cargo.toml              # Minimal dependencies (windows crate only)
├── build.rs                # Application manifest embedding
├── manifest.xml            # Per-monitor DPI v2 & UAC configuration
└── LICENSE                 # GNU General Public License v3.0
```

For detailed systems documentation, memory layout, and concurrency architecture, see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## Contributing

Contributions are welcome! Please follow these steps:

1. **Fork the Repository:** Create your personal fork on GitHub.
2. **Create a Feature Branch:** `git checkout -b feature/my-enhancement`
3. **Follow Code Conventions:**
   - Keep dependencies minimal (avoid heavy external crates or async runtimes).
   - Format Rust code with standard formatting: `cargo fmt`.
   - Ensure clean compilation without warnings: `cargo check`.
4. **Verify Locally:** Build the release target (`cargo build --release`) and verify memory footprint remains under 10 MB.
5. **Submit a Pull Request:** Open a PR against `main` describing your changes and test results.

---

## License

This project is licensed under the **GNU General Public License v3.0** — see the [LICENSE](LICENSE) file for details.
