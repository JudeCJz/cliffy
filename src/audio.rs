//! WASAPI Loopback Audio Capture for Cliffy.
//!
//! Captures system desktop audio and game sounds using Windows Audio Session API (WASAPI)
//! in loopback mode. Records into a circular memory buffer of PCM samples.

use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;

/// While this deadline is in the future, captured samples are replaced with
/// silence so app UI sounds are not baked into saved clips.
static SUPPRESS_UNTIL: OnceLock<Mutex<Instant>> = OnceLock::new();

fn suppression_deadline() -> &'static Mutex<Instant> {
    SUPPRESS_UNTIL.get_or_init(|| {
        Mutex::new(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        )
    })
}

/// Silence loopback capture for `duration`, preserving sample count and timing.
pub fn suppress_capture_sound(duration: Duration) {
    if let Ok(mut deadline) = suppression_deadline().lock() {
        *deadline = Instant::now() + duration;
    }
}

pub struct AudioRing {
    samples: Mutex<Vec<u8>>,
    sample_rate: u32,
    channels: u16,
    bytes_per_sec: usize,
    max_duration_secs: usize,
}

impl AudioRing {
    pub fn new(sample_rate: u32, channels: u16, max_duration_secs: usize) -> Self {
        let bytes_per_sec = (sample_rate as usize) * (channels as usize) * 4; // 32-bit float or 16-bit
        Self {
            samples: Mutex::new(Vec::with_capacity(bytes_per_sec * max_duration_secs)),
            sample_rate,
            channels,
            bytes_per_sec,
            max_duration_secs,
        }
    }

    pub fn push(&self, data: &[u8]) {
        let suppressed = suppression_deadline()
            .lock()
            .map(|deadline| Instant::now() < *deadline)
            .unwrap_or(false);
        if let Ok(mut lock) = self.samples.lock() {
            if suppressed {
                lock.extend(std::iter::repeat(0u8).take(data.len()));
            } else {
                lock.extend_from_slice(data);
            }
            let max_bytes = self.bytes_per_sec * self.max_duration_secs;
            if lock.len() > max_bytes {
                let excess = lock.len() - max_bytes;
                lock.drain(0..excess);
            }
        }
    }

    pub fn snapshot_last_seconds(&self, seconds: usize) -> Vec<u8> {
        if let Ok(lock) = self.samples.lock() {
            let needed = (self.bytes_per_sec * seconds).min(lock.len());
            let start = lock.len() - needed;
            lock[start..].to_vec()
        } else {
            Vec::new()
        }
    }
}

pub fn write_float_wav(
    path: &std::path::Path,
    pcm_bytes: &[u8],
    sample_rate: u32,
    channels: u16,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    let total_len = pcm_bytes.len() as u32;
    file.write_all(b"RIFF")?;
    file.write_all(&(total_len + 36).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&3u16.to_le_bytes())?; // 3 = IEEE float
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    let byte_rate = sample_rate * (channels as u32) * 4;
    file.write_all(&byte_rate.to_le_bytes())?;
    let block_align = channels * 4;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&32u16.to_le_bytes())?; // 32 bits per sample
    file.write_all(b"data")?;
    file.write_all(&total_len.to_le_bytes())?;
    file.write_all(pcm_bytes)?;
    Ok(())
}

pub fn find_ffmpeg() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(output) = std::process::Command::new("ffmpeg")
        .arg("-version")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
    {
        if output.status.success() {
            return Some(PathBuf::from("ffmpeg"));
        }
    }
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join(r"Microsoft\WinGet\Packages\Gyan.FFmpeg_Microsoft.Winget.Source_8wekyb3d8bbwe\ffmpeg-9.0.2-full_build\bin\ffmpeg.exe");
        if p.exists() {
            return Some(p);
        }
    }
    let fdm = PathBuf::from(r"D:\APP FILES\FDM\ffmpeg.exe");
    if fdm.exists() {
        return Some(fdm);
    }
    None
}

pub fn mux_audio_into_video(video_mp4: &std::path::Path, audio_wav: &std::path::Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let Some(ffmpeg) = find_ffmpeg() else {
        return false;
    };
    let final_tmp = video_mp4.with_extension("mux_tmp.mp4");
    let status = std::process::Command::new(ffmpeg)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .args(&[
            "-y",
            "-i",
            video_mp4.to_str().unwrap_or_default(),
            "-i",
            audio_wav.to_str().unwrap_or_default(),
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-shortest",
            final_tmp.to_str().unwrap_or_default(),
        ])
        .output();

    if let Ok(res) = status {
        let usable_output = res.status.success()
            && std::fs::metadata(&final_tmp)
                .map(|metadata| metadata.is_file() && metadata.len() > 0)
                .unwrap_or(false);
        if usable_output {
            let from: Vec<u16> = final_tmp
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let to: Vec<u16> = video_mp4
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let replaced = unsafe {
                MoveFileExW(
                    PCWSTR::from_raw(from.as_ptr()),
                    PCWSTR::from_raw(to.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            };
            if replaced.is_ok() {
                let _ = std::fs::remove_file(audio_wav);
                return true;
            }
        }
    }
    let _ = std::fs::remove_file(&final_tmp);
    false
}

static SWITCH_SOUND_BYTES: &[u8] = include_bytes!("../assets/switch-sound.mp3");
static SOUND_PLAY_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[link(name = "winmm")]
extern "system" {
    fn mciSendStringW(
        lpstr_command: *const u16,
        lpstr_return_string: *mut u16,
        u_return_length: u32,
        hwnd_callback: isize,
    ) -> u32;
}

pub fn play_switch_sound() {
    std::thread::spawn(|| {
        let sound_path = get_sound_file_path();
        let path_str = sound_path.to_string_lossy();
        let id = SOUND_PLAY_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

pub struct WasapiCapture {
    running: Arc<AtomicBool>,
    ring: Arc<AudioRing>,
}

impl WasapiCapture {
    pub fn new(max_duration_secs: usize) -> Self {
        let ring = Arc::new(AudioRing::new(48000, 2, max_duration_secs));
        Self {
            running: Arc::new(AtomicBool::new(false)),
            ring,
        }
    }

    pub fn ring(&self) -> Arc<AudioRing> {
        self.ring.clone()
    }

    pub fn start(&self) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }

        let running = self.running.clone();
        let ring = self.ring.clone();

        std::thread::Builder::new()
            .name("cliffy-wasapi".into())
            .spawn(move || unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                capture_loop(running, ring);
                CoUninitialize();
            })
            .ok();
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

unsafe fn capture_loop(running: Arc<AtomicBool>, ring: Arc<AudioRing>) {
    let enumerator: Result<IMMDeviceEnumerator, _> =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
    let Ok(enumerator) = enumerator else { return };

    let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
        return;
    };

    let Ok(client): Result<IAudioClient, _> = device.Activate(CLSCTX_ALL, None) else {
        return;
    };

    let Ok(format_ptr) = client.GetMixFormat() else {
        return;
    };
    let format = &*format_ptr;

    let hns_requested_duration = 10_000_000; // 1 second buffer
    let init_res = client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK,
        hns_requested_duration,
        0,
        format_ptr,
        None,
    );
    if init_res.is_err() {
        return;
    }

    let Ok(capture_client): Result<IAudioCaptureClient, _> = client.GetService() else {
        return;
    };

    if client.Start().is_err() {
        return;
    }

    let bytes_per_frame = format.nBlockAlign as usize;

    while running.load(Ordering::Relaxed) {
        while let Ok(packet_length) = capture_client.GetNextPacketSize() {
            if packet_length == 0 {
                break;
            }

            let mut data_ptr = std::ptr::null_mut();
            let mut num_frames = 0u32;
            let mut flags = 0u32;

            if capture_client
                .GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)
                .is_ok()
            {
                let byte_count = (num_frames as usize) * bytes_per_frame;
                if byte_count > 0 && !data_ptr.is_null() {
                    let slice = std::slice::from_raw_parts(data_ptr, byte_count);
                    ring.push(slice);
                }
                let _ = capture_client.ReleaseBuffer(num_frames);
            }
        }

        std::thread::sleep(Duration::from_millis(15));
    }

    let _ = client.Stop();
}
