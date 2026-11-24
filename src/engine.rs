//! The engine thread owns everything stateful: capture control, the packet
//! ring, the manual-recording muxer and the stats. The UI only sends commands
//! and reads a snapshot, so a slow UI can never stall capture.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::w;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_RESTORE,
};
use windows_capture::capture::GraphicsCaptureApiHandler;
use windows_capture::settings::D3DAdapterPreference;

use crate::capture::{self, CaptureHandler, EncMsg};
use crate::config::Config;
use crate::muxer::Mp4Muxer;
use crate::ring::PacketRing;
use crate::types::{ClipInfo, CodecInfo, Command, Packet, SessionState, UiSnapshot};

/// Shared with the UI and tray threads.
pub struct AppShared {
    pub ui: Mutex<UiSnapshot>,
    pub ctx: Mutex<Option<egui::Context>>,
    pub show_requested: AtomicBool,
    pub hide_requested: AtomicBool,
    pub quit_requested: AtomicBool,
    pub badge_generation: AtomicU64,
    pub badge_kind: AtomicU8,
    pub toast_sound: AtomicBool,
    pub toast_stop: AtomicBool,
}

impl AppShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ui: Mutex::new(UiSnapshot::default()),
            ctx: Mutex::new(None),
            show_requested: AtomicBool::new(false),
            hide_requested: AtomicBool::new(false),
            quit_requested: AtomicBool::new(false),
            badge_generation: AtomicU64::new(0),
            badge_kind: AtomicU8::new(1),
            toast_sound: AtomicBool::new(false),
            toast_stop: AtomicBool::new(false),
        })
    }

    /// Ask the egui window to repaint on the next frame, if it is open.
    pub fn repaint(&self) {
        if let Ok(guard) = self.ctx.lock() {
            if let Some(ctx) = guard.as_ref() {
                ctx.request_repaint();
            }
        }
    }

    /// Show and focus the settings window. The native restore is needed because
    /// a hidden eframe viewport may not process repaint requests while hidden.
    pub fn request_show_settings(&self) {
        self.hide_requested.store(false, Ordering::Release);
        self.show_requested.store(true, Ordering::Release);
        self.repaint();
        unsafe {
            let hwnd = FindWindowW(None, w!("Cliffy")).unwrap_or_default();
            if !hwnd.is_invalid() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
        }
    }

    /// Toggle the settings window from a tray click.
    pub fn request_toggle_settings(&self) {
        unsafe {
            let hwnd = FindWindowW(None, w!("Cliffy")).unwrap_or_default();
            if !hwnd.is_invalid() && IsWindowVisible(hwnd).as_bool() {
                self.show_requested.store(false, Ordering::Release);
                self.hide_requested.store(true, Ordering::Release);
                self.repaint();
                return;
            }
        }
        self.request_show_settings();
    }

    pub fn set_message(&self, text: impl Into<String>) {
        if let Ok(mut ui) = self.ui.lock() {
            ui.message = text.into();
        }
        self.repaint();
    }

    pub fn set_error(&self, text: impl Into<String>) {
        if let Ok(mut ui) = self.ui.lock() {
            ui.last_error = text.into();
        }
        self.repaint();
    }

    /// Ask the badge overlay to briefly confirm a saved clip.
    pub fn request_clip_saved_badge(&self) {
        self.request_badge(1, true);
    }

    /// Ask the badge overlay to briefly confirm a completed recording.
    pub fn request_recording_saved_badge(&self) {
        self.request_badge(2, true);
    }

    /// Ask the badge overlay to confirm a buffer state change without sound.
    pub fn request_buffer_badge(&self, enabled: bool) {
        self.request_badge(if enabled { 3 } else { 4 }, false);
    }

    fn request_badge(&self, kind: u8, sound: bool) {
        self.badge_kind.store(kind, Ordering::Release);
        if sound {
            self.toast_sound.store(true, Ordering::Release);
        }
        let _ = self.badge_generation.fetch_add(1, Ordering::Release);
    }
}

pub struct EngineHandle {
    pub cmd: Sender<Command>,
    pub shared: Arc<AppShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl EngineHandle {
    pub fn shutdown(&mut self) {
        let _ = self.cmd.send(Command::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        let _ = self.cmd.send(Command::Shutdown);
    }
}

type CaptureControl = windows_capture::capture::CaptureControl<
    CaptureHandler,
    Box<dyn std::error::Error + Send + Sync>,
>;

struct PendingReplay {
    deadline: Instant,
    clip_seconds: u32,
    extra_seconds: u32,
}

pub fn spawn(cfg: Config, shared: Arc<AppShared>) -> EngineHandle {
    let (tx, rx) = mpsc::channel();
    let thread_shared = shared.clone();
    let join = std::thread::Builder::new()
        .name("Cliffy-engine".into())
        .spawn(move || {
            let mut engine = Engine::new(cfg, thread_shared, rx);
            engine.run();
        })
        .expect("spawn engine thread");
    EngineHandle {
        cmd: tx,
        shared,
        join: Some(join),
    }
}

struct Engine {
    cfg: Config,
    shared: Arc<AppShared>,
    cmd_rx: Receiver<Command>,
    control: Option<CaptureControl>,
    enc_rx: Option<Receiver<EncMsg>>,
    ring: PacketRing,
    pending_replays: VecDeque<PendingReplay>,
    info: Option<CodecInfo>,
    state: SessionState,
    recording: bool,
    buffering: bool,
    recorder: Option<Mp4Muxer>,
    pending_packet: Option<Packet>,
    record_path: Option<PathBuf>,
    record_started: Option<Instant>,
    record_base_pts: i64,
    encoder_name: String,
    dropped_frames: u64,
    dropped_packets: u64,
    fps_counter: u32,
    fps_window: Instant,
    fps: f32,
    clips: Vec<ClipInfo>,
    last_clips_scan: Instant,
    proc_sample: Option<(Instant, u64)>,
    restart_at: Option<Instant>,
    adapter_preference: D3DAdapterPreference,
    adapter_fallback_attempted: bool,
    resume_recording_after_fallback: bool,
    audio_capture: Arc<crate::audio::WasapiCapture>,
}

impl Engine {
    fn new(cfg: Config, shared: Arc<AppShared>, cmd_rx: Receiver<Command>) -> Self {
        let ring = PacketRing::new(replay_window_seconds(&cfg));
        let buffering = cfg.buffer_on_start;
        let mut shared_ui = shared.ui.lock().unwrap();
        shared_ui.estimate_bytes_per_min = cfg.estimated_bytes_per_minute();
        drop(shared_ui);
        let audio_capture = Arc::new(crate::audio::WasapiCapture::new(600));
        audio_capture.start();
        Self {
            cfg,
            shared,
            cmd_rx,
            control: None,
            enc_rx: None,
            ring,
            pending_replays: VecDeque::new(),
            info: None,
            state: SessionState::Idle,
            recording: false,
            buffering,
            recorder: None,
            pending_packet: None,
            record_path: None,
            record_started: None,
            record_base_pts: 0,
            encoder_name: "not started".into(),
            dropped_frames: 0,
            dropped_packets: 0,
            fps_counter: 0,
            fps_window: Instant::now(),
            fps: 0.0,
            clips: Vec::new(),
            last_clips_scan: Instant::now() - Duration::from_secs(10),
            proc_sample: None,
            restart_at: None,
            adapter_preference: D3DAdapterPreference::BestAvailable,
            adapter_fallback_attempted: false,
            resume_recording_after_fallback: false,
            audio_capture,
        }
    }

    fn run(&mut self) {
        if self.cfg.buffer_on_start {
            if let Err(e) = self.start_session() {
                self.shared.set_error(e);
            }
        }
        self.scan_clips();
        loop {
            match self.cmd_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(cmd) => self.handle(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.drain_packets();
            self.tick();

            if self.shared.quit_requested.load(Ordering::Relaxed) {
                break;
            }
        }
        self.stop_recording(false);
        self.stop_session();
        // Flush stats one last time.
        self.publish();
    }

    fn take_action_sound(&self) -> bool {
        self.shared.toast_sound.swap(false, Ordering::Relaxed)
    }

    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::StartSession => {
                let sound = self.take_action_sound();
                let was_buffering = self.buffering;
                if !was_buffering {
                    if let Err(e) = self.start_session() {
                        self.shared.set_error(e);
                    }
                }
                if self.buffering && !was_buffering {
                    self.shared.request_buffer_badge(true);
                    crate::toast::splash("BUFFER STARTED", sound);
                }
            }
            Command::StopSession => {
                let sound = self.take_action_sound();
                let was_buffering = self.buffering;
                self.stop_session();
                if was_buffering {
                    self.shared.request_buffer_badge(false);
                    crate::toast::splash("BUFFER STOPPED", sound);
                }
            }
            Command::SaveReplay => self.request_replay_save(),
            Command::ToggleRecording => {
                if self.recording {
                    self.stop_recording(true);
                } else {
                    let sound = self.take_action_sound();
                    self.start_recording();
                    if self.recording {
                        crate::toast::splash("RECORDING STARTED", sound);
                        crate::toast::set_recording_overlay(true);
                    }
                }
            }
            Command::StartRecording => {
                let sound = self.take_action_sound();
                self.start_recording();
                if self.recording {
                    crate::toast::splash("RECORDING STARTED", sound);
                    crate::toast::set_recording_overlay(true);
                }
            }
            Command::StopRecording => self.stop_recording(true),
            Command::UpdateConfig(new_cfg) => {
                self.apply_config(*new_cfg);
            }
            Command::RefreshClips => self.scan_clips(),
            Command::Shutdown => {
                self.shared.quit_requested.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Start the shared capture pipeline if it is not already running. Recording
    /// and buffering both feed from this one pipeline, so neither may tear it
    /// down while the other still needs packets.
    fn ensure_capture(&mut self) -> Result<(), String> {
        if self.control.is_some() {
            return Ok(());
        }
        let (tx, rx) = mpsc::sync_channel::<EncMsg>(240);
        let settings = capture::make_settings(&self.cfg, tx, self.adapter_preference)?;
        let control = CaptureHandler::start_free_threaded(settings)
            .map_err(|e| format!("Could not start capture: {e}"))?;
        self.control = Some(control);
        self.enc_rx = Some(rx);
        self.info = None;
        self.encoder_name = "starting".into();
        self.shared.set_error("");
        self.publish();
        Ok(())
    }

    fn start_session(&mut self) -> Result<(), String> {
        if self.cfg.buffer_only_on_ac && !crate::config::is_ac_power_connected() {
            self.buffering = false;
            self.state = SessionState::Idle;
            self.shared.set_message("Buffering paused: on battery");
            self.publish();
            return Ok(());
        }
        if let Err(e) = self.ensure_capture() {
            self.buffering = false;
            self.publish();
            return Err(e);
        }
        self.buffering = true;
        self.ring = PacketRing::new(replay_window_seconds(&self.cfg));
        self.state = SessionState::Buffering;
        self.shared.set_message("Buffering");
        self.publish();
        Ok(())
    }

    fn stop_session(&mut self) {
        self.buffering = false;
        if !self.pending_replays.is_empty() {
            self.pending_replays.clear();
            self.shared
                .set_error("Delayed clip cancelled because buffering stopped");
        }
        // An active recording still needs the capture thread and its info.
        if !self.recording {
            if let Some(c) = self.control.take() {
                let _ = c.stop();
            }
            self.enc_rx = None;
            self.info = None;
            self.pending_packet = None;
        }
        self.state = if self.recording {
            SessionState::Recording
        } else {
            SessionState::Idle
        };
        self.ring.clear();
        self.shared.set_message("Stopped");
        self.publish();
        trim_working_set();
    }

    fn drain_packets(&mut self) {
        enum Next {
            Message(EncMsg),
            Empty,
            Disconnected,
        }
        loop {
            // End the immutable borrow of self.enc_rx before calling on_msg.
            let next = match self.enc_rx.as_ref() {
                None => return,
                Some(rx) => match rx.try_recv() {
                    Ok(msg) => Next::Message(msg),
                    Err(mpsc::TryRecvError::Empty) => Next::Empty,
                    Err(mpsc::TryRecvError::Disconnected) => Next::Disconnected,
                },
            };
            match next {
                Next::Message(msg) => self.on_msg(msg),
                Next::Empty => break,
                Next::Disconnected => {
                    // Capture thread ended. Keep the session marked idle so the
                    // user can restart; do not panic.
                    self.enc_rx = None;
                    self.control = None;
                    self.buffering = false;
                    if self.recording {
                        self.stop_recording(false);
                    }
                    self.ring.clear();
                    self.state = SessionState::Idle;
                    self.shared.set_message("Capture ended");
                    self.publish();
                    break;
                }
            }
        }
    }

    fn on_msg(&mut self, msg: EncMsg) {
        match msg {
            EncMsg::Config(info) => {
                self.info = Some(info);
                if self.recording && self.recorder.is_none() {
                    if let Err(e) = self.open_recorder() {
                        self.shared.set_error(e);
                    }
                }
            }
            EncMsg::Encoder(name) => {
                self.encoder_name = name.clone();
                self.shared.set_message(format!("Encoder: {name}"));
            }
            EncMsg::EncoderUnavailable(error) => self.fallback_encoder_adapter(error),
            EncMsg::Packets(packets) => {
                for incoming in packets {
                    if let Some(mut pending) = self.pending_packet.take() {
                        let gap = incoming.pts_us - pending.pts_us;
                        pending.dur_us = if gap > 0 {
                            gap.min(1_000_000) as u32
                        } else {
                            self.nominal_dur_us()
                        };
                        self.accept_packet(pending);
                    }
                    self.pending_packet = Some(incoming);
                }
            }
            EncMsg::Error(e) => {
                self.shared.set_error(e.clone());
                // GPU driver reset / capture item closed: schedule a restart
                // instead of dying.
                if e.to_lowercase().contains("closed") || e.to_lowercase().contains("reset") {
                    self.restart_at = Some(Instant::now() + Duration::from_secs(2));
                }
            }
            EncMsg::Dropped(n) => self.dropped_frames = self.dropped_frames.saturating_add(n),
        }
    }

    fn fallback_encoder_adapter(&mut self, error: String) {
        if self.adapter_preference != D3DAdapterPreference::BestAvailable
            || self.adapter_fallback_attempted
        {
            self.shared.set_error(error);
            return;
        }

        self.adapter_fallback_attempted = true;
        self.adapter_preference = D3DAdapterPreference::Intel;
        self.shared
            .set_message("NVIDIA encoder unavailable; trying Intel Quick Sync");

        self.resume_recording_after_fallback = self.recording;
        if self.recording {
            self.stop_recording(false);
        }
        if !self.pending_replays.is_empty() {
            self.pending_replays.clear();
            self.shared
                .set_error("Pending delayed clips were cancelled while switching encoders");
        }
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
        self.enc_rx = None;
        self.info = None;
        self.pending_packet = None;
        self.ring.clear();
        self.restart_at = Some(Instant::now() + Duration::from_millis(500));
        self.publish();
    }

    fn nominal_dur_us(&self) -> u32 {
        let fps = self.info.as_ref().map(|i| i.fps).unwrap_or(self.cfg.fps);
        (1_000_000 / fps.max(1)).max(1)
    }

    fn accept_packet(&mut self, p: Packet) {
        if p.dur_us == 0 {
            return;
        }
        self.fps_counter = self.fps_counter.saturating_add(1);
        if self.recording {
            if self.record_base_pts == i64::MIN {
                if !p.keyframe {
                    // Do not start recording on a non-keyframe: wait for an IDR keyframe
                    // so the MP4 starts cleanly without green frames or missing references.
                    if self.buffering {
                        self.ring.push(p);
                    }
                    return;
                }
                self.record_base_pts = p.pts_us;
            }
            if self.recorder.is_none() && self.info.is_some() {
                if let Err(e) = self.open_recorder() {
                    self.shared.set_error(e);
                    self.recording = false;
                }
            }
            if let Some(rec) = self.recorder.as_mut() {
                if let Err(e) = rec.write(&p, self.record_base_pts) {
                    self.shared.set_error(format!("write failed: {e}"));
                    self.stop_recording(false);
                }
            }
        }
        if self.buffering {
            self.ring.push(p);
        }
    }

    fn start_recording(&mut self) {
        if self.control.is_none() {
            if let Err(e) = self.ensure_capture() {
                self.shared.set_error(e);
                return;
            }
        }
        self.recording = true;
        self.record_base_pts = i64::MIN;
        self.record_started = Some(Instant::now());
        self.state = SessionState::Recording;
        self.shared.set_message("Recording");
        if self.info.is_some() {
            if let Err(e) = self.open_recorder() {
                self.shared.set_error(e);
                self.recording = false;
            }
        }
        self.publish();
    }

    fn open_recorder(&mut self) -> Result<(), String> {
        let info = self.info.clone().ok_or("no encoder info yet")?;
        let dir = PathBuf::from(&self.cfg.output_dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("output folder: {e}"))?;
        let stamp = local_timestamp();
        let path = dir.join(format!("Recording_{stamp}.mp4"));
        let muxer = Mp4Muxer::create(&path, &info)?;
        self.recorder = Some(muxer);
        self.record_path = Some(path);
        Ok(())
    }

    fn stop_recording(&mut self, notify: bool) {
        if !self.recording && self.recorder.is_none() {
            return;
        }
        crate::toast::set_recording_overlay(false);
        let elapsed = self
            .record_started
            .map(|t| t.elapsed().as_secs_f32().ceil().max(1.0) as u32)
            .unwrap_or(1);
        let show = self.cfg.show_toast && notify;
        // Flush while `recording` is still true so the last packet is written
        // to the muxer, and to the ring when buffering is active.
        if let Some(mut p) = self.pending_packet.take() {
            p.dur_us = self.nominal_dur_us();
            self.accept_packet(p);
        }
        self.recording = false;
        if self.record_base_pts == i64::MIN {
            // No keyframe was accepted yet, so nothing was recorded
            let _ = self.recorder.take();
            if let Some(p) = self.record_path.take() {
                let _ = std::fs::remove_file(p);
            }
            self.shared
                .set_message(String::from("Recording cancelled: no frames captured"));
        } else if let Some(m) = self.recorder.take() {
            let path = self.record_path.take();
            let audio_bytes = self
                .audio_capture
                .ring()
                .snapshot_last_seconds(elapsed as usize);
            match m.finish() {
                Ok(()) => {
                    if let Some(p) = path {
                        match std::fs::metadata(&p) {
                            Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {
                                let p_mux = p.clone();
                                std::thread::spawn(move || {
                                    if !audio_bytes.is_empty() {
                                        let wav_path = temp_wav_path();
                                        if crate::audio::write_float_wav(
                                            &wav_path,
                                            &audio_bytes,
                                            48000,
                                            2,
                                        )
                                        .is_ok()
                                        {
                                            let _ = crate::audio::mux_audio_into_video(
                                                &p_mux, &wav_path,
                                            );
                                        }
                                        let _ = std::fs::remove_file(&wav_path);
                                    }
                                });
                                self.shared
                                    .set_message(format!("Recording saved: {}", p.display()));
                                self.shared.ui.lock().unwrap().last_saved = Some(p);
                                if show {
                                    self.shared.request_recording_saved_badge();
                                }
                            }
                            Ok(_) => self.shared.set_error("Recording file is empty"),
                            Err(e) => self
                                .shared
                                .set_error(format!("Recording file was not created: {e}")),
                        }
                    }
                    self.scan_clips();
                }
                Err(e) => self.shared.set_error(format!("finalize failed: {e}")),
            }
        }
        trim_working_set();
        self.record_started = None;
        if !self.buffering {
            // No one needs frames any more, so the shared pipeline can stop.
            if let Some(c) = self.control.take() {
                let _ = c.stop();
            }
            self.enc_rx = None;
            self.info = None;
            self.pending_packet = None;
            self.ring.clear();
        }
        self.state = if self.buffering {
            SessionState::Buffering
        } else {
            SessionState::Idle
        };
        self.publish();
    }

    fn request_replay_save(&mut self) {
        let delay = self.cfg.delayed_stop_seconds;
        let clip_seconds = self.cfg.replay_seconds.saturating_add(delay);
        if delay == 0 {
            self.save_replay(clip_seconds, delay);
            return;
        }
        if !self.buffering {
            self.shared
                .set_error("Start the replay buffer before saving a delayed clip");
            return;
        }
        self.pending_replays.push_back(PendingReplay {
            deadline: Instant::now() + Duration::from_secs(delay as u64),
            clip_seconds,
            extra_seconds: delay,
        });
        self.shared
            .set_message(format!("Saving clip in {delay} seconds..."));
    }

    fn save_replay(&mut self, clip_seconds: u32, extra_seconds: u32) {
        let Some(info) = self.info.clone() else {
            self.shared.set_error("Nothing buffered yet");
            return;
        };
        let packets = self.ring.snapshot_last(clip_seconds);
        if packets.is_empty() {
            self.shared.set_error("Replay buffer is empty");
            return;
        }
        let mut packets = packets;
        retime(&mut packets);
        let dir = PathBuf::from(&self.cfg.output_dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.shared.set_error(format!("output folder: {e}"));
            return;
        }
        let path = dir.join(format!(
            "Clip_{}_extra{}sec.mp4",
            local_timestamp(),
            extra_seconds
        ));
        let base = packets[0].pts_us;
        let show_toast = self.cfg.show_toast;
        let seconds = clip_seconds;
        let audio_bytes = self
            .audio_capture
            .ring()
            .snapshot_last_seconds(seconds as usize);

        {
            let mut ui = self.shared.ui.lock().unwrap_or_else(|e| e.into_inner());
            ui.save_in_progress = ui.save_in_progress.saturating_add(1);
            ui.message = "Saving clip...".into();
        }
        self.shared.repaint();

        let shared = self.shared.clone();
        let path_for_thread = path.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("Cliffy-save".into())
            .spawn(move || {
                let result = (|| -> Result<(), String> {
                    let mut muxer = Mp4Muxer::create(&path_for_thread, &info)?;
                    for p in &packets {
                        muxer.write(p, base)?;
                    }
                    muxer.finish()?;
                    if !audio_bytes.is_empty() {
                        let wav_path = temp_wav_path();
                        if crate::audio::write_float_wav(&wav_path, &audio_bytes, 48000, 2).is_ok()
                        {
                            let _ = crate::audio::mux_audio_into_video(&path_for_thread, &wav_path);
                        }
                        let _ = std::fs::remove_file(&wav_path);
                    }
                    let metadata = std::fs::metadata(&path_for_thread)
                        .map_err(|e| format!("clip was not created: {e}"))?;
                    if !metadata.is_file() || metadata.len() == 0 {
                        return Err("clip file is empty".into());
                    }
                    Ok(())
                })();
                let succeeded = result.is_ok();
                let mut ui = shared.ui.lock().unwrap_or_else(|e| e.into_inner());
                ui.save_in_progress = ui.save_in_progress.saturating_sub(1);
                match result {
                    Ok(()) => {
                        ui.last_saved = Some(path_for_thread.clone());
                        ui.message = format!(
                            "Saved {} ({:.1} MB)",
                            path_for_thread.display(),
                            file_size(&path_for_thread) as f64 / (1024.0 * 1024.0)
                        );
                        if let Ok(entries) = std::fs::read_dir(
                            path_for_thread
                                .parent()
                                .unwrap_or(std::path::Path::new(".")),
                        ) {
                            let mut clips = Vec::new();
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.extension().map(|e| e == "mp4").unwrap_or(false) {
                                    if let Ok(meta) = entry.metadata() {
                                        clips.push(crate::types::ClipInfo {
                                            name: file_name(&p),
                                            path: p,
                                            size_bytes: meta.len(),
                                            modified: meta.modified().ok(),
                                        });
                                    }
                                }
                            }
                            clips.sort_by(|a, b| b.modified.cmp(&a.modified));
                            ui.clips = clips;
                        }
                    }
                    Err(e) => {
                        ui.last_error = format!("Save failed: {e}");
                        let _ = std::fs::remove_file(&path_for_thread);
                    }
                }
                drop(ui);
                shared.repaint();
                if show_toast && succeeded {
                    shared.request_clip_saved_badge();
                }
            })
        {
            let mut ui = self.shared.ui.lock().unwrap_or_else(|e| e.into_inner());
            ui.save_in_progress = ui.save_in_progress.saturating_sub(1);
            ui.last_error = format!("Could not start save thread: {e}");
            drop(ui);
            self.shared.repaint();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        while self
            .pending_replays
            .front()
            .is_some_and(|pending| pending.deadline <= now)
        {
            if let Some(pending) = self.pending_replays.pop_front() {
                self.save_replay(pending.clip_seconds, pending.extra_seconds);
            }
        }

        // Auto-restart after a driver reset / closed item.
        if let Some(when) = self.restart_at {
            if Instant::now() >= when {
                self.restart_at = None;
                if self.control.is_none()
                    && (self.recording || self.buffering || self.resume_recording_after_fallback)
                {
                    let result = if self.buffering {
                        self.start_session()
                    } else {
                        // Recording-only: restore capture without enabling the ring.
                        self.ensure_capture()
                    };
                    if let Err(e) = result {
                        self.resume_recording_after_fallback = false;
                        self.shared.set_error(e);
                    } else if self.resume_recording_after_fallback {
                        self.resume_recording_after_fallback = false;
                        self.start_recording();
                    }
                }
            }
        }

        if self.fps_window.elapsed() >= Duration::from_secs(1) {
            self.fps = self.fps_counter as f32 / self.fps_window.elapsed().as_secs_f32();
            self.fps_window = Instant::now();
            self.fps_counter = 0;
        }

        self.publish();

        if self.cfg.buffer_only_on_ac {
            let ac = crate::config::is_ac_power_connected();
            if self.buffering && !ac {
                self.stop_session();
                self.shared.set_message("Buffering paused: on battery");
            } else if !self.buffering && ac && self.cfg.buffer_on_start {
                // Capture may still be alive for recording; ensure_capture reuses it.
                let _ = self.start_session();
            }
        }

        if self.last_clips_scan.elapsed() >= Duration::from_secs(3) {
            self.scan_clips();
            if !self.recording && !self.buffering {
                trim_working_set();
            }
        }
    }

    fn apply_config(&mut self, mut new_cfg: Config) {
        new_cfg.sanitize();
        let video_changed = self.cfg.resolution != new_cfg.resolution
            || self.cfg.custom_width != new_cfg.custom_width
            || self.cfg.custom_height != new_cfg.custom_height
            || self.cfg.fps != new_cfg.fps
            || self.cfg.bitrate_mbps != new_cfg.bitrate_mbps
            || self.cfg.rate_control != new_cfg.rate_control
            || self.cfg.quality != new_cfg.quality
            || self.cfg.codec != new_cfg.codec
            || self.cfg.source != new_cfg.source
            || self.cfg.capture_cursor != new_cfg.capture_cursor;
        let was_buffering = self.buffering;
        let was_recording = self.recording;
        let restart_pipeline = video_changed && self.control.is_some();

        self.cfg = new_cfg;
        self.ring.set_duration(replay_window_seconds(&self.cfg));
        self.shared.ui.lock().unwrap().estimate_bytes_per_min =
            self.cfg.estimated_bytes_per_minute();

        if restart_pipeline {
            if was_recording {
                self.stop_recording(false);
            }
            if was_buffering {
                self.stop_session();
                if let Err(e) = self.start_session() {
                    self.shared.set_error(e);
                    return;
                }
                if was_recording {
                    self.start_recording();
                }
            } else if was_recording {
                if let Err(e) = self.ensure_capture() {
                    self.shared.set_error(e);
                    return;
                }
                self.start_recording();
            }
            self.shared
                .set_message("Video settings applied; capture restarted");
        } else {
            self.shared.set_message("Settings applied");
        }
        self.scan_clips();
    }

    fn publish(&mut self) {
        self.state = if self.recording {
            SessionState::Recording
        } else if self.buffering {
            SessionState::Buffering
        } else {
            SessionState::Idle
        };
        let (mem_mb, cpu) = process_stats(&mut self.proc_sample);
        let mut ui = self.shared.ui.lock().unwrap();
        ui.state = self.state;
        ui.recording_active = self.recording;
        ui.buffering_active = self.buffering;
        ui.encoder_name = self.encoder_name.clone();
        if let Some(info) = &self.info {
            ui.width = info.width;
            ui.height = info.height;
        }
        ui.fps = self.fps;
        ui.dropped_frames = self.dropped_frames;
        ui.dropped_packets = self.dropped_packets;
        ui.buffer_seconds = self.ring.duration_us() as f32 / 1_000_000.0;
        ui.buffer_bytes = self.ring.bytes();
        ui.record_seconds = self
            .record_started
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        ui.memory_mb = mem_mb;
        ui.cpu_percent = cpu;
        ui.estimate_bytes_per_min = self.cfg.estimated_bytes_per_minute();
        drop(ui);
    }

    fn scan_clips(&mut self) {
        self.last_clips_scan = Instant::now();
        let dir = PathBuf::from(&self.cfg.output_dir);
        let mut clips = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for entry in rd.flatten() {
                let path = entry.path();
                if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("mp4"))
                    != Some(true)
                {
                    continue;
                }
                let meta = entry.metadata().ok();
                clips.push(ClipInfo {
                    name: file_name(&path),
                    path,
                    size_bytes: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                    modified: meta.and_then(|m| m.modified().ok()),
                });
            }
        }
        clips.sort_by(|a, b| b.modified.cmp(&a.modified));
        self.clips = clips;
        self.shared.ui.lock().unwrap().clips = self.clips.clone();
    }
}

/// Give each packet the real gap to the next one. WGC is change driven, so
/// frame times are not perfectly uniform; using the gap keeps playback in real
/// time instead of drifting.
fn retime(packets: &mut [Packet]) {
    for i in 0..packets.len().saturating_sub(1) {
        let d = packets[i + 1].pts_us - packets[i].pts_us;
        if d > 0 {
            packets[i].dur_us = d.min(1_000_000) as u32;
        }
    }
}

/// Scratch WAVs belong in the temp folder, never beside the saved clips.
fn temp_wav_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("cliffy_audio_{}_{}.wav", std::process::id(), nanos))
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

fn replay_window_seconds(cfg: &Config) -> u32 {
    cfg.replay_seconds.saturating_add(cfg.delayed_stop_seconds)
}

fn local_timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let _ = EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Process CPU percent (whole machine) and working set in MB.
fn process_stats(sample: &mut Option<(Instant, u64)>) -> (f32, f32) {
    unsafe {
        let mut pmc = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        let handle = GetCurrentProcess();
        let mem_mb = if K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb).as_bool() {
            pmc.WorkingSetSize as f32 / (1024.0 * 1024.0)
        } else {
            0.0
        };

        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_err() {
            return (mem_mb, 0.0);
        }
        let ticks = filetime_ticks(kernel) + filetime_ticks(user);
        let now = Instant::now();
        let cpu = match sample.replace((now, ticks)) {
            Some((prev_at, prev_ticks)) => {
                let wall = now.duration_since(prev_at).as_secs_f64();
                if wall > 0.0 {
                    let cores = std::thread::available_parallelism()
                        .map(|n| n.get() as f64)
                        .unwrap_or(1.0);
                    ((ticks.saturating_sub(prev_ticks)) as f64 / 10_000_000.0) / wall / cores
                        * 100.0
                } else {
                    0.0
                }
            }
            None => 0.0,
        };
        (mem_mb, cpu as f32)
    }
}

fn filetime_ticks(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

/// Used by --selftest: run the engine without UI, save one clip, exit.
pub fn selftest(cfg: Config, seconds: u64, out_dir: PathBuf) -> Result<PathBuf, String> {
    let shared = AppShared::new();
    let mut cfg = cfg;
    cfg.output_dir = out_dir.to_string_lossy().to_string();
    cfg.buffer_on_start = true;
    cfg.replay_seconds = cfg.replay_seconds.max(5);
    let mut handle = spawn(cfg, shared.clone());
    {
        let mut ui = shared.ui.lock().unwrap();
        ui.last_saved = None;
        ui.last_error.clear();
        ui.message.clear();
    }

    // Give the encoder time to come up and fill the buffer.
    std::thread::sleep(Duration::from_secs(seconds));
    {
        let ui = shared.ui.lock().unwrap();
        if !ui.last_error.is_empty() {
            let err = ui.last_error.clone();
            drop(ui);
            handle.shutdown();
            return Err(err);
        }
    }
    let _ = handle.cmd.send(Command::SaveReplay);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        {
            let ui = shared.ui.lock().unwrap();
            if let Some(p) = &ui.last_saved {
                let path = p.clone();
                let exists = path.exists();
                if exists {
                    drop(ui);
                    handle.shutdown();
                    return Ok(path);
                }
            }
            if !ui.last_error.is_empty() && ui.save_in_progress == 0 {
                let err = ui.last_error.clone();
                drop(ui);
                handle.shutdown();
                return Err(err);
            }
        }
        if Instant::now() > deadline {
            handle.shutdown();
            return Err("timed out waiting for clip save".into());
        }
    }
}

// Silence "field never read" until stage 3 (trim) uses it.
#[allow(dead_code)]
fn _touch(_: &Packet) {}
