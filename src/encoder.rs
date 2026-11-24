//! Media Foundation hardware H.264 encoder wrapper.
//!
//! Frames arrive as D3D11 NV12 textures and are wrapped in IMFSamples with
//! MFCreateDXGISurfaceBuffer. No CPU copy of the picture ever happens: the
//! encoder MFT reads the texture through the DXGI device manager.
//!
//! Handles both synchronous and asynchronous MFTs. Hardware encoders (NVENC,
//! AMF, Quick Sync) are usually asynchronous: they send METransformNeedInput
//! when they want a frame and METransformHaveOutput when a packet is ready.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Instant;

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use crate::config::{Codec, Preset, RateControl};
use crate::types::{CodecInfo, Packet, VideoSettings};

// Codec API GUIDs. Defined here instead of relying on generated names so the
// file keeps working across windows-rs releases.
const CODECAPI_AVENCCOMMONRATECONTROLMODE: GUID =
    GUID::from_u128(0x1c0608e9_370c_4710_8a58_cb6181c42423);
const CODECAPI_AVENCCOMMONMEANBITRATE: GUID =
    GUID::from_u128(0xf7222374_2144_4815_b550_a37f8e12ee52);
const CODECAPI_AVENCCOMMONMAXBITRATE: GUID =
    GUID::from_u128(0x9651eae4_39b9_4ebf_85ef_d7f444ec7465);
const CODECAPI_AVENCCOMMONQUALITY: GUID = GUID::from_u128(0xfcbf57a3_7ea5_4b0c_9644_69b40c39c391);
const CODECAPI_AVENCCOMMONQUALITYVSSPEED: GUID =
    GUID::from_u128(0x98332df8_03cd_476b_89fa_3e9b2e0d8ef5);
const CODECAPI_AVENCCOMMONLOWLATENCY: GUID =
    GUID::from_u128(0x9d3ecd55_89e8_490a_970a_0c9548d5a56e);
const CODECAPI_AVENCMPVDEFAULTBPICTURECOUNT: GUID =
    GUID::from_u128(0x8d390aac_dc5c_4200_b57f_814d04babab2);
const CODECAPI_AVENCMPVGOPSIZE: GUID = GUID::from_u128(0x95f31b26_95a4_41aa_9303_2f4631a9d3b1);
const CODECAPI_AVENCH264CABACENABLE: GUID = GUID::from_u128(0xee6b3bbf_8627_4d41_a67e_4ebef6057c19);
const CODECAPI_AVENCVIDEOFORCEKEYFRAME: GUID =
    GUID::from_u128(0x398c1b98_8353_428d_9ef2_8f265d260345);

const RATE_CONTROL_CBR: u32 = 0;
const RATE_CONTROL_VBR: u32 = 2;
const RATE_CONTROL_QUALITY: u32 = 3;

/// 8 MB is far more than a 4K keyframe needs at sane bitrates. It is the
/// minimum size of a caller-owned output sample; the MFT may declare a larger
/// requirement through GetOutputStreamInfo.
const OUT_BUFFER_BYTES: u32 = 8 * 1024 * 1024;
const MAX_IN_FLIGHT_OUT: usize = 4;

fn debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CLIFFY_DEBUG").is_some())
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("[encoder] {}", format_args!($($arg)*));
        }
    };
}

pub struct MfEncoder {
    transform: IMFTransform,
    // Async MFT events are delivered through IMFMediaEventGenerator, not IMFTransform.
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    async_mode: bool,
    need_input: bool,
    /// Counts submitted inputs that have not produced an output yet.
    inflight_inputs: u32,
    /// Texture-pool slot paired with each submitted input.
    inflight_slots: VecDeque<usize>,
    /// Slots returned to the capture stage after an output arrived.
    released_slots: Vec<usize>,
    free_outputs: Vec<IMFSample>,
    /// The MFT allocates its own output samples; pass pSample=None.
    mft_provides_samples: bool,
    /// Unconsumed METransformHaveOutput credits.
    pending_outputs: u32,
    /// Set when SPS/PPS recovered from an encoded sample changed extradata.
    config_pending: bool,
    pub name: String,
    pub info: CodecInfo,
    pub fps: u32,
    pub width: u32,
    pub height: u32,
    frames_in: u64,
    duplicate_pts: i64,
    last_submit: Instant,
}

impl MfEncoder {
    pub fn new(device: &ID3D11Device, settings: &VideoSettings) -> Result<Self, String> {
        if settings.codec != Codec::H264 {
            return Err("Cliffy v1 supports H.264 only".into());
        }

        // One device manager per encoder instance; it must wrap the same D3D11
        // device the capture frames live on. On Optimus that is the Intel GPU,
        // so the matching hardware encoder is Intel Quick Sync.
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
            .map_err(|e| format!("MFCreateDXGIDeviceManager: {e}"))?;
        let manager = manager.ok_or("device manager null")?;
        unsafe { manager.ResetDevice(device, token) }.map_err(|e| format!("ResetDevice: {e}"))?;

        // H.264 only, hardware only, Quick Sync only for v1. NVENC lives on a
        // different adapter on this laptop and would require a cross-GPU copy.
        let candidates: Vec<IMFActivate> = enum_encoders()
            .into_iter()
            .filter(|a| friendly_name(a).to_ascii_lowercase().contains("quick sync"))
            .collect();
        if candidates.is_empty() {
            return Err(
                "No Intel Quick Sync Video H.264 encoder found. Cliffy v1 requires a hardware Quick Sync encoder on the same GPU as Windows Graphics Capture; NVENC, AMF and software encoding are not used."
                    .into(),
            );
        }

        let mut last_err = String::new();
        for activate in candidates.iter() {
            let name = friendly_name(activate);
            match Self::try_init(
                activate,
                device,
                &manager,
                settings,
                MFVideoFormat_H264,
                name.clone(),
            ) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    last_err = format!("{name}: {e}");
                }
            }
        }
        Err(format!(
            "Quick Sync encoder initialisation failed. Last error: {last_err}"
        ))
    }

    fn try_init(
        activate: &IMFActivate,
        device: &ID3D11Device,
        manager: &IMFDXGIDeviceManager,
        settings: &VideoSettings,
        subtype: GUID,
        name: String,
    ) -> Result<Self, String> {
        unsafe {
            // Async MFTs are locked unless this is set before activation.
            let _ = activate.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            let transform: IMFTransform = activate
                .ActivateObject()
                .map_err(|e| format!("ActivateObject: {e}"))?;

            let attrs = transform
                .GetAttributes()
                .map_err(|e| format!("GetAttributes: {e}"))?;
            let async_mode = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            // The unlock must be applied to the MFT itself; setting it on the
            // IMFActivate alone does not lift MF_E_TRANSFORM_ASYNC_LOCKED.
            if async_mode {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| format!("ASYNC_UNLOCK: {e}"))?;
            }

            // Drive the encoder from the same D3D11 device that owns the WGC
            // texture. The manager must be attached before media types.
            let d3d11_aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0);
            let dev_luid = device_luid(device);
            let mft_luid = encoder_luid(activate);
            let adapter = device_description(device);
            if d3d11_aware != 0 {
                // Some Intel MFTs need the D3D device to be multithread-safe.
                if let Ok(mt) = device.cast::<ID3D10Multithread>() {
                    let _ = mt.SetMultithreadProtected(true);
                }
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .map_err(|e| {
                        format!(
                            "SET_D3D_MANAGER (d3d11_aware={d3d11_aware}, device={adapter}, device_luid={dev_luid:?}, mft_luid={mft_luid:?}): {e}"
                        )
                    })?;
            }

            // Output type: compressed video.
            let out_type = MFCreateMediaType().map_err(|e| e.to_string())?;
            out_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| e.to_string())?;
            out_type
                .SetGUID(&MF_MT_SUBTYPE, &subtype)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps)
                .map_err(|e| e.to_string())?;
            out_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| e.to_string())?;
            // MF packs sizes and ratios into one UINT64: high word / low word.
            out_type
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    packed_word_pair(settings.target_width, settings.target_height),
                )
                .map_err(|e| format!("frame size: {e}"))?;
            out_type
                .SetUINT64(&MF_MT_FRAME_RATE, packed_word_pair(settings.fps, 1))
                .map_err(|e| format!("frame rate: {e}"))?;
            out_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed_word_pair(1, 1))
                .map_err(|e| format!("par: {e}"))?;
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("SetOutputType: {e}"))?;

            // Input type: NV12 progressive, BT.709 limited range (matches shader).
            let in_type = MFCreateMediaType().map_err(|e| e.to_string())?;
            in_type
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| e.to_string())?;
            in_type
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(|e| e.to_string())?;
            in_type
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    packed_word_pair(settings.target_width, settings.target_height),
                )
                .map_err(|e| format!("in frame size: {e}"))?;
            in_type
                .SetUINT64(&MF_MT_FRAME_RATE, packed_word_pair(settings.fps, 1))
                .map_err(|e| format!("in frame rate: {e}"))?;
            in_type
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, packed_word_pair(1, 1))
                .map_err(|e| format!("in par: {e}"))?;
            // MFVideoTransferMatrix_BT709 = 1; BT601 = 2.
            let _ = in_type.SetUINT32(&MF_MT_YUV_MATRIX, 1);
            // MFNominalRange_16_235 = 2; 0_255 = 1.
            let _ = in_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, 2);
            let _ = in_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, settings.target_width);
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("SetInputType: {e}"))?;

            // Query output requirements before streaming starts. Intel Quick
            // Sync sets MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, so ProcessOutput
            // must be called with pSample=None for that stream.
            let output_info = transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("GetOutputStreamInfo: {e}"))?;
            let mft_provides_samples =
                (output_info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let can_provide_samples =
                (output_info.dwFlags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32) != 0;
            let output_buffer_bytes = output_info.cbSize.max(OUT_BUFFER_BYTES);
            debug_log!(
                "output stream info flags={:#x} provides={} can_provide={} cbSize={} cbAlignment={}",
                output_info.dwFlags,
                mft_provides_samples,
                can_provide_samples,
                output_info.cbSize,
                output_info.cbAlignment
            );

            let codec_api: Option<ICodecAPI> = transform.cast().ok();
            // Async MFTs raise input/output events on this interface.
            let events: Option<IMFMediaEventGenerator> = transform.cast().ok();
            if let Some(api) = &codec_api {
                configure_codec(api, settings);
                // Always start on an IDR so a saved clip has a clean entry point.
                let _ = api.SetValue(&CODECAPI_AVENCVIDEOFORCEKEYFRAME, &VARIANT::from(1u32));
            }

            // Pull the SPS/PPS that the MP4 header needs.
            let mut extra = vec![0u8; 4096];
            let mut extra_len = 0u32;
            let _ = out_type.GetBlob(
                &MF_MT_MPEG_SEQUENCE_HEADER,
                &mut extra,
                Some(&mut extra_len),
            );
            let extra_len = (extra_len as usize).min(extra.len());
            extra.truncate(extra_len);
            debug_log!("sequence header blob len={}", extra.len());

            transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(|e| format!("FLUSH: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("BEGIN_STREAMING: {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| format!("START_OF_STREAM: {e}"))?;

            let mut free_outputs = Vec::new();
            if !mft_provides_samples {
                for _ in 0..MAX_IN_FLIGHT_OUT {
                    free_outputs.push(new_output_sample(output_buffer_bytes)?);
                }
            }

            let label = format!("{name} (hardware)");
            let info = CodecInfo {
                width: settings.target_width,
                height: settings.target_height,
                fps: settings.fps,
                codec: settings.codec,
                extradata: extra,
            };

            Ok(Self {
                transform,
                events,
                codec_api,
                async_mode,
                need_input: async_mode,
                inflight_inputs: 0,
                inflight_slots: VecDeque::new(),
                released_slots: Vec::new(),
                free_outputs,
                mft_provides_samples,
                pending_outputs: 0,
                config_pending: false,
                name: label,
                info,
                fps: settings.fps,
                width: settings.target_width,
                height: settings.target_height,
                frames_in: 0,
                duplicate_pts: 0,
                last_submit: Instant::now(),
            })
        }
    }

    /// Whether the encoder wants another frame right now. Async encoders tell
    /// us; sync encoders accept one frame at a time.
    pub fn wants_input(&self) -> bool {
        if self.async_mode {
            self.need_input && self.inflight_inputs < 2
        } else {
            self.inflight_inputs == 0
        }
    }

    /// Drain the MFT event queue. NeedInput grants an input credit; HaveOutput
    /// grants exactly one ProcessOutput call.
    fn pump_events(&mut self) {
        let events = self.events.clone();
        if let Some(events) = events {
            loop {
                let ev = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                let event_type = unsafe { ev.GetType() }.unwrap_or(0);
                debug_log!("async event type={}", event_type);
                match event_type {
                    e if e == METransformNeedInput.0 as u32 => {
                        self.need_input = true;
                    }
                    e if e == METransformHaveOutput.0 as u32 => {
                        self.pending_outputs = self.pending_outputs.saturating_add(1);
                        debug_log!("HaveOutput pending_outputs={}", self.pending_outputs);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Feed one NV12 texture. `slot` identifies the texture-pool entry so it
    /// can be released only after the MFT returns the matching output.
    /// Returns produced packets and slots that are safe to reuse.
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        slot: usize,
        pts_us: i64,
    ) -> Result<(Vec<Packet>, Vec<usize>), String> {
        debug_log!(
            "encode call pts_us={} slot={} async_mode={} need_input={} inflight_inputs={}",
            pts_us,
            slot,
            self.async_mode,
            self.need_input,
            self.inflight_inputs
        );
        let mut packets = Vec::new();
        self.released_slots.clear();

        if self.async_mode {
            // Only a METransformHaveOutput event licenses a ProcessOutput call.
            self.pump_events();
            self.drain_output(&mut packets);

            if self.need_input && self.inflight_inputs < 2 {
                self.submit_input(texture, slot, pts_us)?;
            } else {
                return Err("encoder busy".into());
            }

            self.pump_events();
            self.drain_output(&mut packets);

            if !packets.is_empty() {
                self.need_input = true;
            }
        } else {
            // Synchronous path: drain stale output, then submit.
            self.drain_output(&mut packets);
            self.submit_input(texture, slot, pts_us)?;
            self.drain_output(&mut packets);
        }

        let released = std::mem::take(&mut self.released_slots);
        Ok((packets, released))
    }

    fn submit_input(
        &mut self,
        texture: &ID3D11Texture2D,
        slot: usize,
        pts_us: i64,
    ) -> Result<(), String> {
        // MFCreateDXGISurfaceBuffer requires IID_ID3D11Texture2D, not
        // IID_IDXGISurface, even though the parameter is named punkSurface.
        let buffer = unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false) }
            .map_err(|e| format!("MFCreateDXGISurfaceBuffer: {e}"))?;
        // The surface buffer starts with a zero current length; the MFT needs
        // the full surface size to accept the sample.
        let max_len = unsafe { buffer.GetMaxLength() }.map_err(|e| format!("GetMaxLength: {e}"))?;
        unsafe { buffer.SetCurrentLength(max_len) }
            .map_err(|e| format!("SetCurrentLength: {e}"))?;
        let sample = unsafe { MFCreateSample() }.map_err(|e| format!("MFCreateSample: {e}"))?;
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| format!("AddBuffer: {e}"))?;
        // 100 ns units for Media Foundation. The MFT copies this time to its
        // output sample, so output PTS never depends on a submitted-PTS queue.
        unsafe {
            sample
                .SetSampleTime(pts_us * 10)
                .map_err(|e| e.to_string())?;
            sample
                .SetSampleDuration((1_000_000 / self.fps.max(1)) as i64 * 10)
                .map_err(|e| e.to_string())?;
        }
        unsafe { self.transform.ProcessInput(0, &sample, 0) }
            .map_err(|e| format!("ProcessInput: {e}"))?;
        self.inflight_inputs = self.inflight_inputs.saturating_add(1);
        self.inflight_slots.push_back(slot);
        self.need_input = false;
        self.frames_in += 1;
        self.last_submit = Instant::now();
        debug_log!(
            "submit input ok pts_us={} slot={} frames_in={} inflight_inputs={}",
            pts_us,
            slot,
            self.frames_in,
            self.inflight_inputs
        );
        Ok(())
    }

    fn drain_output(&mut self, out: &mut Vec<Packet>) {
        let packets_before = out.len();
        while self.inflight_inputs > 0 {
            // In async mode each ProcessOutput must be backed by one
            // METransformHaveOutput credit; calling it speculatively yields
            // E_UNEXPECTED on Intel Quick Sync.
            if self.async_mode && self.pending_outputs == 0 {
                break;
            }

            let sample_for_output = if self.mft_provides_samples {
                None
            } else {
                match self.free_outputs.pop() {
                    Some(sample) => Some(sample),
                    // No buffer to receive into: leave the credit untouched so
                    // a later drain can still service this HaveOutput.
                    None => break,
                }
            };
            if self.async_mode {
                self.pending_outputs = self.pending_outputs.saturating_sub(1);
            }

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(sample_for_output),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
            let events = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pEvents) };
            // Any per-output events are intentionally discarded for now.
            drop(events);
            let dw_status = buffers[0].dwStatus;
            let error_code = result.as_ref().err().map(|e| e.code());
            debug_log!(
                "ProcessOutput ok={} error={:?} process_status={} dwStatus={:#x} no_sample={} sample_returned={} packets={} pending={} inflight={}",
                result.is_ok(),
                error_code,
                status,
                dw_status,
                dw_status == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32,
                sample.is_some(),
                out.len(),
                self.pending_outputs,
                self.inflight_inputs
            );

            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let fps = self.fps;
                        if let Some(mut pkt) = sample_to_packet(&sample, fps) {
                            let pts_us = pkt.pts_us;
                            let data_len = pkt.data.len();
                            // Quick Sync may leave MF_MT_MPEG_SEQUENCE_HEADER
                            // empty and emit SPS/PPS in the first sample. Pick
                            // them up before the sample is rewritten to AVCC so
                            // the muxer can still build avcC.
                            if let Some(config) = find_sample_config(&pkt.data) {
                                if config.extradata != self.info.extradata {
                                    for (index, nal_type, len) in &config.nals {
                                        debug_log!(
                                            "SPS/PPS sample NAL index={} type={} name={} len={}",
                                            index,
                                            nal_type,
                                            nal_name(*nal_type),
                                            len
                                        );
                                    }
                                    debug_log!(
                                        "built extradata sps_len={} pps_len={} extradata_len={}",
                                        config.sps_len,
                                        config.pps_len,
                                        config.extradata.len()
                                    );
                                    self.info.extradata = config.extradata;
                                    self.config_pending = true;
                                }
                            }
                            // Normalise to AVCC for the muxer.
                            pkt.data = bytes::Bytes::from(crate::muxer::to_avcc(&pkt.data));
                            out.push(pkt);
                            debug_log!(
                                "ProcessOutput ok status={} produced packet pts_us={} data_len={} packets_total={}",
                                status,
                                pts_us,
                                data_len,
                                out.len()
                            );
                        } else {
                            debug_log!("ProcessOutput ok status={} produced no packet", status);
                        }
                        // One returned sample retires one input and frees one
                        // slot. PTS comes from the output sample only.
                        self.inflight_inputs = self.inflight_inputs.saturating_sub(1);
                        if let Some(slot) = self.inflight_slots.pop_front() {
                            self.released_slots.push(slot);
                        }
                        if !self.mft_provides_samples {
                            self.free_outputs.push(sample);
                        }
                    } else {
                        // No sample means no output: keep the input in flight.
                        debug_log!("ProcessOutput ok status={} returned no sample", status);
                    }
                }
                Err(e) => {
                    debug_log!("ProcessOutput err code={:?} status={}", e.code(), status);
                    // Return a caller-owned sample to the pool if the MFT gave
                    // it back; MFT-provided samples are never pooled.
                    if let Some(sample) = sample {
                        self.free_outputs.push(sample);
                    }
                    if e.code() != MF_E_TRANSFORM_NEED_MORE_INPUT {
                        self.last_error_note(&e);
                    }
                    break;
                }
            }
        }
        if self.async_mode && self.inflight_inputs == 0 {
            self.pending_outputs = 0;
        }
        debug_log!(
            "drain_output done added={} packets_total={} inflight_inputs={} pending={} released={:?}",
            out.len().saturating_sub(packets_before),
            out.len(),
            self.inflight_inputs,
            self.pending_outputs,
            self.released_slots
        );
    }

    fn last_error_note(&self, e: &windows::core::Error) {
        debug_log!("ProcessOutput error code={:?} message={}", e.code(), e);
    }

    /// Ask for an IDR frame, e.g. right after a settings change.
    pub fn request_keyframe(&self) {
        if let Some(api) = &self.codec_api {
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVENCVIDEOFORCEKEYFRAME, &VARIANT::from(1u32));
            }
        }
    }

    pub fn frames_in(&self) -> u64 {
        self.frames_in
    }

    /// Take a refreshed CodecInfo so the capture thread can deliver it before
    /// the packets that depend on its SPS/PPS.
    pub fn take_pending_config(&mut self) -> Option<CodecInfo> {
        if self.config_pending {
            self.config_pending = false;
            Some(self.info.clone())
        } else {
            None
        }
    }
}

fn configure_codec(api: &ICodecAPI, settings: &VideoSettings) {
    unsafe {
        let rc_mode = match settings.rate_control {
            RateControl::Cbr => RATE_CONTROL_CBR,
            RateControl::Vbr => RATE_CONTROL_VBR,
            RateControl::Cqp => RATE_CONTROL_QUALITY,
        };
        let _ = api.SetValue(
            &CODECAPI_AVENCCOMMONRATECONTROLMODE,
            &VARIANT::from(rc_mode),
        );
        let _ = api.SetValue(
            &CODECAPI_AVENCCOMMONMEANBITRATE,
            &VARIANT::from(settings.bitrate_bps),
        );
        let _ = api.SetValue(
            &CODECAPI_AVENCCOMMONMAXBITRATE,
            &VARIANT::from((settings.bitrate_bps as f64 * 1.5) as u32),
        );
        // Codec API quality is 0..100, higher is better. Map CQ 51..1 onto it.
        let q = (100.0 - ((settings.quality.saturating_sub(1) as f32) * (100.0 / 50.0))) as u32;
        let _ = api.SetValue(
            &CODECAPI_AVENCCOMMONQUALITY,
            &VARIANT::from(q.clamp(1, 100)),
        );
        let speed = match settings.preset {
            Preset::LowSize => 20,
            Preset::Balanced => 50,
            Preset::HighQuality => 85,
            Preset::Custom => 50,
        };
        let _ = api.SetValue(&CODECAPI_AVENCCOMMONQUALITYVSSPEED, &VARIANT::from(speed));
        let _ = api.SetValue(&CODECAPI_AVENCCOMMONLOWLATENCY, &VARIANT::from(1u32));
        // No B-frames: keeps PTS == DTS and saves a frame of latency, which
        // makes the encoded-packet ring trivial to mux.
        let _ = api.SetValue(&CODECAPI_AVENCMPVDEFAULTBPICTURECOUNT, &VARIANT::from(0u32));
        let gop = (settings.fps * 2).clamp(30, 600);
        let _ = api.SetValue(&CODECAPI_AVENCMPVGOPSIZE, &VARIANT::from(gop));
        let _ = api.SetValue(&CODECAPI_AVENCH264CABACENABLE, &VARIANT::from(1u32));
    }
}

fn sample_to_packet(sample: &IMFSample, _fps: u32) -> Option<Packet> {
    let buffer = match unsafe { sample.ConvertToContiguousBuffer() } {
        Ok(buffer) => buffer,
        Err(e) => {
            debug_log!("sample_to_packet failed: ConvertToContiguousBuffer: {}", e);
            return None;
        }
    };
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    if let Err(e) = unsafe { buffer.Lock(&mut ptr, None, Some(&mut len)) } {
        debug_log!("sample_to_packet failed: Lock: {}", e);
        return None;
    }
    let data = if ptr.is_null() || len == 0 {
        bytes::Bytes::new()
    } else {
        bytes::Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
    };
    let _ = unsafe { buffer.Unlock() };
    if data.is_empty() {
        debug_log!("sample_to_packet failed: empty output buffer");
        return None;
    }
    let keyframe = unsafe { sample.GetUINT32(&MFSampleExtension_CleanPoint) }
        .map(|v| v != 0)
        .unwrap_or(false);
    // Always use the encoder's output timestamp. 100 ns -> microseconds.
    // Duration is filled in by the engine from the next packet's PTS.
    let pts_raw = match unsafe { sample.GetSampleTime() } {
        Ok(t) => t,
        Err(e) => {
            debug_log!(
                "sample_to_packet failed: missing output sample timestamp: {}",
                e
            );
            return None;
        }
    };
    if pts_raw < 0 {
        debug_log!(
            "sample_to_packet failed: negative output sample timestamp {}",
            pts_raw
        );
        return None;
    }
    let pts_us = pts_raw / 10;
    Some(Packet {
        pts_us,
        dur_us: 0,
        keyframe,
        data,
    })
}

/// Debug-only name for a H.264 NAL unit type.
fn nal_name(nal_type: u8) -> &'static str {
    match nal_type {
        7 => "SPS",
        8 => "PPS",
        5 => "IDR",
        1 => "non-IDR",
        6 => "SEI",
        9 => "AUD",
        _ => "other",
    }
}

/// SPS/PPS recovered from a sample, with NAL details for debug output only.
struct SampleConfig {
    extradata: Vec<u8>,
    sps_len: usize,
    pps_len: usize,
    nals: Vec<(usize, u8, usize)>,
}

/// Read NAL payloads from one encoded sample. Quick Sync emits either Annex-B
/// start codes or AVCC length prefixes; do not assume one format.
fn sample_nals(data: &[u8]) -> Vec<&[u8]> {
    if crate::muxer::starts_with_start_code(data) {
        crate::muxer::annexb_to_nals(data)
            .into_iter()
            .filter(|nal| !nal.is_empty())
            .collect()
    } else {
        parse_avcc_nals(data)
    }
}

/// Parse 4-byte big-endian length-prefixed (AVCC) sample data. Stop at the
/// first bad length instead of trusting the buffer.
fn parse_avcc_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut off = 0usize;
    while off + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += 4;
        if len == 0 || off + len > data.len() {
            break;
        }
        nals.push(&data[off..off + len]);
        off += len;
    }
    nals
}

/// Build Annex-B extradata: one 4-byte start code before SPS and one before PPS.
fn annexb_extradata(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + sps.len() + pps.len());
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(sps);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(pps);
    out
}

/// Find the first non-empty SPS and PPS in a raw sample and package them as
/// Annex-B extradata for the muxer's existing split_sps_pps path.
fn find_sample_config(data: &[u8]) -> Option<SampleConfig> {
    let nals = sample_nals(data);
    let mut sps: Option<&[u8]> = None;
    let mut pps: Option<&[u8]> = None;
    let mut details = Vec::with_capacity(nals.len());
    for (index, nal) in nals.iter().enumerate() {
        let nal_type = nal.first().map(|b| b & 0x1f).unwrap_or(0);
        details.push((index, nal_type, nal.len()));
        if nal.is_empty() {
            continue;
        }
        match nal_type {
            7 if sps.is_none() => sps = Some(nal),
            8 if pps.is_none() => pps = Some(nal),
            _ => {}
        }
    }
    let (sps, pps) = (sps?, pps?);
    Some(SampleConfig {
        extradata: annexb_extradata(sps, pps),
        sps_len: sps.len(),
        pps_len: pps.len(),
        nals: details,
    })
}

fn new_output_sample(buffer_bytes: u32) -> Result<IMFSample, String> {
    unsafe {
        let buffer =
            MFCreateMemoryBuffer(buffer_bytes).map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
        let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("AddBuffer: {e}"))?;
        Ok(sample)
    }
}

fn enum_encoders() -> Vec<IMFActivate> {
    unsafe {
        let input_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output_info = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        // Hardware async MFTs need ASYNCMFT in the enumeration mask.
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_ASYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        );
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input_info),
            Some(&output_info),
            &mut activates,
            &mut count,
        )
        .is_err()
        {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !activates.is_null() {
            for i in 0..count as usize {
                if let Some(a) = (*activates.add(i)).clone() {
                    out.push(a);
                }
            }
            CoTaskMemFree(Some(activates as *const core::ffi::c_void));
        }
        out
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    // GetString copies into a caller-owned wide buffer; no CoTaskMemFree needed.
    let mut buf = [0u16; 256];
    let mut len = 0u32;
    let ok = unsafe {
        activate
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len))
            .is_ok()
    };
    if !ok {
        return "Encoder".into();
    }
    let len = (len as usize).min(buf.len());
    let name = String::from_utf16_lossy(&buf[..len]);
    let name = name.split('\0').next().unwrap_or("").to_string();
    if name.is_empty() {
        "Encoder".into()
    } else {
        name
    }
}

/// Pack a MEDIA_FRAME_SIZE / RATIO pair into the UINT64 layout MF expects.
fn packed_word_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

/// DXGI adapter LUID behind the D3D11 device that owns the capture textures.
fn device_luid(device: &ID3D11Device) -> Option<LUID> {
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let adapter = dxgi.GetAdapter().ok()?;
        let desc = adapter.GetDesc().ok()?;
        Some(desc.AdapterLuid)
    }
}

/// Friendly DXGI adapter name, used in encoder errors so adapter mismatches are
/// obvious on Optimus laptops.
fn device_description(device: &ID3D11Device) -> String {
    unsafe {
        let dxgi: IDXGIDevice = match device.cast() {
            Ok(d) => d,
            Err(_) => return "unknown adapter".into(),
        };
        let adapter = match dxgi.GetAdapter() {
            Ok(a) => a,
            Err(_) => return "unknown adapter".into(),
        };
        let desc = match adapter.GetDesc() {
            Ok(d) => d,
            Err(_) => return "unknown adapter".into(),
        };
        let end = desc
            .Description
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(desc.Description.len());
        let name = String::from_utf16_lossy(&desc.Description[..end]);
        if name.is_empty() {
            "unknown adapter".into()
        } else {
            name
        }
    }
}

/// Adapter LUID advertised by the encoder MFT (absent on some MFTs).
fn encoder_luid(activate: &IMFActivate) -> Option<LUID> {
    let mut bytes = [0u8; 8];
    let mut len = 0u32;
    unsafe {
        activate
            .GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, Some(&mut len))
            .ok()?;
    }
    if len as usize != bytes.len() {
        return None;
    }
    Some(LUID {
        LowPart: u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        HighPart: i32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
    })
}
