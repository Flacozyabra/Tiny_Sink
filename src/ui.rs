//! Lightweight custom-painted Win32 popup windows: volume flyout bar and devices checklist dialog.

use crate::audio::AudioDeviceInfo;
use crate::config::{to_wide_null, AppConfig, DeviceKind};
use crate::updater::ReleaseInfo;
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, MonitorFromPoint, Polygon, Polyline, RoundRect, SelectObject,
    SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_PITCH, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, DT_WORDBREAK, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_NULL, PS_SOLID, SRCCOPY,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos, GetWindowLongPtrW,
    IsWindowVisible, LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, HWND_TOPMOST,
    IDC_ARROW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOW, WM_ACTIVATE, WM_APP, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSEXW, WS_CAPTION, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_OVERLAPPED, WS_POPUP, WS_SYSMENU,
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
const FLYOUT_CLASS: &str = "TinySinkVolumeFlyout";
const DEVICES_CLASS: &str = "TinySinkDevicesDialog";
const UPDATE_CLASS: &str = "TinySinkUpdateDialog";

const FLYOUT_LOGICAL_W: i32 = 324;
const FLYOUT_LOGICAL_H: i32 = 92;
const DEVICES_LOGICAL_W: i32 = 420;
const DEVICES_ROW_H: i32 = 36;
const DEVICES_HEADER_H: i32 = 40;
const DEVICES_FOOTER_H: i32 = 52;
const DEVICES_MAX_VISIBLE_ROWS: usize = 8;
const UPDATE_LOGICAL_W: i32 = 430;
const UPDATE_LOGICAL_H: i32 = 204;

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

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

#[derive(Clone, Copy)]
struct ThemeColors {
    bg: COLORREF,
    surface_hover: COLORREF,
    border: COLORREF,
    text_primary: COLORREF,
    text_muted: COLORREF,
    track_bg: COLORREF,
    accent: COLORREF,
    mute_red: COLORREF,
    btn_bg: COLORREF,
    btn_hover: COLORREF,
    btn_text: COLORREF,
    checkmark: COLORREF,
}

impl ThemeColors {
    fn current() -> Self {
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

fn scale_px(logical: i32, dpi: u32) -> i32 {
    (logical * dpi as i32 + 48) / 96
}

fn window_dpi(hwnd: HWND) -> u32 {
    unsafe {
        let d = GetDpiForWindow(hwnd);
        if d > 0 {
            d
        } else {
            GetDpiForSystem().max(96)
        }
    }
}

fn apply_dwm_style(hwnd: HWND, round_corners: bool) {
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

fn create_segoe_font(px_height: i32, semibold: bool) -> HFONT {
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

fn draw_text(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    text: &str,
    mut rect: RECT,
    color: COLORREF,
    font: HFONT,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
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

fn fill_round_rect(
    hdc: windows::Win32::Graphics::Gdi::HDC,
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

fn with_backbuffer<F: FnOnce(windows::Win32::Graphics::Gdi::HDC, i32, i32)>(
    hwnd: HWND,
    paint_fn: F,
) {
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

fn pt_in_rect(r: RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn sign_extend_i16_from_wparam(wparam: WPARAM) -> i16 {
    ((wparam.0 >> 16) as u16) as i16
}

// ============================================================================
// Volume Flyout Window (LMB single click)
// ============================================================================

struct FlyoutState {
    hwnd: HWND,
    tray_hwnd: HWND,
    device_name: String,
    kind: DeviceKind,
    volume: u8,
    muted: bool,
    dragging: bool,
    hover_mute: bool,
    last_deactivated: Option<Instant>,
}

impl FlyoutState {
    fn track_rect(&self, dpi: u32) -> RECT {
        RECT {
            left: scale_px(56, dpi),
            top: scale_px(36, dpi),
            right: scale_px(264, dpi),
            bottom: scale_px(82, dpi),
        }
    }

    fn mute_rect(&self, dpi: u32) -> RECT {
        RECT {
            left: scale_px(12, dpi),
            top: scale_px(42, dpi),
            right: scale_px(46, dpi),
            bottom: scale_px(76, dpi),
        }
    }

    fn percent_from_x(&self, x: i32, dpi: u32) -> u8 {
        let r = self.track_rect(dpi);
        let pad = scale_px(6, dpi);
        let start = r.left + pad;
        let end = (r.right - pad).max(start + 1);
        let clamped = x.clamp(start, end);
        (((clamped - start) * 100 + (end - start) / 2) / (end - start)).clamp(0, 100) as u8
    }

    fn apply_slider_x(&mut self, x: i32) {
        let dpi = window_dpi(self.hwnd);
        let pct = self.percent_from_x(x, dpi);
        if pct != self.volume || self.muted {
            self.volume = pct;
            self.muted = false;
            unsafe {
                let _ = PostMessageW(
                    Some(self.tray_hwnd),
                    WM_APP_FLYOUT_SET_VOLUME,
                    WPARAM(pct as usize),
                    LPARAM(0),
                );
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    fn paint(&self, hdc: windows::Win32::Graphics::Gdi::HDC, w: i32, h: i32) {
        let dpi = window_dpi(self.hwnd);
        let colors = ThemeColors::current();

        unsafe {
            let bg_brush = CreateSolidBrush(colors.bg);
            let full = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            FillRect(hdc, &full, bg_brush);
            let _ = DeleteObject(bg_brush.into());
        }

        // Crisp outer 1px border.
        fill_round_rect(
            hdc,
            RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
            scale_px(10, dpi),
            colors.bg,
            Some(colors.border),
        );

        let title_font = create_segoe_font(scale_px(14, dpi), true);
        let val_font = create_segoe_font(scale_px(16, dpi), true);

        draw_text(
            hdc,
            &self.device_name,
            RECT {
                left: scale_px(16, dpi),
                top: scale_px(10, dpi),
                right: w - scale_px(16, dpi),
                bottom: scale_px(32, dpi),
            },
            colors.text_primary,
            title_font,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        // Mute button icon on left.
        let mute_rc = self.mute_rect(dpi);
        if self.hover_mute {
            fill_round_rect(hdc, mute_rc, scale_px(8, dpi), colors.surface_hover, None);
        }
        draw_flyout_glyph(hdc, mute_rc, self.kind, self.muted, colors, dpi);

        // Slider track + active fill + circular thumb.
        let tr = self.track_rect(dpi);
        let pad = scale_px(6, dpi);
        let bar_left = tr.left + pad;
        let bar_right = tr.right - pad;
        let cy = (tr.top + tr.bottom) / 2;
        let half_bar = scale_px(2, dpi).max(2);
        let thumb_r = scale_px(7, dpi);

        fill_round_rect(
            hdc,
            RECT {
                left: bar_left,
                top: cy - half_bar,
                right: bar_right,
                bottom: cy + half_bar,
            },
            half_bar * 2,
            colors.track_bg,
            None,
        );

        let thumb_x = bar_left + ((bar_right - bar_left) * self.volume as i32 + 50) / 100;
        let fill_color = if self.muted {
            colors.mute_red
        } else {
            colors.accent
        };
        if thumb_x > bar_left {
            fill_round_rect(
                hdc,
                RECT {
                    left: bar_left,
                    top: cy - half_bar,
                    right: thumb_x,
                    bottom: cy + half_bar,
                },
                half_bar * 2,
                fill_color,
                None,
            );
        }

        // Thumb circle.
        unsafe {
            let outer_brush = CreateSolidBrush(colors.text_primary);
            let outer_pen = CreatePen(PS_SOLID, 1, colors.border);
            let old_b = SelectObject(hdc, outer_brush.into());
            let old_p = SelectObject(hdc, outer_pen.into());
            let _ = Ellipse(
                hdc,
                thumb_x - thumb_r,
                cy - thumb_r,
                thumb_x + thumb_r,
                cy + thumb_r,
            );
            SelectObject(hdc, old_p);
            SelectObject(hdc, old_b);
            let _ = DeleteObject(outer_pen.into());
            let _ = DeleteObject(outer_brush.into());

            let inner_r = (thumb_r / 2).max(2);
            let inner_brush = CreateSolidBrush(fill_color);
            let null_pen = CreatePen(PS_NULL, 0, COLORREF(0));
            let old_b = SelectObject(hdc, inner_brush.into());
            let old_p = SelectObject(hdc, null_pen.into());
            let _ = Ellipse(
                hdc,
                thumb_x - inner_r,
                cy - inner_r,
                thumb_x + inner_r,
                cy + inner_r,
            );
            SelectObject(hdc, old_p);
            SelectObject(hdc, old_b);
            let _ = DeleteObject(null_pen.into());
            let _ = DeleteObject(inner_brush.into());
        }

        // Numeric volume value on right.
        let vol_str = format!("{}", self.volume);
        let num_color = if self.muted {
            colors.mute_red
        } else {
            colors.text_primary
        };
        draw_text(
            hdc,
            &vol_str,
            RECT {
                left: scale_px(268, dpi),
                top: scale_px(40, dpi),
                right: w - scale_px(16, dpi),
                bottom: scale_px(76, dpi),
            },
            num_color,
            val_font,
            DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX,
        );

        unsafe {
            let _ = DeleteObject(title_font.into());
            let _ = DeleteObject(val_font.into());
        }
    }
}

fn draw_flyout_glyph(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rc: RECT,
    kind: DeviceKind,
    muted: bool,
    colors: ThemeColors,
    dpi: u32,
) {
    let cx = (rc.left + rc.right) / 2;
    let cy = (rc.top + rc.bottom) / 2;
    let fg = if muted {
        colors.text_muted
    } else {
        colors.text_primary
    };
    let s = |v: i32| scale_px(v, dpi);

    unsafe {
        let brush = CreateSolidBrush(fg);
        let pen = CreatePen(PS_SOLID, s(2).max(2), fg);
        let null_pen = CreatePen(PS_NULL, 0, COLORREF(0));

        match kind {
            DeviceKind::Speakers => {
                let old_b = SelectObject(hdc, brush.into());
                let old_p = SelectObject(hdc, null_pen.into());
                let pts = [
                    POINT {
                        x: cx - s(8),
                        y: cy - s(3),
                    },
                    POINT {
                        x: cx - s(4),
                        y: cy - s(3),
                    },
                    POINT {
                        x: cx + s(1),
                        y: cy - s(7),
                    },
                    POINT {
                        x: cx + s(1),
                        y: cy + s(7),
                    },
                    POINT {
                        x: cx - s(4),
                        y: cy + s(3),
                    },
                    POINT {
                        x: cx - s(8),
                        y: cy + s(3),
                    },
                ];
                let _ = Polygon(hdc, &pts);
                SelectObject(hdc, old_p);
                SelectObject(hdc, old_b);

                if !muted {
                    let wave_pen = CreatePen(PS_SOLID, s(2).max(1), colors.accent);
                    let old_p = SelectObject(hdc, wave_pen.into());
                    let w1 = [
                        POINT {
                            x: cx + s(4),
                            y: cy - s(4),
                        },
                        POINT {
                            x: cx + s(6),
                            y: cy,
                        },
                        POINT {
                            x: cx + s(4),
                            y: cy + s(4),
                        },
                    ];
                    let _ = Polyline(hdc, &w1);
                    SelectObject(hdc, old_p);
                    let _ = DeleteObject(wave_pen.into());
                }
            }
            DeviceKind::Headphones => {
                let old_p = SelectObject(hdc, pen.into());
                let arc = [
                    POINT {
                        x: cx - s(7),
                        y: cy + s(2),
                    },
                    POINT {
                        x: cx - s(6),
                        y: cy - s(5),
                    },
                    POINT {
                        x: cx,
                        y: cy - s(7),
                    },
                    POINT {
                        x: cx + s(6),
                        y: cy - s(5),
                    },
                    POINT {
                        x: cx + s(7),
                        y: cy + s(2),
                    },
                ];
                let _ = Polyline(hdc, &arc);
                SelectObject(hdc, old_p);

                fill_round_rect(
                    hdc,
                    RECT {
                        left: cx - s(9),
                        top: cy,
                        right: cx - s(5),
                        bottom: cy + s(7),
                    },
                    s(2),
                    fg,
                    None,
                );
                fill_round_rect(
                    hdc,
                    RECT {
                        left: cx + s(5),
                        top: cy,
                        right: cx + s(9),
                        bottom: cy + s(7),
                    },
                    s(2),
                    fg,
                    None,
                );
            }
        }

        if muted {
            let x_pen = CreatePen(PS_SOLID, s(2).max(2), colors.mute_red);
            let old_p = SelectObject(hdc, x_pen.into());
            let l1 = [
                POINT {
                    x: cx + s(3),
                    y: cy - s(3),
                },
                POINT {
                    x: cx + s(9),
                    y: cy + s(3),
                },
            ];
            let l2 = [
                POINT {
                    x: cx + s(9),
                    y: cy - s(3),
                },
                POINT {
                    x: cx + s(3),
                    y: cy + s(3),
                },
            ];
            let _ = Polyline(hdc, &l1);
            let _ = Polyline(hdc, &l2);
            SelectObject(hdc, old_p);
            let _ = DeleteObject(x_pen.into());
        }

        let _ = DeleteObject(null_pen.into());
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());
    }
}

/// Manages the reusable LMB volume flyout window.
pub struct VolumeFlyout {
    hwnd: HWND,
    state: Rc<RefCell<FlyoutState>>,
}

impl VolumeFlyout {
    pub fn create(tray_hwnd: HWND) -> Option<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None).ok()?;
            let class_name = to_wide_null(FLYOUT_CLASS);
            let cursor = LoadCursorW(None, IDC_ARROW).ok();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(flyout_wnd_proc),
                hInstance: hinstance.into(),
                hCursor: cursor.unwrap_or_default(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let title = to_wide_null("Tiny_Sink Volume");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                100,
                100,
                Some(tray_hwnd),
                None,
                Some(hinstance.into()),
                None,
            )
            .ok()?;

            let state = Rc::new(RefCell::new(FlyoutState {
                hwnd,
                tray_hwnd,
                device_name: String::new(),
                kind: DeviceKind::Speakers,
                volume: 0,
                muted: false,
                dragging: false,
                hover_mute: false,
                last_deactivated: None,
            }));
            let raw = Rc::into_raw(Rc::clone(&state));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            apply_dwm_style(hwnd, true);

            Some(Self { hwnd, state })
        }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn was_recently_deactivated(&self, window: Duration) -> bool {
        self.state
            .try_borrow()
            .ok()
            .and_then(|s| s.last_deactivated)
            .is_some_and(|t| t.elapsed() <= window)
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    pub fn sync_state(&self, name: &str, kind: DeviceKind, volume: u8, muted: bool) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.device_name = name.to_string();
            s.kind = kind;
            s.volume = volume;
            s.muted = muted;
        }
        if self.is_visible() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    pub fn show_near_tray(&self, tray_hwnd: HWND, tray_icon_id: u32) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.last_deactivated = None;
        }
        apply_dwm_style(self.hwnd, true);
        let dpi = window_dpi(self.hwnd);
        let w = scale_px(FLYOUT_LOGICAL_W, dpi);
        let h = scale_px(FLYOUT_LOGICAL_H, dpi);

        let anchor = query_tray_anchor(tray_hwnd, tray_icon_id);
        let (x, y) = compute_flyout_pos(anchor, w, h, scale_px(8, dpi));

        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                w,
                h,
                SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(self.hwnd);
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }
}

impl Drop for VolumeFlyout {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn query_tray_anchor(tray_hwnd: HWND, tray_icon_id: u32) -> RECT {
    let nii = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: tray_hwnd,
        uID: tray_icon_id,
        guidItem: GUID::zeroed(),
    };
    if let Ok(r) = unsafe { Shell_NotifyIconGetRect(&nii) } {
        if r.right > r.left && r.bottom > r.top {
            return r;
        }
    }
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    RECT {
        left: pt.x - 8,
        top: pt.y - 8,
        right: pt.x + 8,
        bottom: pt.y + 8,
    }
}

fn compute_flyout_pos(anchor: RECT, w: i32, h: i32, margin: i32) -> (i32, i32) {
    let center_x = (anchor.left + anchor.right) / 2;
    let center_y = (anchor.top + anchor.bottom) / 2;
    let work = unsafe {
        let hmon = MonitorFromPoint(
            POINT {
                x: center_x,
                y: center_y,
            },
            MONITOR_DEFAULTTONEAREST,
        );
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

    let mut x = center_x - w / 2;
    x = x.clamp(work.left + margin, (work.right - w - margin).max(work.left + margin));

    // Prefer above taskbar when anchor is near bottom; below if taskbar is at top.
    let y = if center_y > (work.top + work.bottom) / 2 {
        (work.bottom - h - margin).max(work.top + margin)
    } else {
        (work.top + margin).min((work.bottom - h - margin).max(work.top + margin))
    };

    (x, y)
}

unsafe extern "system" fn flyout_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<FlyoutState>;
    if !ptr.is_null() {
        Rc::increment_strong_count(ptr);
        let state_rc = Rc::from_raw(ptr);

        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                if let Ok(state) = state_rc.try_borrow() {
                    with_backbuffer(hwnd, |hdc, w, h| state.paint(hdc, w, h));
                }
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    if pt_in_rect(state.mute_rect(dpi), x, y) {
                        let _ = PostMessageW(
                            Some(state.tray_hwnd),
                            WM_APP_FLYOUT_TOGGLE_MUTE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    } else if pt_in_rect(state.track_rect(dpi), x, y) {
                        state.dragging = true;
                        let _ = SetCapture(hwnd);
                        state.apply_slider_x(x);
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    if state.dragging {
                        state.apply_slider_x(x);
                    } else {
                        let over_mute = pt_in_rect(state.mute_rect(dpi), x, y);
                        if over_mute != state.hover_mute {
                            state.hover_mute = over_mute;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        }
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    if state.dragging {
                        state.dragging = false;
                        let _ = ReleaseCapture();
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = sign_extend_i16_from_wparam(wparam);
                if let Ok(state) = state_rc.try_borrow() {
                    let _ = PostMessageW(
                        Some(state.tray_hwnd),
                        WM_APP_WHEEL_VOLUME,
                        WPARAM(delta as i16 as isize as usize),
                        LPARAM(0),
                    );
                }
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u32;
                if let Ok(state) = state_rc.try_borrow() {
                    match vk {
                        0x1B => {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                        0x25 | 0x28 => {
                            let _ = PostMessageW(
                                Some(state.tray_hwnd),
                                WM_APP_WHEEL_VOLUME,
                                WPARAM((-120i16) as isize as usize),
                                LPARAM(0),
                            );
                        }
                        0x27 | 0x26 => {
                            let _ = PostMessageW(
                                Some(state.tray_hwnd),
                                WM_APP_WHEEL_VOLUME,
                                WPARAM(120usize),
                                LPARAM(0),
                            );
                        }
                        0x4D => {
                            let _ = PostMessageW(
                                Some(state.tray_hwnd),
                                WM_APP_FLYOUT_TOGGLE_MUTE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        _ => {}
                    }
                }
                return LRESULT(0);
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) == 0 {
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.last_deactivated = Some(Instant::now());
                        if state.dragging {
                            state.dragging = false;
                            let _ = ReleaseCapture();
                        }
                    }
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _owned = Rc::from_raw(ptr);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ============================================================================
// Devices Checklist Window ("Устройства")
// ============================================================================

#[derive(Clone)]
struct DeviceRow {
    id: String,
    name: String,
    checked: bool,
    is_default: bool,
}

struct DevicesDialogState {
    hwnd: HWND,
    tray_hwnd: HWND,
    rows: Vec<DeviceRow>,
    hover_row: Option<usize>,
    hover_done: bool,
    scroll_offset: usize,
}

impl DevicesDialogState {
    fn row_rect(&self, visible_idx: usize, w: i32, dpi: u32) -> RECT {
        let top = scale_px(DEVICES_HEADER_H, dpi) + (visible_idx as i32) * scale_px(DEVICES_ROW_H, dpi);
        RECT {
            left: scale_px(12, dpi),
            top,
            right: w - scale_px(12, dpi),
            bottom: top + scale_px(DEVICES_ROW_H - 4, dpi),
        }
    }

    fn done_rect(&self, w: i32, h: i32, dpi: u32) -> RECT {
        let btn_w = scale_px(100, dpi);
        let btn_h = scale_px(30, dpi);
        let right = w - scale_px(16, dpi);
        let bottom = h - scale_px(12, dpi);
        RECT {
            left: right - btn_w,
            top: bottom - btn_h,
            right,
            bottom,
        }
    }

    fn paint(&self, hdc: windows::Win32::Graphics::Gdi::HDC, w: i32, h: i32) {
        let dpi = window_dpi(self.hwnd);
        let colors = ThemeColors::current();

        unsafe {
            let bg_brush = CreateSolidBrush(colors.bg);
            FillRect(
                hdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
                bg_brush,
            );
            let _ = DeleteObject(bg_brush.into());
        }

        let header_font = create_segoe_font(scale_px(13, dpi), false);
        let row_font = create_segoe_font(scale_px(14, dpi), false);
        let btn_font = create_segoe_font(scale_px(13, dpi), true);

        draw_text(
            hdc,
            "Отметьте устройства для меню и цикла 2× клика:",
            RECT {
                left: scale_px(16, dpi),
                top: scale_px(10, dpi),
                right: w - scale_px(16, dpi),
                bottom: scale_px(34, dpi),
            },
            colors.text_muted,
            header_font,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        if self.rows.is_empty() {
            draw_text(
                hdc,
                "Активные устройства вывода не найдены",
                RECT {
                    left: scale_px(16, dpi),
                    top: scale_px(48, dpi),
                    right: w - scale_px(16, dpi),
                    bottom: scale_px(84, dpi),
                },
                colors.text_muted,
                row_font,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
            );
        } else {
            let visible_count = DEVICES_MAX_VISIBLE_ROWS.min(self.rows.len());
            for vis in 0..visible_count {
                let idx = self.scroll_offset + vis;
                let Some(row) = self.rows.get(idx) else {
                    break;
                };
                let r = self.row_rect(vis, w, dpi);
                if self.hover_row == Some(idx) {
                    fill_round_rect(hdc, r, scale_px(6, dpi), colors.surface_hover, None);
                }

                // Checkbox box.
                let box_sz = scale_px(18, dpi);
                let box_x = r.left + scale_px(8, dpi);
                let box_y = (r.top + r.bottom - box_sz) / 2;
                let box_rc = RECT {
                    left: box_x,
                    top: box_y,
                    right: box_x + box_sz,
                    bottom: box_y + box_sz,
                };

                if row.checked {
                    fill_round_rect(hdc, box_rc, scale_px(4, dpi), colors.accent, None);
                    unsafe {
                        let ck_pen = CreatePen(PS_SOLID, scale_px(2, dpi).max(2), colors.checkmark);
                        let old_p = SelectObject(hdc, ck_pen.into());
                        let pts = [
                            POINT {
                                x: box_x + scale_px(4, dpi),
                                y: box_y + scale_px(9, dpi),
                            },
                            POINT {
                                x: box_x + scale_px(8, dpi),
                                y: box_y + scale_px(13, dpi),
                            },
                            POINT {
                                x: box_x + scale_px(14, dpi),
                                y: box_y + scale_px(5, dpi),
                            },
                        ];
                        let _ = Polyline(hdc, &pts);
                        SelectObject(hdc, old_p);
                        let _ = DeleteObject(ck_pen.into());
                    }
                } else {
                    fill_round_rect(
                        hdc,
                        box_rc,
                        scale_px(4, dpi),
                        colors.bg,
                        Some(colors.border),
                    );
                }

                let label = if row.is_default {
                    format!("{}  •", row.name)
                } else {
                    row.name.clone()
                };
                draw_text(
                    hdc,
                    &label,
                    RECT {
                        left: box_rc.right + scale_px(10, dpi),
                        top: r.top,
                        right: r.right - scale_px(8, dpi),
                        bottom: r.bottom,
                    },
                    colors.text_primary,
                    row_font,
                    DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
                );
            }
        }

        // Done button.
        let btn_rc = self.done_rect(w, h, dpi);
        let btn_fill = if self.hover_done {
            colors.btn_hover
        } else {
            colors.btn_bg
        };
        fill_round_rect(hdc, btn_rc, scale_px(6, dpi), btn_fill, None);
        draw_text(
            hdc,
            "Готово",
            btn_rc,
            colors.btn_text,
            btn_font,
            DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
        );

        unsafe {
            let _ = DeleteObject(header_font.into());
            let _ = DeleteObject(row_font.into());
            let _ = DeleteObject(btn_font.into());
        }
    }
}

/// Reusable non-closing checklist dialog for selecting active devices.
pub struct DevicesDialog {
    hwnd: HWND,
    state: Rc<RefCell<DevicesDialogState>>,
}

impl DevicesDialog {
    pub fn create(tray_hwnd: HWND) -> Option<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None).ok()?;
            let class_name = to_wide_null(DEVICES_CLASS);
            let cursor = LoadCursorW(None, IDC_ARROW).ok();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(devices_wnd_proc),
                hInstance: hinstance.into(),
                hCursor: cursor.unwrap_or_default(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let title = to_wide_null("Устройства — Tiny_Sink");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                420,
                240,
                Some(tray_hwnd),
                None,
                Some(hinstance.into()),
                None,
            )
            .ok()?;

            let state = Rc::new(RefCell::new(DevicesDialogState {
                hwnd,
                tray_hwnd,
                rows: Vec::new(),
                hover_row: None,
                hover_done: false,
                scroll_offset: 0,
            }));
            let raw = Rc::into_raw(Rc::clone(&state));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            apply_dwm_style(hwnd, false);

            Some(Self { hwnd, state })
        }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn sync_devices(&self, devices: &[AudioDeviceInfo], config: &AppConfig) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.rows = devices
                .iter()
                .map(|d| DeviceRow {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    checked: config.is_in_cycle(&d.id),
                    is_default: d.is_default,
                })
                .collect();
            let max_scroll = s.rows.len().saturating_sub(DEVICES_MAX_VISIBLE_ROWS);
            s.scroll_offset = s.scroll_offset.min(max_scroll);
        }
        if self.is_visible() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    pub fn show(&self, devices: &[AudioDeviceInfo], config: &AppConfig) {
        apply_dwm_style(self.hwnd, false);
        self.sync_devices(devices, config);

        let dpi = window_dpi(self.hwnd);
        let rows_count = devices.len().clamp(1, DEVICES_MAX_VISIBLE_ROWS) as i32;
        let client_w = scale_px(DEVICES_LOGICAL_W, dpi);
        let client_h = scale_px(DEVICES_HEADER_H + DEVICES_FOOTER_H, dpi)
            + rows_count * scale_px(DEVICES_ROW_H, dpi);
        show_centered_tool_dialog(self.hwnd, client_w, client_h);
    }
}

impl Drop for DevicesDialog {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn devices_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<DevicesDialogState>;
    if !ptr.is_null() {
        Rc::increment_strong_count(ptr);
        let state_rc = Rc::from_raw(ptr);

        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                if let Ok(state) = state_rc.try_borrow() {
                    with_backbuffer(hwnd, |hdc, w, h| state.paint(hdc, w, h));
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    let over_done = pt_in_rect(state.done_rect(w, h, dpi), x, y);
                    let visible_count = DEVICES_MAX_VISIBLE_ROWS.min(state.rows.len());
                    let mut over_row = None;
                    for vis in 0..visible_count {
                        let idx = state.scroll_offset + vis;
                        if idx < state.rows.len() && pt_in_rect(state.row_rect(vis, w, dpi), x, y) {
                            over_row = Some(idx);
                            break;
                        }
                    }
                    if over_done != state.hover_done || over_row != state.hover_row {
                        state.hover_done = over_done;
                        state.hover_row = over_row;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    if pt_in_rect(state.done_rect(w, h, dpi), x, y) {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                        return LRESULT(0);
                    }
                    let visible_count = DEVICES_MAX_VISIBLE_ROWS.min(state.rows.len());
                    for vis in 0..visible_count {
                        let idx = state.scroll_offset + vis;
                        if idx < state.rows.len() && pt_in_rect(state.row_rect(vis, w, dpi), x, y) {
                            let id = state.rows[idx].id.clone();
                            state.rows[idx].checked = !state.rows[idx].checked;
                            let boxed = Box::into_raw(Box::new(id));
                            let _ = PostMessageW(
                                Some(state.tray_hwnd),
                                WM_APP_DEVICES_TOGGLE_ID,
                                WPARAM(0),
                                LPARAM(boxed as isize),
                            );
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            break;
                        }
                    }
                }
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let delta = sign_extend_i16_from_wparam(wparam);
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    let max_scroll = state.rows.len().saturating_sub(DEVICES_MAX_VISIBLE_ROWS);
                    if delta < 0 && state.scroll_offset < max_scroll {
                        state.scroll_offset += 1;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    } else if delta > 0 && state.scroll_offset > 0 {
                        state.scroll_offset -= 1;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u32;
                if vk == 0x1B || vk == 0x0D {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _owned = Rc::from_raw(ptr);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn show_centered_tool_dialog(hwnd: HWND, client_w: i32, client_h: i32) {
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

// ============================================================================
// Update Status / Download Progress Dialog
// ============================================================================

#[derive(Debug, Clone)]
pub enum UpdateDialogView {
    Checking {
        current_version: String,
    },
    UpToDate {
        current_version: String,
    },
    Available {
        current_version: String,
        info: ReleaseInfo,
    },
    Downloading {
        version: String,
        percent: u8,
        downloaded_bytes: u64,
        total_bytes: u64,
        speed_bps: f64,
    },
    Applying {
        version: String,
    },
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateBtnHit {
    Primary,
    Secondary,
}

struct UpdateDialogState {
    hwnd: HWND,
    tray_hwnd: HWND,
    view: UpdateDialogView,
    hover_btn: Option<UpdateBtnHit>,
}

impl UpdateDialogState {
    fn secondary_rect(&self, w: i32, h: i32, dpi: u32) -> Option<RECT> {
        let has_secondary = matches!(
            self.view,
            UpdateDialogView::Available { .. } | UpdateDialogView::Error { .. }
        );
        if !has_secondary {
            return None;
        }
        let btn_w = scale_px(108, dpi);
        let btn_h = scale_px(32, dpi);
        let right = w - scale_px(16, dpi) - btn_w - scale_px(10, dpi);
        let bottom = h - scale_px(14, dpi);
        Some(RECT {
            left: right - btn_w,
            top: bottom - btn_h,
            right,
            bottom,
        })
    }

    fn primary_rect(&self, w: i32, h: i32, dpi: u32) -> Option<RECT> {
        if matches!(self.view, UpdateDialogView::Applying { .. }) {
            return None;
        }
        let btn_w = scale_px(108, dpi);
        let btn_h = scale_px(32, dpi);
        let right = w - scale_px(16, dpi);
        let bottom = h - scale_px(14, dpi);
        Some(RECT {
            left: right - btn_w,
            top: bottom - btn_h,
            right,
            bottom,
        })
    }

    fn button_labels(&self) -> (&'static str, &'static str, bool) {
        match self.view {
            UpdateDialogView::Checking { .. } => ("Скрыть", "", false),
            UpdateDialogView::UpToDate { .. } => ("ОК", "", true),
            UpdateDialogView::Available { .. } => ("Обновить", "Позже", true),
            UpdateDialogView::Downloading { .. } => ("Отмена", "", false),
            UpdateDialogView::Applying { .. } => ("", "", false),
            UpdateDialogView::Error { .. } => ("Повторить", "Закрыть", true),
        }
    }

    fn paint(&self, hdc: windows::Win32::Graphics::Gdi::HDC, w: i32, h: i32) {
        let dpi = window_dpi(self.hwnd);
        let colors = ThemeColors::current();

        unsafe {
            let bg_brush = CreateSolidBrush(colors.bg);
            FillRect(
                hdc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
                bg_brush,
            );
            let _ = DeleteObject(bg_brush.into());
        }

        let title_font = create_segoe_font(scale_px(16, dpi), true);
        let body_font = create_segoe_font(scale_px(13, dpi), false);
        let btn_font = create_segoe_font(scale_px(13, dpi), true);

        let (title, body, progress) = match &self.view {
            UpdateDialogView::Checking { current_version } => (
                "Проверка обновлений...".to_string(),
                format!(
                    "Текущая версия: v{current_version}\nЗапрос последнего релиза на GitHub..."
                ),
                None,
            ),
            UpdateDialogView::UpToDate { current_version } => (
                "Обновления не требуются".to_string(),
                format!("Установлена актуальная версия Tiny_Sink (v{current_version})."),
                None,
            ),
            UpdateDialogView::Available {
                current_version,
                info,
            } => {
                let mut desc = format!(
                    "Текущая версия: v{current_version}  ->  Новая: {}",
                    info.tag_name
                );
                if !info.notes.is_empty() {
                    desc.push_str("\n\n");
                    desc.push_str(&info.notes);
                } else {
                    desc.push_str(&format!("\nФайл релиза: {}", info.exe_name));
                }
                (format!("Доступно обновление {}", info.tag_name), desc, None)
            }
            UpdateDialogView::Downloading {
                version,
                percent,
                downloaded_bytes,
                total_bytes,
                speed_bps,
            } => {
                let dl_mb = (*downloaded_bytes as f64) / (1024.0 * 1024.0);
                let speed = format_speed(*speed_bps);
                let info_line = if *total_bytes > 0 {
                    let total_mb = (*total_bytes as f64) / (1024.0 * 1024.0);
                    format!("Загружено: {dl_mb:.2} МБ из {total_mb:.2} МБ  •  {speed}")
                } else {
                    format!("Загружено: {dl_mb:.2} МБ  •  {speed}")
                };
                (
                    format!("Загрузка обновления v{version}..."),
                    info_line,
                    Some((*percent, *total_bytes > 0)),
                )
            }
            UpdateDialogView::Applying { version } => (
                format!("Установка v{version}..."),
                "Проверка SHA-256 и перезапуск Tiny_Sink...".to_string(),
                Some((100, true)),
            ),
            UpdateDialogView::Error { message } => (
                "Ошибка обновления".to_string(),
                message.clone(),
                None,
            ),
        };

        draw_text(
            hdc,
            &title,
            RECT {
                left: scale_px(18, dpi),
                top: scale_px(16, dpi),
                right: w - scale_px(18, dpi),
                bottom: scale_px(42, dpi),
            },
            colors.text_primary,
            title_font,
            DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        let body_bottom = if progress.is_some() {
            scale_px(94, dpi)
        } else {
            h - scale_px(56, dpi)
        };
        draw_text(
            hdc,
            &body,
            RECT {
                left: scale_px(18, dpi),
                top: scale_px(48, dpi),
                right: w - scale_px(18, dpi),
                bottom: body_bottom,
            },
            colors.text_muted,
            body_font,
            DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        if let Some((pct, known_total)) = progress {
            let bar_rc = RECT {
                left: scale_px(18, dpi),
                top: scale_px(102, dpi),
                right: w - scale_px(18, dpi),
                bottom: scale_px(128, dpi),
            };
            fill_round_rect(
                hdc,
                bar_rc,
                scale_px(8, dpi),
                colors.track_bg,
                Some(colors.border),
            );
            let bar_w = bar_rc.right - bar_rc.left;
            let fill_w = (bar_w * (pct as i32) / 100).clamp(0, bar_w);
            if fill_w > scale_px(4, dpi) {
                fill_round_rect(
                    hdc,
                    RECT {
                        left: bar_rc.left,
                        top: bar_rc.top,
                        right: bar_rc.left + fill_w,
                        bottom: bar_rc.bottom,
                    },
                    scale_px(8, dpi),
                    colors.accent,
                    None,
                );
            }
            let bar_label = if known_total {
                format!("{pct}%")
            } else {
                "Загрузка...".to_string()
            };
            let label_color = if fill_w > bar_w / 2 {
                colors.btn_text
            } else {
                colors.text_primary
            };
            draw_text(
                hdc,
                &bar_label,
                bar_rc,
                label_color,
                btn_font,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
            );
        }

        let (primary_lbl, secondary_lbl, primary_accent) = self.button_labels();
        if let Some(sec_rc) = self.secondary_rect(w, h, dpi) {
            let fill = if self.hover_btn == Some(UpdateBtnHit::Secondary) {
                colors.surface_hover
            } else {
                colors.track_bg
            };
            fill_round_rect(hdc, sec_rc, scale_px(6, dpi), fill, Some(colors.border));
            draw_text(
                hdc,
                secondary_lbl,
                sec_rc,
                colors.text_primary,
                btn_font,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
            );
        }

        if let Some(pri_rc) = self.primary_rect(w, h, dpi) {
            let (fill, text_col, border) = if primary_accent {
                let f = if self.hover_btn == Some(UpdateBtnHit::Primary) {
                    colors.btn_hover
                } else {
                    colors.btn_bg
                };
                (f, colors.btn_text, None)
            } else {
                let f = if self.hover_btn == Some(UpdateBtnHit::Primary) {
                    colors.surface_hover
                } else {
                    colors.track_bg
                };
                (f, colors.text_primary, Some(colors.border))
            };
            fill_round_rect(hdc, pri_rc, scale_px(6, dpi), fill, border);
            draw_text(
                hdc,
                primary_lbl,
                pri_rc,
                text_col,
                btn_font,
                DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
            );
        }

        unsafe {
            let _ = DeleteObject(title_font.into());
            let _ = DeleteObject(body_font.into());
            let _ = DeleteObject(btn_font.into());
        }
    }

    fn handle_primary_action(&self) {
        match &self.view {
            UpdateDialogView::Checking { .. } | UpdateDialogView::UpToDate { .. } => unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            },
            UpdateDialogView::Available { .. } => unsafe {
                let _ = PostMessageW(
                    Some(self.tray_hwnd),
                    WM_APP_UPDATE_START_DOWNLOAD,
                    WPARAM(0),
                    LPARAM(0),
                );
            },
            UpdateDialogView::Downloading { .. } => unsafe {
                let _ = PostMessageW(
                    Some(self.tray_hwnd),
                    WM_APP_UPDATE_CANCEL_DOWNLOAD,
                    WPARAM(0),
                    LPARAM(0),
                );
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            },
            UpdateDialogView::Applying { .. } => {}
            UpdateDialogView::Error { .. } => unsafe {
                let _ = PostMessageW(
                    Some(self.tray_hwnd),
                    WM_APP_UPDATE_RETRY_CHECK,
                    WPARAM(0),
                    LPARAM(0),
                );
            },
        }
    }

    fn handle_close_or_escape(&self) {
        if matches!(self.view, UpdateDialogView::Downloading { .. }) {
            unsafe {
                let _ = PostMessageW(
                    Some(self.tray_hwnd),
                    WM_APP_UPDATE_CANCEL_DOWNLOAD,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        if !matches!(self.view, UpdateDialogView::Applying { .. }) {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }
}

fn format_speed(speed_bps: f64) -> String {
    if speed_bps <= 1.0 {
        "вычисляется...".to_string()
    } else if speed_bps < 1024.0 {
        format!("{speed_bps:.0} Б/с")
    } else if speed_bps < 1024.0 * 1024.0 {
        format!("{:.1} КБ/с", speed_bps / 1024.0)
    } else {
        format!("{:.1} МБ/с", speed_bps / (1024.0 * 1024.0))
    }
}

/// Reusable themed dialog for update checks, prompts, download progress, and errors.
pub struct UpdateDialog {
    hwnd: HWND,
    state: Rc<RefCell<UpdateDialogState>>,
}

impl UpdateDialog {
    pub fn create(tray_hwnd: HWND) -> Option<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None).ok()?;
            let class_name = to_wide_null(UPDATE_CLASS);
            let cursor = LoadCursorW(None, IDC_ARROW).ok();
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(update_wnd_proc),
                hInstance: hinstance.into(),
                hCursor: cursor.unwrap_or_default(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassExW(&wc);

            let title = to_wide_null("Обновление — Tiny_Sink");
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                UPDATE_LOGICAL_W,
                UPDATE_LOGICAL_H,
                Some(tray_hwnd),
                None,
                Some(hinstance.into()),
                None,
            )
            .ok()?;

            let state = Rc::new(RefCell::new(UpdateDialogState {
                hwnd,
                tray_hwnd,
                view: UpdateDialogView::Checking {
                    current_version: String::new(),
                },
                hover_btn: None,
            }));
            let raw = Rc::into_raw(Rc::clone(&state));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            apply_dwm_style(hwnd, false);

            Some(Self { hwnd, state })
        }
    }

    pub fn is_visible(&self) -> bool {
        unsafe { IsWindowVisible(self.hwnd).as_bool() }
    }

    pub fn set_view(&self, view: UpdateDialogView, bring_to_front: bool) {
        if let Ok(mut s) = self.state.try_borrow_mut() {
            s.view = view;
            s.hover_btn = None;
        }
        apply_dwm_style(self.hwnd, false);
        if bring_to_front || !self.is_visible() {
            let dpi = window_dpi(self.hwnd);
            show_centered_tool_dialog(
                self.hwnd,
                scale_px(UPDATE_LOGICAL_W, dpi),
                scale_px(UPDATE_LOGICAL_H, dpi),
            );
        } else {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }

    pub fn refresh_theme(&self) {
        apply_dwm_style(self.hwnd, false);
        if self.is_visible() {
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }
}

impl Drop for UpdateDialog {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn update_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<UpdateDialogState>;
    if !ptr.is_null() {
        Rc::increment_strong_count(ptr);
        let state_rc = Rc::from_raw(ptr);

        match msg {
            WM_ERASEBKGND => return LRESULT(1),
            WM_PAINT => {
                if let Ok(state) = state_rc.try_borrow() {
                    with_backbuffer(hwnd, |hdc, w, h| state.paint(hdc, w, h));
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    let hit = if state
                        .primary_rect(w, h, dpi)
                        .is_some_and(|r| pt_in_rect(r, x, y))
                    {
                        Some(UpdateBtnHit::Primary)
                    } else if state
                        .secondary_rect(w, h, dpi)
                        .is_some_and(|r| pt_in_rect(r, x, y))
                    {
                        Some(UpdateBtnHit::Secondary)
                    } else {
                        None
                    };
                    if hit != state.hover_btn {
                        state.hover_btn = hit;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = window_dpi(hwnd);
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let w = rc.right - rc.left;
                let h = rc.bottom - rc.top;

                if let Ok(state) = state_rc.try_borrow() {
                    if state
                        .primary_rect(w, h, dpi)
                        .is_some_and(|r| pt_in_rect(r, x, y))
                    {
                        state.handle_primary_action();
                    } else if state
                        .secondary_rect(w, h, dpi)
                        .is_some_and(|r| pt_in_rect(r, x, y))
                    {
                        let _ = ShowWindow(hwnd, SW_HIDE);
                    }
                }
                return LRESULT(0);
            }
            WM_KEYDOWN => {
                let vk = wparam.0 as u32;
                if let Ok(state) = state_rc.try_borrow() {
                    if vk == 0x1B {
                        state.handle_close_or_escape();
                    } else if vk == 0x0D {
                        state.handle_primary_action();
                    }
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                if let Ok(state) = state_rc.try_borrow() {
                    state.handle_close_or_escape();
                }
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _owned = Rc::from_raw(ptr);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

