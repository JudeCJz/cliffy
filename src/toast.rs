//! Top-right recording and clip-saved badge overlays, plus native Windows MCI
//! audio playback for switch-sound.mp3.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DrawTextW, GdiFlush, GetDC,
    ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DIB_RGB_COLORS, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, NONANTIALIASED_QUALITY,
    OUT_TT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, HCURSOR, HTTRANSPARENT, HWND_TOPMOST, MSG,
    PM_REMOVE, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WM_NCHITTEST, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::engine::AppShared;

// Dimensions for the active recording badge
const BADGE_WIDTH: u32 = 140;
const BADGE_HEIGHT: u32 = 44;
const BADGE_STRIDE: usize = BADGE_WIDTH as usize * 4;
const BADGE_MARGIN: i32 = 24;
const BADGE_BACKGROUND_ALPHA: u8 = 84;
const BADGE_BACKGROUND_BGRA: [u8; 3] = [29, 23, 19];
const BADGE_CORNER_RADIUS: i32 = 10;
const SAVED_BADGE_DURATION: Duration = Duration::from_secs(1);
// Logo is 64 px in the source asset; show it 1.5 times smaller (about 43 px).
const BADGE_LOGO_SIZE: u32 = 43;

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

/// Enable or disable visual overlay confirmations.
pub fn set_enabled(enabled: bool) {
    TOAST_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Enable or disable the switch sound for overlay actions.
pub fn set_sound_enabled(enabled: bool) {
    SOUND_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Preserve configured action sounds. Visual confirmations use the badge.
pub fn splash(_message: impl Into<String>, sound: bool) {
    if !TOAST_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if sound && SOUND_ENABLED.load(Ordering::Relaxed) {
        play_switch_sound();
    }
}

/// Compatibility wrapper for older notification call sites.
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

    let screen_dc = GetDC(None);
    let badge_dc = CreateCompatibleDC(Some(screen_dc));

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

    // Rebuild the badge DIB when its label changes: logo on the left and a
    // REC timer or a short save confirmation on the right.
    let render_badge = |label: &str| {
        let badge_slice = std::slice::from_raw_parts_mut(
            badge_bits as *mut u8,
            BADGE_STRIDE * BADGE_HEIGHT as usize,
        );
        let radius = BADGE_CORNER_RADIUS;
        for y in 0..BADGE_HEIGHT as i32 {
            for x in 0..BADGE_WIDTH as i32 {
                let nearest_x = x.clamp(radius, BADGE_WIDTH as i32 - radius - 1);
                let nearest_y = y.clamp(radius, BADGE_HEIGHT as i32 - radius - 1);
                let dx = x - nearest_x;
                let dy = y - nearest_y;
                let idx = y as usize * BADGE_STRIDE + x as usize * 4;
                if dx * dx + dy * dy <= radius * radius {
                    badge_slice[idx] = ((BADGE_BACKGROUND_BGRA[0] as u16
                        * BADGE_BACKGROUND_ALPHA as u16)
                        / 255) as u8;
                    badge_slice[idx + 1] = ((BADGE_BACKGROUND_BGRA[1] as u16
                        * BADGE_BACKGROUND_ALPHA as u16)
                        / 255) as u8;
                    badge_slice[idx + 2] = ((BADGE_BACKGROUND_BGRA[2] as u16
                        * BADGE_BACKGROUND_ALPHA as u16)
                        / 255) as u8;
                    badge_slice[idx + 3] = BADGE_BACKGROUND_ALPHA;
                } else {
                    badge_slice[idx..idx + 4].fill(0);
                }
            }
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
                let inverse = 255 - a;
                let dst_alpha = badge_slice[idx + 3] as u32;
                badge_slice[idx] = ((b * a / 255) + badge_slice[idx] as u32 * inverse / 255) as u8;
                badge_slice[idx + 1] =
                    ((g * a / 255) + badge_slice[idx + 1] as u32 * inverse / 255) as u8;
                badge_slice[idx + 2] =
                    ((r * a / 255) + badge_slice[idx + 2] as u32 * inverse / 255) as u8;
                badge_slice[idx + 3] = (a + dst_alpha * inverse / 255) as u8;
            }
        }

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

    let mut last_badge_generation = 0u64;
    let mut saved_until: Option<Instant> = None;
    let mut badge_status = "CLIPPED";
    let mut badge_was_shown = false;
    let mut last_badge_label: Option<String> = None;

    while !shared.toast_stop.load(Ordering::Relaxed) {
        // Pump Win32 messages
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Clip, recording, and buffering actions temporarily replace the REC
        // timer in the same badge.
        let badge_gen = shared.badge_generation.load(Ordering::Acquire);
        if badge_gen != last_badge_generation {
            last_badge_generation = badge_gen;
            badge_status = match shared.badge_kind.load(Ordering::Acquire) {
                2 => "RECORDED",
                3 => "BUFFER ON",
                4 => "BUFFER OFF",
                _ => "CLIPPED",
            };
            let sound = shared.toast_sound.swap(false, Ordering::Relaxed);
            if TOAST_ENABLED.load(Ordering::Relaxed) {
                saved_until = Some(Instant::now() + SAVED_BADGE_DURATION);
                if sound && SOUND_ENABLED.load(Ordering::Relaxed) {
                    play_switch_sound();
                }
            }
        }

        let now = Instant::now();
        if saved_until.is_some_and(|until| until <= now) {
            saved_until = None;
        }
        let (is_rec, record_seconds) = match shared.ui.lock() {
            Ok(ui) => (ui.recording_active, ui.record_seconds),
            Err(_) => (RECORDING_ACTIVE.load(Ordering::Acquire), 0.0),
        };
        let show_badge = is_rec || saved_until.is_some();
        if show_badge {
            let label = if saved_until.is_some() {
                badge_status.to_owned()
            } else {
                let seconds = record_seconds.max(0.0) as u64;
                format!("REC {:02}:{:02}", seconds / 60, seconds % 60)
            };
            if last_badge_label.as_deref() != Some(label.as_str()) {
                render_badge(&label);
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
                last_badge_label = Some(label);
            }
            if !badge_was_shown {
                let _ = ShowWindow(badge_hwnd, SW_SHOWNOACTIVATE);
                badge_was_shown = true;
            }
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
            last_badge_label = None;
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
