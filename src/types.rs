//! Shared data structures between capture, engine, muxer and UI.

use std::path::PathBuf;

use bytes::Bytes;

use crate::config::{Codec, Config, Resolution};

/// One encoded video packet as produced by the Media Foundation encoder.
/// Data is the raw H.264/HEVC sample exactly as the muxer needs it.
#[derive(Clone, Debug)]
pub struct Packet {
    /// Presentation timestamp in microseconds from session start.
    pub pts_us: i64,
    /// Duration in microseconds (1e6 / fps for CFR).
    pub dur_us: u32,
    pub keyframe: bool,
    /// Encoded sample bytes. Cloning a clip only bumps this refcount.
    pub data: Bytes,
}

impl Packet {
    pub fn end_us(&self) -> i64 {
        self.pts_us + self.dur_us as i64
    }
}

/// Codec setup needed to write an MP4 header for already-encoded packets.
#[derive(Clone, Debug)]
pub struct CodecInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: Codec,
    /// Raw SPS/PPS (or VPS/SPS/PPS for HEVC) without start codes.
    pub extradata: Vec<u8>,
}

/// Everything the capture+encode thread needs, copied from Config when a
/// session starts. Plain data so it can cross threads freely.
#[derive(Clone, Debug)]
pub struct VideoSettings {
    pub target_width: u32,
    pub target_height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub codec: Codec,
    pub rate_control: crate::config::RateControl,
    pub quality: u32,
    pub preset: crate::config::Preset,
}

impl VideoSettings {
    /// Compute encoder dimensions from a source frame and the user's resolution.
    pub fn for_source(cfg: &Config, src_w: u32, src_h: u32) -> Self {
        let (tw, th) = match cfg.resolution {
            Resolution::Native => (src_w, src_h),
            // Explicit width and height; never upscale, always even.
            Resolution::Custom => (
                (cfg.custom_width.min(src_w) & !1).max(2),
                (cfg.custom_height.min(src_h) & !1).max(2),
            ),
            // Scale by height, keep aspect; never upscale.
            Resolution::P1440 | Resolution::P1080 | Resolution::P720 | Resolution::P480 => {
                let target_h = match cfg.resolution {
                    Resolution::P1440 => 1440,
                    Resolution::P1080 => 1080,
                    Resolution::P720 => 720,
                    Resolution::P480 => 480,
                    Resolution::Native | Resolution::Custom => src_h,
                }
                .min(src_h);
                let target_h = target_h & !1; // even
                let target_w = ((src_w as f32 * target_h as f32 / src_h as f32) as u32) & !1;
                (target_w.max(2), target_h)
            }
        };
        Self {
            target_width: tw & !1,
            target_height: th & !1,
            fps: cfg.fps,
            bitrate_bps: cfg.bitrate_mbps * 1_000_000,
            codec: cfg.codec,
            rate_control: cfg.rate_control,
            quality: cfg.quality,
            preset: cfg.preset,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Buffering,
    Recording,
}

#[derive(Debug, Clone)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub name: String,
    pub size_bytes: u64,
    pub modified: Option<std::time::SystemTime>,
}

impl ClipInfo {
    pub fn size_label(&self) -> String {
        let mb = self.size_bytes as f64 / (1024.0 * 1024.0);
        if mb >= 1024.0 {
            format!("{:.2} GB", mb / 1024.0)
        } else {
            format!("{:.1} MB", mb)
        }
    }
}

/// Live numbers rendered in the UI. Written by the engine thread only.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub state: SessionState,
    pub recording_active: bool,
    pub buffering_active: bool,
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub dropped_frames: u64,
    pub dropped_packets: u64,
    pub buffer_seconds: f32,
    pub buffer_bytes: u64,
    pub record_seconds: f32,
    pub memory_mb: f32,
    pub cpu_percent: f32,
    pub estimate_bytes_per_min: u64,
    pub message: String,
    pub last_error: String,
    pub clips: Vec<ClipInfo>,
    pub last_saved: Option<PathBuf>,
    pub save_in_progress: u32,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
            recording_active: false,
            buffering_active: false,
            encoder_name: "detecting".into(),
            width: 0,
            height: 0,
            fps: 0.0,
            dropped_frames: 0,
            dropped_packets: 0,
            buffer_seconds: 0.0,
            buffer_bytes: 0,
            record_seconds: 0.0,
            memory_mb: 0.0,
            cpu_percent: 0.0,
            estimate_bytes_per_min: 0,
            message: String::new(),
            last_error: String::new(),
            clips: Vec::new(),
            last_saved: None,
            save_in_progress: 0,
        }
    }
}

/// Commands from UI / hotkeys / tray into the engine thread.
#[derive(Debug, Clone)]
pub enum Command {
    StartSession,
    StopSession,
    SaveReplay,
    ToggleRecording,
    StartRecording,
    StopRecording,
    UpdateConfig(Box<Config>),
    RefreshClips,
    Shutdown,
}
