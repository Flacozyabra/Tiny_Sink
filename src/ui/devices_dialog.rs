//! Non-closing checklist dialog for selecting managed playback devices.

use super::{
    apply_dwm_style, create_segoe_font, draw_text, fill_round_rect, pt_in_rect, scale_px,
    show_centered_tool_dialog, sign_extend_i16_from_wparam, window_dpi, with_backbuffer,
    ThemeColors, WM_APP_DEVICES_TOGGLE_ID,
};
use crate::audio::AudioDeviceInfo;
use crate::config::{to_wide_null, AppConfig};
use std::cell::RefCell;
use std::rc::Rc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, FillRect, InvalidateRect, Polyline, SelectObject,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    IsWindowVisible, LoadCursorW, PostMessageW, RegisterClassExW, SetWindowLongPtrW, ShowWindow,
    CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, SW_HIDE, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WNDCLASSEXW,
    WS_CAPTION, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPED, WS_SYSMENU,
};

const DEVICES_CLASS: &str = "TinySinkDevicesDialog";
const DEVICES_LOGICAL_W: i32 = 420;
const DEVICES_ROW_H: i32 = 36;
const DEVICES_HEADER_H: i32 = 40;
const DEVICES_FOOTER_H: i32 = 52;
const DEVICES_MAX_VISIBLE_ROWS: usize = 8;

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

    fn paint(&self, hdc: HDC, w: i32, h: i32) {
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

