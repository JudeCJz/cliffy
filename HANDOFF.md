# Cliffy handoff

Date: 2026-11-24
Project: `D:\VS\PROJECTS\FREE WORK\Cliffy`
Purpose: Windows-native desktop/game clipper in Rust using Windows Graphics Capture, D3D11 GPU conversion, NVIDIA NVENC or Intel Quick Sync H.264 encode, an encoded-packet replay ring, and a simplified eframe/egui window based on `D:\Downloads\base ui.html`.
State: v1 completed with hardware NVENC/Quick Sync zero-copy capture pipeline, simplified in-session settings UI, system tray, minimize-to-tray, autostart with Windows, power-aware buffering on AC/battery, a top-right recording badge, and sub-30 MB idle RAM footprint. NVIDIA is preferred when available; Intel Quick Sync is the fallback. No config file is used. Defaults are 1080p, 30 FPS, 20 seconds pre-roll plus 2 seconds delayed post-roll, `D:\Videos\Cliffy`, and clip sound on. Launching starts Cliffy in the system tray.

---

## 1. Scope Implemented & Verified

- Rust package/binary `cliffy` (`target\release\cliffy.exe`, window title `Cliffy`).
- **Windows Graphics Capture (WGC)** through vendored `windows-capture` (`[patch.crates-io]` points to `vendor/windows-capture`). The vendored copy creates the capture D3D11 device with video support (`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`), which Media Foundation hardware MFTs require.
- **D3D11 GPU conversion**: BGRA capture textures convert directly to NV12 encoder textures on the GPU via custom vertex and pixel shaders. Zero CPU readback of video frames.
  - *Fix for Green Video*: Disabled backface culling (`D3D11_CULL_NONE`) and corrected vertex winding to a full-screen quad so UV chrominance planes are actively written, eliminating solid green frames.
- **Media Foundation hardware H.264 MFT**: Prefers NVIDIA NVENC, then falls back to Intel Quick Sync. The capture adapter and encoder share the same D3D11 device for zero-copy frames; the capture session restarts on Intel if NVENC cannot initialize.
- **System Tray Integration (`src/tray.rs`)**:
  - Dedicated Win32 message loop on a background thread.
  - Custom embedded PNG icon from `assets/icon.png`.
  - Context menu: "Save Replay Clip  Alt+R", "Toggle Recording  F10", "Toggle Buffering  F11", and "Quit Cliffy". Left-click or Alt+Shift+S opens settings.
  - Left-click toggles the settings window; Alt+Shift+S opens/focuses it.
  - Click outside cleanly dismisses the context menu using a top-level hidden tool window (`WS_EX_TOOLWINDOW` / `WS_POPUP`) with `PostMessageW(Some(hwnd), WM_NULL, ...)`.
  - Clean `NIM_DELETE` and thread joining on drop/exit prevents ghost taskbar icons.
- **Minimize to Tray (`src/ui.rs`)**:
  - Always on and forced true by `Config::sanitize()`; no toggle in the simplified window.
  - Minimizing or closing the native window hides it. Left-clicking the tray icon or pressing Alt+Shift+S restores and focuses it; choosing Quit Cliffy from the tray exits.
- **Power & Charger Awareness (`src/config.rs`, `src/engine.rs`, `src/main.rs`)**:
  - Querying Win32 `GetSystemPowerStatus` directly from `kernel32.dll`.
  - Config options:
    - `autostart`: Start with Windows (registers `"path\cliffy.exe" --autostart` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`).
    - `autostart_only_on_ac`: If started on battery, exits immediately with zero resource usage.
    - `buffer_only_on_ac`: Automatically pauses buffering when unplugged / running on battery, and auto-resumes buffering when plugged back into the charger.
  - No power-status readout in the simplified window; power state is used internally for autostart and buffering decisions.
- **RAM Optimization**:
  - Reduced idle working set from 300+ MB down to ~29 MB (>90% reduction).
  - Memory trimming (`EmptyWorkingSet`) is executed on session stop, after clip save, and when idle, without thrashing page faults during active 60+ fps capture.
- **Folder Button & File Selection Fix**:
  - `open()` automatically creates the directory if missing.
  - `reveal()` uses `CommandExt::raw_arg(format!("/select,\"{}\"", path.display()))` to guarantee Windows Explorer highlights the saved clip.
  - Output paths are trimmed of whitespace and outer quotation marks.
- **Recording and Clip Badge (`src/toast.rs`)**:
  - A layered Win32 badge shows the Cliffy logo and live REC timer while recording.
  - A saved clip replaces the timer with `CLIPPED` for one second; the timer returns if recording continues.
  - The background extends behind the logo, has rounded corners, and uses half its previous opacity.
  - Action sounds and the badge message pump run on the overlay thread.
- **Live settings**: UI edits auto-apply after a short debounce; `Apply Changes` applies them immediately. Settings are session-only; Cliffy does not read or write a config file. Global Alt+Shift+S restores and focuses the settings window; tray left-click toggles its visibility.
- **Replay Ring & MP4 Muxer (`src/ring.rs`, `src/muxer.rs`)**:
  - Encoded-packet ring buffer with GOP-aware trimming. Replay saves include the configured pre-roll and delayed post-roll; filenames end in `_extraNsec.mp4`.
  - Millisecond timestamping (`Clip_YYYY-MM-DD_HH-MM-SS_mmm.mp4`) prevents file collision on rapid saves.
  - Monotonic DTS clamping and duration rounding (`+ 500_000`) prevents 1-tick/frame drift.
  - Early-stop recording guard: if stopped before the first keyframe arrives, empty corrupt MP4 files are cleaned up and user is notified.

---

## 2. Architecture and Files

| File | Owns |
| --- | --- |
| `src/main.rs` | Entry point, CLI (`--autostart`, `--selftest`, `--out`, `--help`), power check on autostart, STA COM init, Media Foundation startup/shutdown, engine/hotkey/overlay/tray startup, launch of the simplified eframe/egui window. |
| `src/capture.rs` | Windows Graphics Capture handler, cursor capture toggle, source resolution, frame pacing, GPU/encoder rebuild on size change, error retry, packet/config messages. |
| `src/config.rs` | Runtime defaults, sanitization, presets, hotkeys, size estimates, `REPLAY_RAM_CAP_BYTES`, autostart registry management, `GetSystemPowerStatus` bindings. |
| `src/encoder.rs` | NVIDIA NVENC and Intel Quick Sync hardware MFT discovery/init, adapter matching, async event pumping, D3D11 surface samples, output packet extraction, SPS/PPS recovery, codec API setup. |
| `src/engine.rs` | Engine thread, bounded packet channel, ring ownership, recording, replay saves, power monitoring, stats, clip scan, `Command` handling, `UpdateConfig`, selftest driver. |
| `src/gpu.rs` | Runtime HLSL, BGRA -> NV12 shader passes with `D3D11_CULL_NONE` quad, three-texture NV12 pool, plane-sliced RTVs, texture release after encode output. |
| `src/hotkeys.rs` | `RegisterHotKey` thread and default shortcuts (Alt+R Save, F10 Record, F11 Buffer, Alt+Shift+S Show Settings). |
| `src/muxer.rs` | MP4 writing of already-encoded H.264 packets, SPS/PPS split, Annex-B/AVCC helpers, finalization. |
| `src/ring.rs` | Encoded-packet `VecDeque`, GOP-aware trim, RAM cap, keyframe-aligned replay snapshot. |
| `src/toast.rs` | Layered Win32 recording badge, one-second CLIPPED confirmation, rounded translucent background, and action sound playback. |
| `src/tray.rs` | Dedicated Win32 thread, taskbar notification icon, context menu with Alt+R, F10, and F11 shortcuts, and left-click settings toggle. |
| `src/types.rs` | `Packet`, `CodecInfo`, `VideoSettings`, `ClipInfo`, `UiSnapshot`, `Command`, `SessionState`. |
| `src/ui.rs` | Compact eframe/egui settings UI: Record/Buffer buttons with buffer-fill timer, Resolution (480p/720p/1080p/Custom), Clip Length, Delayed Stop, and Frame Rate fields, three startup toggles, Open Folder, debounced in-session apply, and an `Apply Changes` button. No scroll bar, config editor, or shortcut labels. |

---

## 3. Verified Tests

1. **Build and unit tests**:
   `cargo test --offline` passes all four tests. `cargo build --offline` and the release build succeed; existing dead-code warnings remain.
2. **Hardware encoder capture and fallback**:
   Ran the 8-second headless capture selftest with NVIDIA-first selection, with Intel Quick Sync selected directly, and with NVIDIA disabled to exercise automatic fallback. All three returned `SELFTEST_OK` and saved an MP4. Logs confirmed the RTX 3050 Laptop GPU and Intel Iris Xe Graphics were selected as expected. These runs also verified capture continues past the three-texture pool without exhausting it.
3. **RAM Footprint**:
   Working set reduced from 300+ MB to ~29 MB under idle/buffering.
4. **Power Detection**:
   `GetSystemPowerStatus` accurately identifies `ACLineStatus` (Online = Charger connected, Offline = Battery) and battery percentage.
5. **Autostart & Tray**:
   Registry key management verified; tray menu and window restore verified.
