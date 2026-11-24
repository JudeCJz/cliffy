//! Overlays for Cliffy:
//! 1. Simple, modern HUD splash triggered by keybinds/events (2.4s auto-dismiss).
//! 2. Top-right active recording badge (small logo plus live REC timer) displayed continuously on
//!    top of the recording layer and all other overlays while recording is active.
//! 3. Native Windows MCI audio playback for switch-sound.mp3.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DrawTextW, GdiFlush, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD,
    NONANTIALIASED_QUALITY, OUT_TT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, HCURSOR, HTTRANSPARENT, HWND_TOPMOST, MSG,
    PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WM_NCHITTEST, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::engine::AppShared;

// Dimensions for the simple splash HUD
const SPLASH_WIDTH: u32 = 220;
const SPLASH_HEIGHT: u32 = 44;
const SPLASH_STRIDE: usize = SPLASH_WIDTH as usize * 4;
const SPLASH_DURATION: Duration = Duration::from_millis(2400);
const SPLASH_LOGO_SIZE: usize = 32;
const SPLASH_LOGO_X: usize = 12;
const SPLASH_LOGO_Y: usize = 6;

// Dimensions for the active recording badge
const BADGE_WIDTH: u32 = 140;
const BADGE_HEIGHT: u32 = 44;
const BADGE_STRIDE: usize = BADGE_WIDTH as usize * 4;
const BADGE_MARGIN: i32 = 24;
const BADGE_BACKGROUND_ALPHA: u8 = 168;
const BADGE_BACKGROUND_BGRA: [u8; 3] = [29, 23, 19];
// Logo is 64 px in the source asset; show it 1.5 times smaller (about 43 px).
const BADGE_LOGO_SIZE: u32 = 43;

static SPLASH_GENERATION: AtomicU64 = AtomicU64::new(0);
static SPLASH_MESSAGE: Mutex<String> = Mutex::new(String::new());
static SPLASH_SHOW_LOGO: AtomicBool = AtomicBool::new(false);
static RECORDING_ACTIVE: AtomicBool = AtomicBool::new(false);
static TOAST_ENABLED: AtomicBool = AtomicBool::new(true);
static SOUND_ENABLED: AtomicBool = AtomicBool::new(true);
static SOUND_PLAY_COUNTER: AtomicU64 = AtomicU64::new(0);

static SWITCH_SOUND_BYTES: &[u8] = include_bytes!("../assets/switch-sound.mp3");

#[link(name = "winmm")]
extern "system" {
    fn mciSendStringW(
        lpstr_command: *const u16,
        lpstr_return_string: *mut u16,
        u_return_length: u32,
        hwnd_callback: isize,
    ) -> u32;
}

fn get_sound_file_path() -> std::path::PathBuf {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let dir = std::path::PathBuf::from(appdata).join("Cliffy");
        let path = dir.join("switch-sound.mp3");
        if !path.exists() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, SWITCH_SOUND_BYTES);
        }
        if path.exists() {
            return path;
        }
    }
    let dl_path = std::path::PathBuf::from(r"D:\Downloads\switch-sound.mp3");
    if dl_path.exists() {
        return dl_path;
    }
    std::path::PathBuf::from("assets/switch-sound.mp3")
}

/// Plays the switch-sound.mp3 audio track using Windows MCI
pub fn play_switch_sound() {
    // Keep the UI sound itself out of saved clips: silence captured desktop
    // audio while it plays, without disturbing A/V timing.
    crate::audio::suppress_capture_sound(Duration::from_millis(1200));
    std::thread::spawn(|| {
        let path = get_sound_file_path();
        let path_str = path.to_string_lossy();
        let id = SOUND_PLAY_COUNTER.fetch_add(1, Ordering::Relaxed);
        let alias = format!("sw_snd_{}", id % 8);

        let close_cmd: Vec<u16> = format!("close {}\0", alias).encode_utf16().collect();
        let open_cmd: Vec<u16> = format!("open \"{}\" type mpegvideo alias {}\0", path_str, alias)
            .encode_utf16()
            .collect();
        let volume_cmd: Vec<u16> = format!("setaudio {} volume to 400\0", alias)
            .encode_utf16()
            .collect();
        let play_cmd: Vec<u16> = format!("play {} from 0\0", alias).encode_utf16().collect();

        unsafe {
            mciSendStringW(close_cmd.as_ptr(), std::ptr::null_mut(), 0, 0);
            mciSendStringW(open_cmd.as_ptr(), std::ptr::null_mut(), 0, 0);
            mciSendStringW(volume_cmd.as_ptr(), std::ptr::null_mut(), 0, 0);
            mciSendStringW(play_cmd.as_ptr(), std::ptr::null_mut(), 0, 0);
        }
    });
}

/// Enable or disable all toast splashes, including direct ones.
pub fn set_enabled(enabled: bool) {
    TOAST_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Enable or disable the switch sound for all splashes.
pub fn set_sound_enabled(enabled: bool) {
    SOUND_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Fire a simple, sleek HUD splash notification (auto-dismisses after 2.4s).
pub fn splash(message: impl Into<String>, sound: bool) {
    show_splash(message, sound, false);
}

fn clipped_splash(sound: bool) {
    show_splash("CLIPPED", sound, true);
}

fn show_splash(message: impl Into<String>, sound: bool, show_logo: bool) {
    if !TOAST_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let msg = message.into();
    if let Ok(mut lock) = SPLASH_MESSAGE.lock() {
        *lock = msg;
    }
    SPLASH_SHOW_LOGO.store(show_logo, Ordering::Relaxed);
    SPLASH_GENERATION.fetch_add(1, Ordering::Release);
    if sound && SOUND_ENABLED.load(Ordering::Relaxed) {
        play_switch_sound();
    }
}

/// Compatibility wrapper for toast calls. Triggers a simple splash.
#[allow(dead_code)]
pub fn toast(message: impl Into<String>, sound: bool) {
    splash(message, sound);
}

/// Activate or deactivate the fallback flag for the top-right recording badge.
/// The overlay loop derives visibility from the engine snapshot; this flag is
/// only used if that snapshot cannot be locked.
pub fn set_recording_overlay(active: bool) {
    RECORDING_ACTIVE.store(active, Ordering::Release);
}

pub fn spawn(shared: Arc<AppShared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("Cliffy-overlay".into())
        .spawn(move || unsafe {
            run_overlay_inner(shared);
        })
        .expect("spawn overlay thread")
}

unsafe fn run_overlay_inner(shared: Arc<AppShared>) {
    let hinstance = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE::from(h),
        Err(_) => return,
    };

    let class_name = w!("CliffyOverlayClass");
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name,
        hCursor: HCURSOR::default(),
        ..Default::default()
    };
    let _ = RegisterClassW(&class);

    // Get monitor work area
    let mut work_area = RECT::default();
    let _ = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work_area as *mut _ as *mut _),
        windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    );

    // 1. Create Active Recording Badge Window (Top-Right)
    let badge_x = work_area.right - BADGE_WIDTH as i32 - BADGE_MARGIN;
    let badge_y = work_area.top + BADGE_MARGIN;

    let badge_hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name,
        w!("CliffyRecBadge"),
        WS_POPUP,
        badge_x,
        badge_y,
        BADGE_WIDTH as i32,
        BADGE_HEIGHT as i32,
        None,
        None,
        Some(hinstance),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let _ = SetWindowDisplayAffinity(badge_hwnd, WDA_EXCLUDEFROMCAPTURE);

    // 2. Create Simple Splash Window (Top-Center)
    let splash_x = (work_area.right + work_area.left - SPLASH_WIDTH as i32) / 2;
    let splash_y = work_area.top + 32;

    let splash_hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class_name,
        w!("CliffySplash"),
        WS_POPUP,
        splash_x,
        splash_y,
        SPLASH_WIDTH as i32,
        SPLASH_HEIGHT as i32,
        None,
        None,
        Some(hinstance),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let _ = SetWindowDisplayAffinity(splash_hwnd, WDA_EXCLUDEFROMCAPTURE);

    let screen_dc = GetDC(None);
    let badge_dc = CreateCompatibleDC(Some(screen_dc));
    let splash_dc = CreateCompatibleDC(Some(screen_dc));

    // Decode badge image (controller + red dot) and scale it down by 1.5.
    const BADGE_PNG_BYTES: &[u8] = include_bytes!("../assets/rec_badge_64.png");
    let badge_img = match image::load_from_memory(BADGE_PNG_BYTES) {
        Ok(dyn_img) => dyn_img
            .resize_exact(
                BADGE_LOGO_SIZE,
                BADGE_LOGO_SIZE,
                image::imageops::FilterType::Lanczos3,
            )
            .to_rgba8(),
        Err(_) => {
            let _ = ReleaseDC(None, screen_dc);
            let _ = DeleteDC(badge_dc);
            let _ = DeleteDC(splash_dc);
            return;
        }
    };

    // Prepare badge DIB section
    let badge_bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: BADGE_WIDTH as i32,
            biHeight: -(BADGE_HEIGHT as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut badge_bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let badge_dib = match CreateDIBSection(
        Some(badge_dc),
        &badge_bmi,
        DIB_RGB_COLORS,
        &mut badge_bits,
        None,
        0,
    ) {
        Ok(b) => b,
        Err(_) => return,
    };
    let _old_badge_bmp = SelectObject(badge_dc, badge_dib.into());

    // Geometry and blend shared by every badge update while recording.
    let pt_src = POINT { x: 0, y: 0 };
    let size_badge = SIZE {
        cx: BADGE_WIDTH as i32,
        cy: BADGE_HEIGHT as i32,
    };
    let pt_badge_dst = POINT {
        x: badge_x,
        y: badge_y,
    };
    let blend_badge = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    // Prepare splash DIB section
    let splash_bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SPLASH_WIDTH as i32,
            biHeight: -(SPLASH_HEIGHT as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut splash_bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let splash_dib = match CreateDIBSection(
        Some(splash_dc),
        &splash_bmi,
        DIB_RGB_COLORS,
        &mut splash_bits,
        None,
        0,
    ) {
        Ok(b) => b,
        Err(_) => return,
    };
    let _old_splash_bmp = SelectObject(splash_dc, splash_dib.into());

    let font_splash = CreateFontW(
        -13,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_TT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        NONANTIALIASED_QUALITY,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        w!("Segoe UI"),
    );

    let font_badge = CreateFontW(
        -14,
        0,
        0,
        0,
        FW_BOLD.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_TT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        NONANTIALIASED_QUALITY,
        (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
        w!("Segoe UI"),
    );

    // Rebuild the badge DIB on each timer tick: smaller logo on the left,
    // live REC MM:SS counter on the right.
    let render_badge = |total_seconds: u64| {
        let badge_slice = std::slice::from_raw_parts_mut(
            badge_bits as *mut u8,
            BADGE_STRIDE * BADGE_HEIGHT as usize,
        );
        for pixel in badge_slice.chunks_exact_mut(4) {
            pixel[0] =
                ((BADGE_BACKGROUND_BGRA[0] as u16 * BADGE_BACKGROUND_ALPHA as u16) / 255) as u8;
            pixel[1] =
                ((BADGE_BACKGROUND_BGRA[1] as u16 * BADGE_BACKGROUND_ALPHA as u16) / 255) as u8;
            pixel[2] =
                ((BADGE_BACKGROUND_BGRA[2] as u16 * BADGE_BACKGROUND_ALPHA as u16) / 255) as u8;
            pixel[3] = BADGE_BACKGROUND_ALPHA;
        }

        let logo_y = (BADGE_HEIGHT - BADGE_LOGO_SIZE) as usize / 2;
        let raw_rgba = badge_img.as_raw();
        for y in 0..BADGE_LOGO_SIZE as usize {
            for x in 0..BADGE_LOGO_SIZE as usize {
                let src = (y * BADGE_LOGO_SIZE as usize + x) * 4;
                let idx = (y + logo_y) * BADGE_STRIDE + x * 4;
                let r = raw_rgba[src] as u32;
                let g = raw_rgba[src + 1] as u32;
                let b = raw_rgba[src + 2] as u32;
                let a = raw_rgba[src + 3] as u32;

                badge_slice[idx] = ((b * a) / 255) as u8;
                badge_slice[idx + 1] = ((g * a) / 255) as u8;
                badge_slice[idx + 2] = ((r * a) / 255) as u8;
                badge_slice[idx + 3] = a as u8;
            }
        }

        let label = format!("REC {:02}:{:02}", total_seconds / 60, total_seconds % 60);
        SetBkMode(badge_dc, TRANSPARENT);
        let old_font = SelectObject(badge_dc, font_badge.into());
        SetTextColor(badge_dc, COLORREF(0x00f9f5f1)); // TEXT_PRIMARY BGR
        let mut text_rect = RECT {
            left: BADGE_LOGO_SIZE as i32 + 4,
            top: 0,
            right: BADGE_WIDTH as i32 - 4,
            bottom: BADGE_HEIGHT as i32,
        };
        let mut wide_label: Vec<u16> = label.encode_utf16().collect();
        DrawTextW(
            badge_dc,
            &mut wide_label,
            &mut text_rect,
            DT_SINGLELINE | DT_VCENTER,
        );
        SelectObject(badge_dc, old_font);
        let _ = GdiFlush();

        // GDI text leaves the DIB alpha channel empty; restore it in the text region.
        for y in 4..BADGE_HEIGHT as usize - 4 {
            for x in (BADGE_LOGO_SIZE as usize + 4)..(BADGE_WIDTH as usize - 4) {
                let idx = y * BADGE_STRIDE + x * 4;
                if badge_slice[idx] > 0x30
                    || badge_slice[idx + 1] > 0x30
                    || badge_slice[idx + 2] > 0x30
                {
                    badge_slice[idx + 3] = 0xff;
                }
            }
        }
    };

    let mut last_splash_gen = 0u64;
    let mut last_shared_toast_gen = 0u64;
    let mut splash_show_time: Option<Instant> = None;
    let mut badge_was_shown = false;
    let mut last_badge_second: Option<u64> = None;

    while !shared.toast_stop.load(Ordering::Relaxed) {
        // Pump Win32 messages
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // ==========================================
        // 1. Manage Active Recording Badge
        // ==========================================
        let (is_rec, record_seconds) = match shared.ui.lock() {
            Ok(ui) => (ui.recording_active, ui.record_seconds),
            Err(_) => (RECORDING_ACTIVE.load(Ordering::Acquire), 0.0),
        };
        if is_rec {
            let second = record_seconds.max(0.0) as u64;
            if last_badge_second != Some(second) {
                last_badge_second = Some(second);
                render_badge(second);
                let _ = UpdateLayeredWindow(
                    badge_hwnd,
                    Some(screen_dc),
                    Some(&pt_badge_dst),
                    Some(&size_badge),
                    Some(badge_dc),
                    Some(&pt_src),
                    COLORREF(0),
                    Some(&blend_badge),
                    ULW_ALPHA,
                );
            }
            if !badge_was_shown {
                let _ = ShowWindow(badge_hwnd, SW_SHOWNOACTIVATE);
                badge_was_shown = true;
            }
            // Continuous topmost assertion every tick to stay on top of the recording layer and all overlays
            let _ = SetWindowPos(
                badge_hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        } else if badge_was_shown {
            let _ = ShowWindow(badge_hwnd, SW_HIDE);
            badge_was_shown = false;
            last_badge_second = None;
        }

        // Check shared toast generation from engine
        let shared_gen = shared.toast_generation.load(Ordering::Acquire);
        if shared_gen != last_shared_toast_gen {
            last_shared_toast_gen = shared_gen;
            let sound = shared.toast_sound.swap(false, Ordering::Relaxed);
            clipped_splash(sound);
        }

        // ==========================================
        // 2. Manage Simple HUD Splash
        // ==========================================
        let gen = SPLASH_GENERATION.load(Ordering::Acquire);
        if gen != last_splash_gen {
            last_splash_gen = gen;
            let text = SPLASH_MESSAGE.lock().unwrap().clone();

            // Render sleek dark rounded HUD pill (width 220, height 44, radius 21)
            let slice = std::slice::from_raw_parts_mut(
                splash_bits as *mut u8,
                SPLASH_STRIDE * SPLASH_HEIGHT as usize,
            );
            let cx1 = 21.0f32;
            let cx2 = (SPLASH_WIDTH as f32) - 21.0;
            let cy = 21.0f32;
            let r_outer = 20.5f32;
            let r_inner = 19.0f32;

            for y in 0..SPLASH_HEIGHT as usize {
                let yf = y as f32;
                for x in 0..SPLASH_WIDTH as usize {
                    let xf = x as f32;
                    let dist = if xf < cx1 {
                        ((xf - cx1).powi(2) + (yf - cy).powi(2)).sqrt()
                    } else if xf > cx2 {
                        ((xf - cx2).powi(2) + (yf - cy).powi(2)).sqrt()
                    } else {
                        (yf - cy).abs()
                    };

                    let idx = y * SPLASH_STRIDE + x * 4;
                    if dist <= r_outer {
                        if dist >= r_inner {
                            // Border #3b4252
                            slice[idx] = 0x52;
                            slice[idx + 1] = 0x42;
                            slice[idx + 2] = 0x3b;
                            slice[idx + 3] = 0xff;
                        } else {
                            // BG_SURFACE #14171d with 90% opacity
                            slice[idx] = 0x1d;
                            slice[idx + 1] = 0x17;
                            slice[idx + 2] = 0x14;
                            slice[idx + 3] = 0xe8;
                        }
                    } else {
                        slice[idx] = 0;
                        slice[idx + 1] = 0;
                        slice[idx + 2] = 0;
                        slice[idx + 3] = 0;
                    }
                }
            }

            let show_logo = SPLASH_SHOW_LOGO.load(Ordering::Relaxed);
            if show_logo {
                let raw_rgba = badge_img.as_raw();
                for y in 0..SPLASH_LOGO_SIZE {
                    for x in 0..SPLASH_LOGO_SIZE {
                        let src_x = x * BADGE_LOGO_SIZE as usize / SPLASH_LOGO_SIZE;
                        let src_y = y * BADGE_LOGO_SIZE as usize / SPLASH_LOGO_SIZE;
                        let src = (src_y * BADGE_LOGO_SIZE as usize + src_x) * 4;
                        let idx = (y + SPLASH_LOGO_Y) * SPLASH_STRIDE + (x + SPLASH_LOGO_X) * 4;
                        let alpha = raw_rgba[src + 3] as u32;
                        let inverse = 255 - alpha;
                        let src_b = raw_rgba[src + 2] as u32 * alpha / 255;
                        let src_g = raw_rgba[src + 1] as u32 * alpha / 255;
                        let src_r = raw_rgba[src] as u32 * alpha / 255;
                        let dst_alpha = slice[idx + 3] as u32;
                        slice[idx] = (src_b + slice[idx] as u32 * inverse / 255) as u8;
                        slice[idx + 1] = (src_g + slice[idx + 1] as u32 * inverse / 255) as u8;
                        slice[idx + 2] = (src_r + slice[idx + 2] as u32 * inverse / 255) as u8;
                        slice[idx + 3] = (alpha + dst_alpha * inverse / 255) as u8;
                    }
                }
            }

            SetBkMode(splash_dc, TRANSPARENT);
            let old_font = SelectObject(splash_dc, font_splash.into());
            SetTextColor(splash_dc, COLORREF(0x00f9f5f1)); // TEXT_PRIMARY BGR

            let mut text_rect = RECT {
                left: if show_logo {
                    (SPLASH_LOGO_X + SPLASH_LOGO_SIZE + 8) as i32
                } else {
                    10
                },
                top: 0,
                right: SPLASH_WIDTH as i32 - 10,
                bottom: SPLASH_HEIGHT as i32,
            };
            let mut wide_msg: Vec<u16> = text.encode_utf16().collect();
            DrawTextW(
                splash_dc,
                &mut wide_msg,
                &mut text_rect,
                DT_CENTER | DT_SINGLELINE | DT_VCENTER,
            );

            SelectObject(splash_dc, old_font);
            let _ = GdiFlush();

            // Fix alpha for text pixels
            for y in 4..SPLASH_HEIGHT as usize - 4 {
                for x in 10..SPLASH_WIDTH as usize - 10 {
                    let idx = y * SPLASH_STRIDE + x * 4;
                    if slice[idx] > 0x30 || slice[idx + 1] > 0x30 || slice[idx + 2] > 0x30 {
                        slice[idx + 3] = 0xff;
                    }
                }
            }

            let size_splash = SIZE {
                cx: SPLASH_WIDTH as i32,
                cy: SPLASH_HEIGHT as i32,
            };
            let pt_splash_dst = POINT {
                x: splash_x,
                y: splash_y,
            };
            let blend_splash = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                splash_hwnd,
                Some(screen_dc),
                Some(&pt_splash_dst),
                Some(&size_splash),
                Some(splash_dc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend_splash),
                ULW_ALPHA,
            );

            let _ = ShowWindow(splash_hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                splash_hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );

            splash_show_time = Some(Instant::now());
        } else if let Some(started) = splash_show_time {
            if started.elapsed() >= SPLASH_DURATION {
                let _ = ShowWindow(splash_hwnd, SW_HIDE);
                splash_show_time = None;
            } else {
                let _ = SetWindowPos(
                    splash_hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        std::thread::sleep(Duration::from_millis(30));
    }
}

unsafe extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as _);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
