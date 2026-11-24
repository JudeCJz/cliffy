//! Global hotkeys via RegisterHotKey on a dedicated thread.
//!
//! A thread-level hotkey (hwnd = null) posts WM_HOTKEY to the thread's message
//! queue, so no window is needed. Rebinds are pushed from the UI with a
//! PostThreadMessage wake-up.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_ESCAPE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_APP, WM_HOTKEY,
    WM_QUIT,
};

use crate::config::Hotkeys;
use crate::engine::AppShared;
use crate::types::Command;

const ID_SAVE: i32 = 1;
const ID_RECORD: i32 = 2;
const ID_BUFFER: i32 = 3;
const ID_SETTINGS: i32 = 4;
const WM_REBIND: u32 = WM_APP + 21;

static THREAD_ID: AtomicU32 = AtomicU32::new(0);
static PENDING: Mutex<Option<Hotkeys>> = Mutex::new(None);

pub struct HotkeyThread {
    join: Option<JoinHandle<()>>,
}

impl HotkeyThread {
    pub fn update(&self, hk: &Hotkeys) {
        if let Ok(mut p) = PENDING.lock() {
            *p = Some(hk.clone());
        }
        let id = THREAD_ID.load(Ordering::Relaxed);
        if id != 0 {
            unsafe {
                let _ = PostThreadMessageW(id, WM_REBIND, Default::default(), Default::default());
            }
        }
    }
}

impl Drop for HotkeyThread {
    fn drop(&mut self) {
        let id = THREAD_ID.load(Ordering::Relaxed);
        if id != 0 {
            unsafe {
                let _ = PostThreadMessageW(id, WM_QUIT, Default::default(), Default::default());
            }
        }
        // The thread is detached on exit; no join needed, but keep the handle
        // type future-proof.
        let _ = self.join.take();
    }
}

pub fn spawn(
    cmd: Sender<Command>,
    shared: Arc<AppShared>,
    initial: &Hotkeys,
) -> Result<HotkeyThread, String> {
    let initial = initial.clone();
    let thread = std::thread::Builder::new()
        .name("Cliffy-hotkeys".into())
        .spawn(move || run(cmd, shared, initial))
        .map_err(|e| e.to_string())?;
    Ok(HotkeyThread { join: Some(thread) })
}

fn run(cmd: Sender<Command>, shared: Arc<AppShared>, mut current: Hotkeys) {
    THREAD_ID.store(
        unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
        Ordering::Relaxed,
    );
    set_hotkeys(&current, &shared);

    loop {
        let mut msg = MSG::default();
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == 0 || ret.0 == -1 {
            break;
        }
        match msg.message {
            WM_HOTKEY => {
                match msg.wParam.0 as i32 {
                    ID_SAVE => {
                        shared.toast_sound.store(true, Ordering::Relaxed);
                        let _ = cmd.send(Command::SaveReplay);
                    }
                    ID_RECORD => {
                        let is_rec = shared
                            .ui
                            .lock()
                            .map(|u| u.recording_active)
                            .unwrap_or(false);
                        shared.toast_sound.store(true, Ordering::Relaxed);
                        if is_rec {
                            let _ = cmd.send(Command::StopRecording);
                        } else {
                            let _ = cmd.send(Command::StartRecording);
                        }
                    }
                    ID_BUFFER => {
                        let is_buffering = shared
                            .ui
                            .lock()
                            .map(|u| u.buffering_active)
                            .unwrap_or(false);
                        shared.toast_sound.store(true, Ordering::Relaxed);
                        if is_buffering {
                            let _ = cmd.send(Command::StopSession);
                        } else {
                            let _ = cmd.send(Command::StartSession);
                        }
                    }
                    ID_SETTINGS => {
                        shared.request_show_settings();
                    }
                    _ => {}
                }
                shared.repaint();
            }
            WM_REBIND => {
                if let Ok(mut p) = PENDING.lock() {
                    if let Some(new) = p.take() {
                        current = new;
                    }
                }
                set_hotkeys(&current, &shared);
                shared.repaint();
            }
            WM_QUIT => break,
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
    unregister_all();
    THREAD_ID.store(0, Ordering::Relaxed);
}

fn set_hotkeys(hk: &Hotkeys, shared: &AppShared) {
    unregister_all();
    let mut errors = Vec::new();
    for (id, spec) in [
        (ID_SAVE, &hk.save_replay),
        (ID_RECORD, &hk.toggle_record),
        (ID_BUFFER, &hk.toggle_buffer),
        (ID_SETTINGS, &hk.show_settings),
    ] {
        match parse_hotkey(spec) {
            Ok((mods, vk)) => {
                if let Err(e) = unsafe { RegisterHotKey(None, id, mods, vk) } {
                    errors.push(format!("{spec}: {e}"));
                }
            }
            Err(e) => errors.push(format!("{spec}: {e}")),
        }
    }
    if errors.is_empty() {
        shared.set_message("Hotkeys active");
    } else {
        shared.set_error(format!("Hotkey problem: {}", errors.join("; ")));
    }
}

fn unregister_all() {
    for id in [ID_SAVE, ID_RECORD, ID_BUFFER, ID_SETTINGS] {
        unsafe {
            let _ = UnregisterHotKey(None, id);
        }
    }
}

/// Parse strings like "Alt+R", "Ctrl+Shift+F9", "F10".
pub fn parse_hotkey(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let mut mods = MOD_NOREPEAT;
    let mut key: Option<u32> = None;
    for part in spec.split('+').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods = HOT_KEY_MODIFIERS(mods.0 | MOD_CONTROL.0),
            "alt" => mods = HOT_KEY_MODIFIERS(mods.0 | MOD_ALT.0),
            "shift" => mods = HOT_KEY_MODIFIERS(mods.0 | MOD_SHIFT.0),
            "win" | "super" => mods = HOT_KEY_MODIFIERS(mods.0 | MOD_WIN.0),
            other => {
                if key.is_some() {
                    return Err("more than one key".into());
                }
                key = Some(parse_key(other)?);
            }
        }
    }
    let key = key.ok_or("missing key")?;
    if mods.0 == MOD_NOREPEAT.0 && key == VK_ESCAPE.0 as u32 {
        return Err("Escape alone is not allowed".into());
    }
    Ok((mods, key))
}

fn parse_key(s: &str) -> Result<u32, String> {
    let upper = s.to_ascii_uppercase();
    if upper.len() == 1 {
        let c = upper.chars().next().unwrap();
        if c.is_ascii_alphanumeric() {
            return Ok(c as u32);
        }
    }
    if let Some(n) = upper.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Ok(0x70 + (n - 1));
        }
    }
    let vk = match upper.as_str() {
        "PRINTSCREEN" | "PRTSC" => 0x2C,
        "SCROLLLOCK" => 0x91,
        "PAUSE" => 0x13,
        "INSERT" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "SPACE" => 0x20,
        "TAB" => 0x09,
        "BACKSPACE" => 0x08,
        "ENTER" | "RETURN" => 0x0D,
        "UP" => 0x26,
        "DOWN" => 0x28,
        "LEFT" => 0x25,
        "RIGHT" => 0x27,
        _ => return Err(format!("unknown key '{s}'")),
    };
    Ok(vk)
}
