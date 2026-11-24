//! Windows Graphics Capture session. Runs on its own thread (started by
//! windows-capture), converts each frame on the GPU and feeds the encoder.
//!
//! The thread never touches the UI and never blocks: packets go to a bounded
//! channel, and if the consumer is behind we drop frames instead of stalling.

use std::sync::mpsc::SyncSender;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, D3DAdapterPreference, DirtyRegionSettings,
    DrawBorderSettings, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::config::{CaptureSource, Config};
use crate::encoder::MfEncoder;
use crate::gpu::Nv12Converter;
use crate::types::{CodecInfo, Packet, VideoSettings};

fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CLIFFY_DEBUG").is_some())
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[capture] {}", format_args!($($arg)*));
        }
    };
}

pub enum EncMsg {
    /// Sent once per encoder creation (includes a new SPS/PPS).
    Config(CodecInfo),
    Packets(Vec<Packet>),
    Encoder(String),
    /// A hardware encoder could not initialize on the selected GPU adapter.
    EncoderUnavailable(String),
    Error(String),
    /// Frames captured but not encoded, for the dropped-frame readout.
    Dropped(u64),
}

pub struct Flags {
    pub tx: SyncSender<EncMsg>,
    /// Snapshot of the user config for this session.
    pub cfg: Config,
}

pub struct CaptureHandler {
    cfg: Config,
    tx: SyncSender<EncMsg>,
    encoder: Option<MfEncoder>,
    converter: Option<Nv12Converter>,
    start: Option<Instant>,
    last_encode: Option<Instant>,
    frame_interval: Duration,
    arrivals: u64,
    frame_index: u64,
    dropped: u64,
    last_size: (u32, u32),
    encoder_error: Option<String>,
    /// Skip frames briefly after a GPU/encoder failure instead of ending the
    /// whole WGC session.
    retry_frames: u32,
}

// The COM interfaces are only touched on the capture thread; windows-capture
// requires the handler type itself to be Send so it can move that thread.
unsafe impl Send for CaptureHandler {}

impl GraphicsCaptureApiHandler for CaptureHandler {
    type Flags = Flags;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let fps = ctx.flags.cfg.fps.max(1);
        Ok(Self {
            cfg: ctx.flags.cfg.clone(),
            tx: ctx.flags.tx.clone(),
            encoder: None,
            converter: None,
            start: None,
            last_encode: None,
            frame_interval: Duration::from_secs_f64(1.0 / fps as f64),
            arrivals: 0,
            frame_index: 0,
            dropped: 0,
            last_size: (0, 0),
            encoder_error: None,
            retry_frames: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let now = Instant::now();
        let start = *self.start.get_or_insert(now);
        self.arrivals += 1;
        let (w, h) = (frame.width(), frame.height());
        debug_log!("frame arrival count={} size={}x{}", self.arrivals, w, h);

        // A GPU hiccup should skip frames, not kill the capture session.
        if self.retry_frames > 0 {
            self.retry_frames -= 1;
            return Ok(());
        }

        // Frame-rate cap. WGC is change-driven, so a 144 Hz game can deliver
        // 144 frames/s; we only want the configured rate.
        if let Some(last) = self.last_encode {
            if now.duration_since(last) + Duration::from_micros(500) < self.frame_interval {
                self.dropped += 1;
                return Ok(());
            }
        }

        // (Re)build the GPU + encoder stage if the source size changed.
        if w < 2 || h < 2 {
            return Ok(());
        }
        if self.encoder.is_none() || self.last_size != (w, h) {
            if let Err(e) = self.rebuild(frame.device(), frame.device_context(), w, h) {
                self.report_error(format!("Encoder setup failed: {e}"));
                self.retry_frames = 60;
                return Ok(());
            }
        }

        let converter = match self.converter.as_mut() {
            Some(c) => c,
            None => return Ok(()),
        };
        let encoder = match self.encoder.as_mut() {
            Some(e) => e,
            None => return Ok(()),
        };

        // Skip conversion only when every NV12 texture is still waiting for
        // its encoded packet. An async MFT may report busy at encode() time;
        // that error drops this frame and releases its slot without stalling.
        if !converter.has_free_slot() {
            self.dropped += 1;
            debug_log!("drop before convert: no free slot count={}", self.arrivals);
            return Ok(());
        }

        let (texture, slot) = match converter.convert(frame.as_raw_texture()) {
            Ok((texture, slot)) => {
                debug_log!("convert ok slot={}", slot);
                (texture, slot)
            }
            Err(e) => {
                debug_log!("convert failed: {}", e);
                self.report_error(format!("GPU convert failed: {e}"));
                self.retry_frames = 30;
                return Ok(());
            }
        };

        let pts_us = (now - start).as_micros() as i64;
        self.last_encode = Some(now);
        self.frame_index += 1;

        let encoded = encoder.encode(&texture, slot, pts_us);
        match encoded {
            Ok((packets, released)) => {
                debug_log!(
                    "encode ok slot={} pts_us={} packets={} released={:?}",
                    slot,
                    pts_us,
                    packets.len(),
                    released
                );
                for slot in released {
                    converter.release(slot);
                }
                // Quick Sync can publish SPS/PPS only in the first encoded
                // sample. Send the refreshed config first; the sync channel
                // keeps it ahead of the packets that depend on it.
                if let Some(info) = encoder.take_pending_config() {
                    debug_log!("pending config extradata_len={}", info.extradata.len());
                    let _ = self.tx.try_send(EncMsg::Config(info));
                }
                if !packets.is_empty() {
                    let _ = self.tx.try_send(EncMsg::Packets(packets));
                }
                if self.dropped > 0 {
                    let _ = self.tx.try_send(EncMsg::Dropped(self.dropped));
                    self.dropped = 0;
                }
            }
            Err(e) => {
                // The texture was not submitted (or the MFT rejected it), so
                // it is safe to reuse immediately.
                converter.release(slot);
                if e.contains("busy") {
                    debug_log!("encode busy slot={} pts_us={}: {}", slot, pts_us, e);
                    self.dropped += 1;
                } else {
                    debug_log!("encode failed slot={} pts_us={}: {}", slot, pts_us, e);
                    self.report_error(format!("encode failed: {e}"));
                    self.encoder = None;
                    self.converter = None;
                    self.retry_frames = 60;
                }
            }
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        let _ = self
            .tx
            .try_send(EncMsg::Error("capture item closed".into()));
        Ok(())
    }
}

impl CaptureHandler {
    fn rebuild(
        &mut self,
        device: &ID3D11Device,
        context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.encoder = None;
        self.converter = None;

        let video = VideoSettings::for_source(&self.cfg, src_w, src_h);

        // Encoders that can't take this GPU fail here; we surface the message
        // instead of pretending to record.
        let encoder = match MfEncoder::new(device, &video) {
            Ok(e) => e,
            Err(e) => {
                self.encoder_error = Some(e.clone());
                let _ = self.tx.try_send(EncMsg::EncoderUnavailable(e.clone()));
                return Err(e.into());
            }
        };

        let converter = match Nv12Converter::new(
            device,
            context,
            src_w,
            src_h,
            video.target_width,
            video.target_height,
        ) {
            Ok(c) => c,
            Err(e) => {
                self.report_error(e.clone());
                return Err(e.into());
            }
        };

        let _ = self.tx.try_send(EncMsg::Config(encoder.info.clone()));
        let _ = self.tx.try_send(EncMsg::Encoder(encoder.name.clone()));
        self.encoder = Some(encoder);
        self.converter = Some(converter);
        self.last_size = (src_w, src_h);
        self.encoder_error = None;
        Ok(())
    }

    fn report_error(&mut self, msg: String) {
        if self.encoder_error.as_deref() != Some(msg.as_str()) {
            self.encoder_error = Some(msg.clone());
            let _ = self.tx.try_send(EncMsg::Error(msg));
        }
    }
}

/// Build the windows-capture Settings for the configured source.
pub fn make_settings(
    cfg: &Config,
    tx: SyncSender<EncMsg>,
    adapter_preference: D3DAdapterPreference,
) -> Result<Settings<Flags, Item>, String> {
    let cursor = if cfg.capture_cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let item = resolve_item(&cfg.source)?;
    let flags = Flags {
        tx,
        cfg: cfg.clone(),
    };
    Ok(Settings::new(
        item,
        cursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    )
    .with_d3d_adapter_preference(adapter_preference))
}

/// The capture item can be a monitor or a window; keep them in one type that
/// windows-capture accepts.
pub enum Item {
    Monitor(Monitor),
    Window(Window),
}

impl TryInto<windows_capture::settings::GraphicsCaptureItemType> for Item {
    type Error = windows::core::Error;
    fn try_into(self) -> Result<windows_capture::settings::GraphicsCaptureItemType, Self::Error> {
        match self {
            Item::Monitor(m) => m.try_into(),
            Item::Window(w) => w.try_into(),
        }
    }
}

pub fn resolve_item(source: &CaptureSource) -> Result<Item, String> {
    match source {
        // Auto means the primary monitor. Grabbing a large foreground window
        // too easily captures a browser, and window capture includes the title
        // bar; explicit window selection remains available above.
        CaptureSource::Auto => Monitor::primary()
            .map(Item::Monitor)
            .map_err(|e| format!("no primary monitor: {e}")),
        CaptureSource::Monitor(n) => Monitor::from_index(*n as usize)
            .map(Item::Monitor)
            .map_err(|e| format!("monitor {n} not found: {e}")),
        CaptureSource::Window(hwnd) => {
            let w = Window::from_raw_hwnd(*hwnd as *mut core::ffi::c_void);
            Ok(Item::Window(w))
        }
    }
}
