# Changelog

## 0.1.2 - 2026-11-25

- Removed popup toast notifications. A saved clip now briefly replaces the top-right badge label with "CLIPPED" for one second, then restores the REC timer if recording continues.
- Set the badge background to half its previous opacity, rounded its corners, and extended it behind the logo.

## 0.1.1 - 2026-11-25

- Automatically fall back from NVIDIA NVENC to Intel Quick Sync when NVIDIA encoder setup fails; verified the complete fallback path on this PC.
- Return completed and unused GPU texture slots to the capture pool so buffering continues while async encoders are busy.
- Show Alt+R, F10, and F11 beside the replay, recording, and buffering actions in the tray menu.
- Show "Start Buffering" when buffering is off.
- Add a simple translucent square background behind the top-right recording logo and timer.
- Replace the clip-saved seconds toast with the Cliffy logo and "CLIPPED".

## 2026-11-25

- Added the compact dark settings window based on the supplied HTML design.
- Added tray visibility toggling, Alt+Shift+S settings access, and launch-to-tray behavior.
- Set defaults to 1080p, 30 FPS, 20 seconds of replay pre-roll, and 2 seconds of delayed post-roll.
- Added a live replay-buffer fill timer and delayed replay save handling.
- Added unique replay filenames with an `_extraNsec` suffix.
- Kept settings in memory and removed config-file editing from the UI.
- Verified with `cargo check --offline`, `cargo test --offline`, and a release build.
- Added NVIDIA NVENC selection with Intel Quick Sync fallback on supported GPUs.
