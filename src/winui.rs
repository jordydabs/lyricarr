//! The handful of Win32 calls a tray-resident window needs.
//!
//! Why this exists rather than `ViewportCommand::Visible(false)`: egui can only
//! act on a command on its next frame, and a hidden window gets no `WM_PAINT`,
//! so the frame that would un-hide it may never come. The tray lives on its own
//! thread with its own message pump; when you click "Open window" there it has
//! to be able to put the window back on screen **without** waiting for egui to
//! tick. `ShowWindow` does that directly, so hide/show never depends on a frame
//! being drawn.

use std::sync::atomic::{AtomicIsize, Ordering};
use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    MsgWaitForMultipleObjects, PeekMessageW, SetForegroundWindow, ShowWindow, TranslateMessage,
    MSG, PM_REMOVE, QS_ALLINPUT, SW_HIDE, SW_RESTORE, SW_SHOW, WM_QUIT,
};

/// Cached handle of our own main window. Found once by enumeration, then
/// reused - `EnumWindows` cannot find it again once it is hidden, because the
/// title check needs a visible window to be unambiguous.
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

struct Hunt {
    pid: u32,
    found: isize,
}

unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let hunt = &mut *(lparam as *mut Hunt);
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid != hunt.pid {
        return 1;
    }
    // Our window is the one with a title; winit also creates untitled helper
    // windows, and so does the tray icon.
    let mut buf = [0u16; 128];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if n > 0 {
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        if title.starts_with("Lyricarr") {
            hunt.found = hwnd as isize;
            return 0; // stop
        }
    }
    1
}

/// Our main window, looked up the first time and remembered after that.
pub fn main_window() -> Option<HWND> {
    let cached = MAIN_HWND.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached as HWND);
    }
    let mut hunt = Hunt {
        pid: unsafe { GetCurrentProcessId() },
        found: 0,
    };
    unsafe {
        EnumWindows(Some(visit), &mut hunt as *mut Hunt as LPARAM);
    }
    if hunt.found == 0 {
        return None;
    }
    MAIN_HWND.store(hunt.found, Ordering::Relaxed);
    Some(hunt.found as HWND)
}

/// Take the window off the screen, the taskbar and Alt-Tab. Safe to call when
/// the window has not been created yet; it simply does nothing.
pub fn hide() -> bool {
    match main_window() {
        Some(h) => unsafe {
            ShowWindow(h, SW_HIDE);
            true
        },
        None => false,
    }
}

/// Put it back and bring it to the front.
pub fn show() -> bool {
    match main_window() {
        Some(h) => unsafe {
            ShowWindow(h, SW_SHOW);
            // SW_RESTORE only when it is actually minimised. A hidden window
            // keeps WS_MAXIMIZE, so restoring unconditionally un-maximised a
            // maximised window every single time it came back from the tray.
            if IsIconic(h) != 0 {
                ShowWindow(h, SW_RESTORE);
            }
            SetForegroundWindow(h);
            true
        },
        None => false,
    }
}

/// Minimised - which is a different thing from hidden, and the two can both
/// be true at once.
pub fn is_minimized() -> bool {
    match main_window() {
        Some(h) => unsafe { IsIconic(h) != 0 },
        None => false,
    }
}

pub fn is_hidden() -> bool {
    match main_window() {
        Some(h) => unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible(h) == 0
        },
        None => false,
    }
}

/// Wait for messages for up to `timeout_ms`, then dispatch whatever arrived.
/// Returns false when `WM_QUIT` has been posted.
///
/// The tray icon creates its own window on whichever thread built it, and the
/// shell talks to it with window messages - so that thread has to run a pump or
/// clicks on the icon are never delivered. Waiting on the message queue (rather
/// than sleeping in a poll loop) is what keeps an idle tray at zero CPU while
/// still letting the thread wake on a timer to refresh its tooltip.
pub fn pump_for(timeout_ms: u32) -> bool {
    unsafe {
        MsgWaitForMultipleObjects(0, std::ptr::null(), 0, timeout_ms, QS_ALLINPUT);
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                return false;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    true
}
