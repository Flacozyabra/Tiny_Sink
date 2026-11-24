//! LMB single-click custom-painted volume slider flyout near the system tray.

use super::{
    apply_dwm_style, create_segoe_font, draw_text, fill_round_rect, pt_in_rect, scale_px,
    sign_extend_i16_from_wparam, window_dpi, with_backbuffer, ThemeColors,
    WM_APP_FLYOUT_SET_VOLUME, WM_APP_FLYOUT_TOGGLE_MUTE, WM_APP_WHEEL_VOLUME,
};
use crate::config::{to_wide_null, DeviceKind};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, Ellipse, FillRect, GetMonitorInfoW, InvalidateRect,
    MonitorFromPoint, Polygon, Polyline, SelectObject, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC, MONITORINFO, MONITOR_DEFAULTTONEAREST, PS_NULL,
    PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetWindowLongPtrW,
    IsWindowVisible, LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, HWND_TOPMOST,
    IDC_ARROW, SWP_SHOWWINDOW, SW_HIDE, WM_ACTIVATE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

const FLYOUT_CLASS: &str = "TinySinkVolumeFlyout";
const FLYOUT_LOGICAL_W: i32 = 324;
const FLYOUT_LOGICAL_H: i32 = 92;

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

    fn paint(&self, hdc: HDC, w: i32, h: i32) {
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
    hdc: HDC,
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
                        WPARAM(delta as isize as usize),
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

