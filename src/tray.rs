//! System tray integration for Cliffy.
//!
//! Owns a message-only HWND on a dedicated thread, registers a Shell_NotifyIcon,
//! and handles mouse clicks and a context menu (Clip, Record, Quit).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use egui::ViewportCommand;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconFromResourceEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW,
    LoadIconW, PostMessageW, PostThreadMessageW, RegisterClassW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, CS_DBLCLKS, HCURSOR, IDI_APPLICATION, LR_DEFAULTCOLOR,
    MF_SEPARATOR, MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_QUIT, WM_RBUTTONUP, WM_USER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::engine::AppShared;
use crate::types::Command;

const WM_TRAY_CALLBACK: u32 = WM_USER + 101;
static TRAY_THREAD_ID: AtomicU32 = AtomicU32::new(0);

struct TrayState {
    cmd: Sender<Command>,
    shared: Arc<AppShared>,
}

static TRAY_STATE: Mutex<Option<TrayState>> = Mutex::new(None);

pub struct TrayThread {
    join: Option<JoinHandle<()>>,
}

impl Drop for TrayThread {
    fn drop(&mut self) {
        let id = TRAY_THREAD_ID.load(Ordering::Relaxed);
        if id != 0 {
            unsafe {
                let _ = PostThreadMessageW(id, WM_QUIT, Default::default(), Default::default());
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        if let Ok(mut lock) = TRAY_STATE.lock() {
            *lock = None;
        }
    }
}

pub fn spawn(cmd: Sender<Command>, shared: Arc<AppShared>) -> Result<TrayThread, String> {
    if let Ok(mut lock) = TRAY_STATE.lock() {
        *lock = Some(TrayState {
            cmd,
            shared: shared.clone(),
        });
    }

    let thread = std::thread::Builder::new()
        .name("Cliffy-tray".into())
        .spawn(move || run(&shared))
        .map_err(|e| e.to_string())?;

    Ok(TrayThread { join: Some(thread) })
}

unsafe extern "system" fn tray_wndproc(
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TRAY_CALLBACK {
        let event = (lparam.0 & 0xffff) as u32;
        match event {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                if let Ok(guard) = TRAY_STATE.lock() {
                    if let Some(state) = guard.as_ref() {
                        state.shared.request_toggle_settings();
                    }
                }
            }
            WM_RBUTTONUP => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                let _ = SetForegroundWindow(hwnd);
                if let Ok(hmenu) = CreatePopupMenu() {
                    let _ = AppendMenuW(hmenu, MF_STRING, 1, w!("Save Replay Clip"));
                    let _ = AppendMenuW(hmenu, MF_STRING, 2, w!("Toggle Recording"));
                    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
                    let _ = AppendMenuW(hmenu, MF_STRING, 3, w!("Quit Cliffy"));

                    let choice = TrackPopupMenu(
                        hmenu,
                        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                        pt.x,
                        pt.y,
                        Some(0),
                        hwnd,
                        None,
                    );
                    let _ = DestroyMenu(hmenu);
                    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

                    if let Ok(guard) = TRAY_STATE.lock() {
                        if let Some(state) = guard.as_ref() {
                            match choice.0 {
                                1 => {
                                    state.shared.toast_sound.store(false, Ordering::Relaxed);
                                    let _ = state.cmd.send(Command::SaveReplay);
                                }
                                2 => {
                                    state.shared.toast_sound.store(false, Ordering::Relaxed);
                                    let _ = state.cmd.send(Command::ToggleRecording);
                                }
                                3 => {
                                    state.shared.quit_requested.store(true, Ordering::Relaxed);
                                    if let Ok(ctx) = state.shared.ctx.lock() {
                                        if let Some(ctx) = ctx.as_ref() {
                                            ctx.send_viewport_cmd(ViewportCommand::Close);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        return LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, _wparam, lparam)
}

fn run(shared: &AppShared) {
    unsafe {
        run_inner(shared);
    }
}

unsafe fn run_inner(shared: &AppShared) {
    TRAY_THREAD_ID.store(
        windows::Win32::System::Threading::GetCurrentThreadId(),
        Ordering::Relaxed,
    );

    let hinstance = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE::from(h),
        Err(_) => return,
    };

    let class_name = w!("CliffyTrayClass");
    let class = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(tray_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name,
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        ..Default::default()
    };
    let _ = RegisterClassW(&class);

    let hwnd = match CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class_name,
        w!("CliffyTrayWindow"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinstance),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };

    const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
    let hicon = match CreateIconFromResourceEx(ICON_PNG, true, 0x00030000, 32, 32, LR_DEFAULTCOLOR)
    {
        Ok(icon) => icon,
        Err(_) => LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
    };

    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY_CALLBACK,
        hIcon: hicon,
        ..Default::default()
    };

    let tip = "Cliffy\0";
    for (slot, ch) in nid.szTip.iter_mut().zip(tip.encode_utf16()) {
        *slot = ch;
    }

    let _ = Shell_NotifyIconW(NIM_ADD, &nid);

    loop {
        if shared.quit_requested.load(Ordering::Relaxed) {
            break;
        }
        let mut msg = MSG::default();
        let ret = GetMessageW(&mut msg, None, 0, 0);
        if ret.0 == 0 || ret.0 == -1 {
            break;
        }
        if msg.message == WM_QUIT {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    if !hicon.is_invalid() {
        let _ = DestroyIcon(hicon);
    }
    let _ = DestroyWindow(hwnd);
    if let Ok(mut lock) = TRAY_STATE.lock() {
        *lock = None;
    }
    TRAY_THREAD_ID.store(0, Ordering::Relaxed);
}
