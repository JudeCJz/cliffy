# Changelog

## 2026-11-25

- Added the compact dark settings window based on the supplied HTML design.
- Added tray visibility toggling, Alt+Shift+S settings access, and launch-to-tray behavior.
- Set defaults to 1080p, 30 FPS, 20 seconds of replay pre-roll, and 2 seconds of delayed post-roll.
- Added a live replay-buffer fill timer and delayed replay save handling.
- Added unique replay filenames with an `_extraNsec` suffix.
- Kept settings in memory and removed config-file editing from the UI.
- Verified with `cargo check --offline`, `cargo test --offline`, and a release build.
- Added NVIDIA NVENC selection with Intel Quick Sync fallback on supported GPUs.
