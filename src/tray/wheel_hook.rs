//! Low-level mouse wheel hook for adjusting volume directly over the tray icon.

use super::TRAY_ICON_ID;
use crate::ui::WM_APP_WHEEL_VOLUME;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::GUID;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::PtInRect;
use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClassNameW, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    WindowFromPoint, HHOOK, MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_MOUSEWHEEL,
};

/// Target tray window handle read by the low-level mouse hook for wheel scrolling.
static HOOK_TRAY_HWND: AtomicUsize = AtomicUsize::new(0);

const TOOLBAR_CLASS_UTF16: [u16; 15] = [
    b'T' as u16, b'o' as u16, b'o' as u16, b'l' as u16, b'b' as u16, b'a' as u16, b'r' as u16,
    b'W' as u16, b'i' as u16, b'n' as u16, b'd' as u16, b'o' as u16, b'w' as u16, b'3' as u16,
    b'2' as u16,
];

pub(super) fn install_wheel_hook(tray_hwnd: HWND) -> Option<HHOOK> {
    HOOK_TRAY_HWND.store(tray_hwnd.0 as usize, Ordering::Release);
    unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), None, 0).ok() }
}

pub(super) fn uninstall_wheel_hook(hook: &mut Option<HHOOK>) {
    HOOK_TRAY_HWND.store(0, Ordering::Release);
    if let Some(hhook) = hook.take() {
        unsafe {
            let _ = UnhookWindowsHookEx(hhook);
        }
    }
}

/// Converts a raw Win32 wheel delta (`120` per notch) into a `±2%` volume step.
pub fn wheel_step_from_delta(delta: i16) -> i16 {
    if delta == 0 {
        return 0;
    }
    let notches = if delta > 0 {
        (delta as i32 + 119) / 120
    } else {
        (delta as i32 - 119) / 120
    };
    (notches * 2).clamp(-100, 100) as i16
}

fn is_cursor_over_tray_toolbar(pt: POINT) -> bool {
    unsafe {
        let target = WindowFromPoint(pt);
        if target.is_invalid() {
            return false;
        }
        let mut buf = [0u16; 32];
        let len = GetClassNameW(target, &mut buf);
        len == TOOLBAR_CLASS_UTF16.len() as i32 && buf[..len as usize] == TOOLBAR_CLASS_UTF16
    }
}

unsafe extern "system" fn mouse_ll_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL && lparam.0 != 0 {
        let tray_raw = HOOK_TRAY_HWND.load(Ordering::Relaxed);
        if tray_raw != 0 {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            // Filter non-tray scrolls locally before any cross-process Shell_NotifyIconGetRect call.
            if is_cursor_over_tray_toolbar(ms.pt) {
                let tray_hwnd = HWND(tray_raw as *mut _);
                let nii = NOTIFYICONIDENTIFIER {
                    cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                    hWnd: tray_hwnd,
                    uID: TRAY_ICON_ID,
                    guidItem: GUID::zeroed(),
                };
                if let Ok(rect) = Shell_NotifyIconGetRect(&nii) {
                    if rect.right > rect.left
                        && rect.bottom > rect.top
                        && PtInRect(&rect, ms.pt).as_bool()
                    {
                        let delta = ((ms.mouseData >> 16) as u16) as i16;
                        if delta != 0 {
                            let _ = PostMessageW(
                                Some(tray_hwnd),
                                WM_APP_WHEEL_VOLUME,
                                WPARAM(delta as isize as usize),
                                LPARAM(0),
                            );
                            return LRESULT(1);
                        }
                    }
                }
            }
        }
    }
    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::wheel_step_from_delta;

    #[test]
    fn wheel_delta_maps_to_two_percent_steps() {
        assert_eq!(wheel_step_from_delta(0), 0);
        assert_eq!(wheel_step_from_delta(120), 2);
        assert_eq!(wheel_step_from_delta(-120), -2);
        assert_eq!(wheel_step_from_delta(240), 4);
        assert_eq!(wheel_step_from_delta(-240), -4);
        assert_eq!(wheel_step_from_delta(30), 2);
        assert_eq!(wheel_step_from_delta(-30), -2);
    }
}

