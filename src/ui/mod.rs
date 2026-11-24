//! Lightweight custom-painted Win32 popup windows and shared GDI/DWM theming helpers.

mod devices_dialog;
mod update_dialog;
mod volume_flyout;

pub use devices_dialog::DevicesDialog;
pub use update_dialog::{UpdateDialog, UpdateDialogView};
pub use volume_flyout::VolumeFlyout;

use crate::config::to_wide_null;
use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, BeginPaint, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, RoundRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, DRAW_TEXT_FORMAT, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, HDC,
    HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_NULL,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, SetForegroundWindow, SetWindowPos, ShowWindow, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOW, WM_APP, WS_CAPTION, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_OVERLAPPED, WS_SYSMENU,
};

pub const WM_APP_FLYOUT_SET_VOLUME: u32 = WM_APP + 20;
pub const WM_APP_FLYOUT_TOGGLE_MUTE: u32 = WM_APP + 21;
pub const WM_APP_WHEEL_VOLUME: u32 = WM_APP + 22;
pub const WM_APP_DEVICES_TOGGLE_ID: u32 = WM_APP + 23;
pub const WM_APP_UPDATE_START_DOWNLOAD: u32 = WM_APP + 33;
pub const WM_APP_UPDATE_CANCEL_DOWNLOAD: u32 = WM_APP + 34;
pub const WM_APP_UPDATE_RETRY_CHECK: u32 = WM_APP + 35;

const PERSONALIZE_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const APPS_LIGHT_VALUE: &str = "AppsUseLightTheme";

pub(super) const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

/// Returns whether Windows application theme is set to Light mode (`AppsUseLightTheme != 0`).
pub fn is_apps_light_theme() -> bool {
    let subkey = to_wide_null(PERSONALIZE_SUBKEY);
    let value_name = to_wide_null(APPS_LIGHT_VALUE);
    let mut hkey = HKEY::default();

    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }

        let mut data: u32 = 0;
        let mut data_len = std::mem::size_of::<u32>() as u32;
        let mut reg_type = REG_VALUE_TYPE::default();
        let ok = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            Some(&mut reg_type),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut data_len),
        )
        .is_ok();
        let _ = RegCloseKey(hkey);
        ok && reg_type == REG_DWORD && data != 0
    }
}

#[derive(Clone, Copy)]
pub(super) struct ThemeColors {
    pub bg: COLORREF,
    pub surface_hover: COLORREF,
    pub border: COLORREF,
    pub text_primary: COLORREF,
    pub text_muted: COLORREF,
    pub track_bg: COLORREF,
    pub accent: COLORREF,
    pub mute_red: COLORREF,
    pub btn_bg: COLORREF,
    pub btn_hover: COLORREF,
    pub btn_text: COLORREF,
    pub checkmark: COLORREF,
}

impl ThemeColors {
    pub(super) fn current() -> Self {
        if is_apps_light_theme() {
            Self {
                bg: rgb(246, 248, 252),
                surface_hover: rgb(230, 236, 244),
                border: rgb(198, 206, 218),
                text_primary: rgb(24, 28, 36),
                text_muted: rgb(95, 106, 122),
                track_bg: rgb(202, 210, 222),
                accent: rgb(0, 103, 192),
                mute_red: rgb(208, 48, 56),
                btn_bg: rgb(0, 103, 192),
                btn_hover: rgb(0, 88, 166),
                btn_text: rgb(255, 255, 255),
                checkmark: rgb(255, 255, 255),
            }
        } else {
            Self {
                bg: rgb(30, 34, 43),
                surface_hover: rgb(44, 51, 65),
                border: rgb(58, 66, 82),
                text_primary: rgb(242, 245, 250),
                text_muted: rgb(154, 166, 184),
                track_bg: rgb(62, 71, 88),
                accent: rgb(96, 205, 255),
                mute_red: rgb(255, 107, 107),
                btn_bg: rgb(96, 205, 255),
                btn_hover: rgb(126, 216, 255),
                btn_text: rgb(18, 22, 30),
                checkmark: rgb(18, 22, 30),
            }
        }
    }
}

pub(super) fn scale_px(logical: i32, dpi: u32) -> i32 {
    (logical * dpi as i32 + 48) / 96
}

pub(super) fn window_dpi(hwnd: HWND) -> u32 {
    unsafe {
        let d = GetDpiForWindow(hwnd);
        if d > 0 {
            d
        } else {
            GetDpiForSystem().max(96)
        }
    }
}

pub(super) fn apply_dwm_style(hwnd: HWND, round_corners: bool) {
    let dark: i32 = if is_apps_light_theme() { 0 } else { 1 };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        );
        if round_corners {
            let pref = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
}

pub(super) fn create_segoe_font(px_height: i32, semibold: bool) -> HFONT {
    let face = to_wide_null("Segoe UI");
    let weight = if semibold { FW_SEMIBOLD.0 } else { FW_NORMAL.0 };
    unsafe {
        CreateFontW(
            -px_height,
            0,
            0,
            0,
            weight as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR(face.as_ptr()),
        )
    }
}

pub(super) fn draw_text(
    hdc: HDC,
    text: &str,
    mut rect: RECT,
    color: COLORREF,
    font: HFONT,
    flags: DRAW_TEXT_FORMAT,
) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return;
    }
    unsafe {
        let prev_font = SelectObject(hdc, font.into());
        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, color);
        DrawTextW(hdc, &mut wide, &mut rect, flags);
        SelectObject(hdc, prev_font);
    }
}

pub(super) fn fill_round_rect(
    hdc: HDC,
    r: RECT,
    radius: i32,
    fill: COLORREF,
    border: Option<COLORREF>,
) {
    unsafe {
        let brush = CreateSolidBrush(fill);
        let pen = match border {
            Some(c) => CreatePen(PS_SOLID, 1, c),
            None => CreatePen(PS_NULL, 0, COLORREF(0)),
        };
        let old_b = SelectObject(hdc, brush.into());
        let old_p = SelectObject(hdc, pen.into());
        let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
        SelectObject(hdc, old_p);
        SelectObject(hdc, old_b);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());
    }
}

pub(super) fn with_backbuffer<F: FnOnce(HDC, i32, i32)>(hwnd: HWND, paint_fn: F) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);

        let mem_dc = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, h);
        let old_bmp = SelectObject(mem_dc, bmp.into());

        paint_fn(mem_dc, w, h);

        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem_dc), 0, 0, SRCCOPY);
        SelectObject(mem_dc, old_bmp);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        let _ = EndPaint(hwnd, &ps);
    }
}

pub(super) fn pt_in_rect(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

pub(super) fn sign_extend_i16_from_wparam(wparam: WPARAM) -> i16 {
    ((wparam.0 >> 16) as u16) as i16
}

pub(super) fn show_centered_tool_dialog(hwnd: HWND, client_w: i32, client_h: i32) {
    let dpi = window_dpi(hwnd);
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    unsafe {
        let _ = AdjustWindowRectExForDpi(
            &mut frame,
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            false,
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            dpi,
        );
    }
    let win_w = frame.right - frame.left;
    let win_h = frame.bottom - frame.top;

    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let work = unsafe {
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            }
        }
    };
    let x = (work.left + work.right - win_w) / 2;
    let y = (work.top + work.bottom - win_h) / 2;

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            win_w,
            win_h,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}
