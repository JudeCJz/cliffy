//! Cliffy entry point.
//!
//! Modes:
//!   Cliffy.exe                 normal mode, window UI (buffering starts automatically)
//!   Cliffy.exe --selftest [s]  headless capture test, saves one clip
//!   Cliffy.exe --out <dir>     override output folder for selftest

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod config;
mod encoder;
mod engine;
mod gpu;
mod hotkeys;
mod muxer;
mod ring;
mod toast;
mod tray;
mod types;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use config::Config;
use engine::AppShared;

/// Explicit opt-out for RivaTuner Statistics Server (RTSS) / MSI Afterburner.
/// When RTSS scans processes for 3D/OpenGL hooks, this exported symbol instructs it
/// to bypass hooking this executable so the overlay does not draw over the UI.
#[no_mangle]
#[used]
pub static RTSSHooksCompatibility: u32 = 0;

fn main() {
    std::env::set_var("RTSSHooksCompatibility", "0");
    init_com();
    if let Err(e) = mf_startup() {
        eprintln!("Media Foundation failed to start: {e}");
    }

    let args: Vec<String> = std::env::args().collect();
    let mut selftest_secs: Option<u64> = None;
    let mut out_override: Option<PathBuf> = None;
    let mut is_autostart = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--autostart" => {
                is_autostart = true;
            }
            "--selftest" => {
                let mut secs = 8;
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                    secs = v;
                    i += 1;
                }
                selftest_secs = Some(secs);
            }
            "--out" => {
                if let Some(v) = args.get(i + 1) {
                    let clean = v.trim().trim_matches('"').trim();
                    out_override = Some(PathBuf::from(clean));
                    i += 1;
                }
            }
            "--help" | "-h" => {
                println!("Cliffy [--selftest [seconds]] [--out <dir>]");
                return;
            }
            _ => {}
        }
        i += 1;
    }

    let mut cfg = Config::default();
    cfg.fps = cfg.fps.clamp(1, config::monitor_refresh_hz());

    if let Some(secs) = selftest_secs {
        cfg.replay_seconds = cfg.replay_seconds.max(secs as u32).clamp(5, 120);
        if let Some(out) = out_override {
            cfg.output_dir = out.to_string_lossy().to_string();
        }
        let out_dir = PathBuf::from(&cfg.output_dir);
        match engine::selftest(cfg, secs, out_dir) {
            Ok(path) => {
                println!("SELFTEST_OK {}", path.display());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("SELFTEST_FAIL {e}");
                std::process::exit(1);
            }
        }
    }

    toast::set_enabled(cfg.show_toast);
    toast::set_sound_enabled(cfg.sound_enabled);
    if is_autostart && cfg.autostart_only_on_ac && !crate::config::is_ac_power_connected() {
        eprintln!(
            "Cliffy: autostart skipped because device is on battery (charger not plugged in)"
        );
        return;
    }
    let shared = AppShared::new();
    let toast = toast::spawn(shared.clone());

    // Start the engine (this begins buffering because buffer_on_start = true).
    let mut engine = engine::spawn(cfg.clone(), shared.clone());

    // Global hotkeys live on their own thread; failures are non-fatal.
    let mut hotkeys = match hotkeys::spawn(engine.cmd.clone(), shared.clone(), &cfg.hotkeys) {
        Ok(h) => Some(h),
        Err(e) => {
            shared.set_error(format!("Hotkeys unavailable: {e}"));
            None
        }
    };

    // System tray icon lives on its own Win32 message loop thread.
    let tray = match tray::spawn(engine.cmd.clone(), shared.clone()) {
        Ok(t) => Some(t),
        Err(e) => {
            shared.set_error(format!("Tray unavailable: {e}"));
            None
        }
    };

    // The window owns the hotkey handle; dropping the app unregisters it.
    let hotkeys_for_ui = hotkeys.take();
    let cmd = engine.cmd.clone();
    let shared_ui = shared.clone();

    if let Err(e) = eframe::run_native(
        "Cliffy",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_visible(false)
                .with_inner_size([460.0, 420.0])
                .with_min_inner_size([440.0, 400.0])
                .with_title("Cliffy")
                .with_resizable(false)
                .with_icon(app_icon()),
            ..Default::default()
        },
        Box::new(move |cc| {
            Ok(Box::new(ui::App::new(
                cc,
                cfg,
                cmd,
                shared_ui,
                hotkeys_for_ui,
            )))
        }),
    ) {
        eprintln!("Cliffy window failed: {e}");
    }

    shared
        .quit_requested
        .store(true, std::sync::atomic::Ordering::Relaxed);
    drop(tray);
    engine.shutdown();
    drop(hotkeys);
    shared
        .toast_stop
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = toast.join();
    let _ = mf_shutdown();
}

/// 64x64 RGBA icon for the native egui window.
pub fn app_icon() -> Arc<egui::IconData> {
    let icon = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("valid assets/icon.png")
        .resize_exact(64, 64, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    Arc::new(egui::IconData {
        rgba: icon.into_raw(),
        width: 64,
        height: 64,
    })
}

fn init_com() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        // COM is required for the shell and Media Foundation APIs.
        // RPC_E_CHANGED_MODE is fine; somebody already initialised COM.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

fn mf_startup() -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL};
    // MF_VERSION for Windows 7+ is 0x00020070. The generated constant is not
    // present in every windows-rs release, so spell it out.
    const MF_VERSION: u32 = 0x0002_0070;
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string())
}

fn mf_shutdown() -> Result<(), String> {
    use windows::Win32::Media::MediaFoundation::MFShutdown;
    unsafe { MFShutdown() }.map_err(|e| e.to_string())
}
