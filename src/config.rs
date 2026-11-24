//! Runtime settings shared by the UI and capture engine.

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::{w, PCWSTR};
use windows::Win32::Graphics::Gdi::{GetDC, GetDeviceCaps, ReleaseDC, VREFRESH};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Hard RAM ceiling for the encoded replay ring. At 100 Mbps, ten minutes is
/// about 7.5 GB, which is not acceptable on a 16 GB laptop.
pub const REPLAY_RAM_CAP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Native,
    P1440,
    P1080,
    P720,
    P480,
    Custom,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Resolution::Native => "Native",
            Resolution::P1440 => "1440p",
            Resolution::P1080 => "1080p",
            Resolution::P720 => "720p",
            Resolution::P480 => "480p",
            Resolution::Custom => "Custom",
        }
    }

    /// Scale factor applied on the GPU. Native and Custom use their dimensions
    /// directly, so both are 1.0 here.
    pub fn scale(self) -> f32 {
        match self {
            Resolution::Native | Resolution::Custom => 1.0,
            Resolution::P1440 => 1440.0 / 2160.0,
            Resolution::P1080 => 1080.0 / 2160.0,
            Resolution::P720 => 720.0 / 2160.0,
            Resolution::P480 => 480.0 / 2160.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControl {
    Cbr,
    Vbr,
    Cqp,
}

impl RateControl {
    pub fn label(self) -> &'static str {
        match self {
            RateControl::Cbr => "CBR",
            RateControl::Vbr => "VBR",
            RateControl::Cqp => "CQP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    LowSize,
    Balanced,
    HighQuality,
    Custom,
}

impl Preset {
    pub fn label(self) -> &'static str {
        match self {
            Preset::LowSize => "Low size",
            Preset::Balanced => "Balanced",
            Preset::HighQuality => "High quality",
            Preset::Custom => "Custom",
        }
    }

    /// Sensible starting points. Presets only rewrite these fields; the user
    /// can then tweak anything and the preset flips to Custom.
    pub fn apply(self, c: &mut Config) {
        match self {
            Preset::LowSize => {
                c.resolution = Resolution::P1080;
                c.fps = 60;
                c.bitrate_mbps = 10;
                c.rate_control = RateControl::Vbr;
                c.quality = 24;
            }
            Preset::Balanced => {
                c.resolution = Resolution::Native;
                c.fps = 60;
                c.bitrate_mbps = 20;
                c.rate_control = RateControl::Vbr;
                c.quality = 21;
            }
            Preset::HighQuality => {
                c.resolution = Resolution::Native;
                c.fps = 60;
                c.bitrate_mbps = 50;
                c.rate_control = RateControl::Vbr;
                c.quality = 18;
            }
            Preset::Custom => {}
        }
        c.preset = self;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureSource {
    /// Primary monitor; v1 deliberately does not auto-pick foreground windows.
    Auto,
    Monitor(u32),
    Window(u64), // HWND as u64, resolved at session start
}

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub system: bool,
    pub mic: bool,
    pub bitrate_kbps: u32,
    /// Keep loopback and mic as two MP4 tracks instead of mixing.
    pub separate_tracks: bool,
    pub mic_gain: f32,
    pub system_gain: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            system: true,
            mic: false,
            bitrate_kbps: 160,
            separate_tracks: false,
            mic_gain: 1.0,
            system_gain: 1.0,
        }
    }
}

fn default_show_settings() -> String {
    "Alt+Shift+S".into()
}

#[derive(Debug, Clone)]
pub struct Hotkeys {
    pub save_replay: String,
    pub toggle_record: String,
    pub toggle_buffer: String,
    pub show_settings: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            save_replay: "Alt+R".into(),
            toggle_record: "F10".into(),
            toggle_buffer: "F11".into(),
            show_settings: default_show_settings(),
        }
    }
}

fn default_custom_width() -> u32 {
    1920
}

fn default_custom_height() -> u32 {
    1080
}

#[derive(Debug, Clone)]
pub struct Config {
    // Replay
    pub replay_seconds: u32,       // 5..120
    pub delayed_stop_seconds: u32, // 0..60, recorded after the replay hotkey
    pub buffer_on_start: bool,

    // Video
    pub resolution: Resolution,
    pub custom_width: u32,
    pub custom_height: u32,
    pub fps: u32,
    pub rate_control: RateControl,
    pub bitrate_mbps: u32, // 2..100
    pub quality: u32,      // 1..51 (lower = better) for CQP/VBR CQ
    pub codec: Codec,
    pub preset: Preset,

    // Audio
    pub audio: AudioConfig,

    // Capture
    pub source: CaptureSource,
    pub capture_cursor: bool,

    // Output / behaviour
    pub output_dir: String,
    pub hotkeys: Hotkeys,
    pub minimize_to_tray: bool,
    pub show_toast: bool,
    pub sound_enabled: bool,
    pub autostart: bool,
    pub autostart_only_on_ac: bool,
    pub buffer_only_on_ac: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            replay_seconds: 20,
            delayed_stop_seconds: 2,
            buffer_on_start: true,
            resolution: Resolution::P1080,
            custom_width: default_custom_width(),
            custom_height: default_custom_height(),
            fps: 30,
            rate_control: RateControl::Vbr,
            bitrate_mbps: 20,
            quality: 21,
            codec: Codec::H264,
            preset: Preset::Balanced,
            audio: AudioConfig::default(),
            source: CaptureSource::Auto,
            capture_cursor: true,
            output_dir: default_output_dir(),
            hotkeys: Hotkeys::default(),
            minimize_to_tray: true,
            show_toast: true,
            sound_enabled: true,
            autostart: false,
            autostart_only_on_ac: true,
            buffer_only_on_ac: false,
        }
    }
}

/// Query the primary monitor refresh rate. Invalid values fall back to 60 Hz.
pub fn monitor_refresh_hz() -> u32 {
    // VREFRESH (116) is what the Win32 API calls REFRESH.
    unsafe {
        let dc = GetDC(None);
        let hz = GetDeviceCaps(Some(dc), VREFRESH);
        let _ = ReleaseDC(None, dc);
        if (1..=480).contains(&hz) {
            hz as u32
        } else {
            60
        }
    }
}

pub fn default_output_dir() -> String {
    r"D:\Videos\Cliffy".to_string()
}

/// Open a file or folder through the Windows shell using its default handler.
pub fn open_path(path: impl AsRef<Path>) -> io::Result<()> {
    let path = path.as_ref();
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR::from_raw(wide_path.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a pseudo-handle > 32 on success.
    if result.0 as usize <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl Config {
    pub fn sanitize(&mut self) {
        // v1 muxes and encodes H.264 only, regardless of stored config.
        self.codec = Codec::H264;
        self.replay_seconds = self.replay_seconds.clamp(5, 120);
        self.delayed_stop_seconds = self.delayed_stop_seconds.min(60);
        self.custom_width = self.custom_width.clamp(320, 7680) & !1;
        self.custom_height = self.custom_height.clamp(320, 7680) & !1;
        self.fps = self.fps.clamp(1, 480);
        self.bitrate_mbps = self.bitrate_mbps.clamp(2, 100);
        self.quality = self.quality.clamp(1, 51);
        self.capture_cursor = true;
        self.minimize_to_tray = true;
        let trimmed = self.output_dir.trim().trim_matches('"').trim();
        if trimmed.is_empty() {
            self.output_dir = default_output_dir();
        } else {
            self.output_dir = trimmed.to_string();
        }
    }

    /// Rough size estimate shown in the UI. Video bits plus audio bits.
    pub fn estimated_bytes_per_minute(&self) -> u64 {
        let audio_kbps = if self.audio.system || self.audio.mic {
            self.audio.bitrate_kbps * if self.audio.separate_tracks { 2 } else { 1 }
        } else {
            0
        };
        let total_kbps = self.bitrate_mbps as u64 * 1000 + audio_kbps as u64;
        total_kbps * 1000 / 8 * 60
    }

    /// Estimated RAM held by the encoded replay buffer at this bitrate.
    pub fn estimated_buffer_bytes(&self) -> u64 {
        let audio_kbps = if self.audio.system || self.audio.mic {
            self.audio.bitrate_kbps as u64
        } else {
            0
        };
        (self.bitrate_mbps as u64 * 1000 + audio_kbps) * 1000 / 8 * self.replay_seconds as u64
    }
}

pub fn set_autostart(enable: bool) -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_SZ,
    };

    unsafe {
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let res = RegOpenKeyExW(HKEY_CURRENT_USER, subkey, Some(0), KEY_SET_VALUE, &mut hkey);
        if res.is_err() {
            return Err(format!("Could not open registry Run key: {:?}", res));
        }

        let val_name = w!("Cliffy");
        if enable {
            let current_exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let exe_str = format!("\"{}\" --autostart", current_exe.display());
            let wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();
            let slice = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            let set_res = RegSetValueExW(hkey, val_name, Some(0), REG_SZ, Some(slice));
            let _ = RegCloseKey(hkey);
            if set_res.is_err() {
                return Err(format!(
                    "Could not set registry autostart value: {:?}",
                    set_res
                ));
            }
        } else {
            let _ = RegDeleteValueW(hkey, val_name);
            let _ = RegCloseKey(hkey);
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn is_autostart_registered() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    };
    unsafe {
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let subkey = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey,
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .is_ok()
        {
            let val_name = w!("Cliffy");
            let res = RegQueryValueExW(hkey, val_name, None, None, None, None);
            let _ = RegCloseKey(hkey);
            return res.is_ok();
        }
    }
    false
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SystemPowerStatus {
    pub ac_line_status: u8,
    pub battery_flag: u8,
    pub battery_life_percent: u8,
    pub system_status_flag: u8,
    pub battery_life_time: u32,
    pub battery_full_life_time: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetSystemPowerStatus(lpSystemPowerStatus: *mut SystemPowerStatus) -> i32;
}

pub fn get_power_status() -> Option<SystemPowerStatus> {
    let mut sps = SystemPowerStatus {
        ac_line_status: 255,
        battery_flag: 255,
        battery_life_percent: 255,
        system_status_flag: 0,
        battery_life_time: 0,
        battery_full_life_time: 0,
    };
    let ok = unsafe { GetSystemPowerStatus(&mut sps) };
    if ok != 0 {
        Some(sps)
    } else {
        None
    }
}

pub fn is_ac_power_connected() -> bool {
    match get_power_status() {
        Some(sps) => sps.ac_line_status == 1 || sps.ac_line_status == 255,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_requested_capture_and_output() {
        let cfg = Config::default();
        assert_eq!(cfg.resolution, Resolution::P1080);
        assert_eq!(cfg.fps, 30);
        assert_eq!(cfg.replay_seconds, 20);
        assert_eq!(cfg.delayed_stop_seconds, 2);
        assert_eq!(cfg.output_dir, r"D:\Videos\Cliffy");
        assert!(cfg.sound_enabled);
    }
}
