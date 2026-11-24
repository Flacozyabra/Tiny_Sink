//! Themed Win32 dialog for update checks, prompts, download progress, and errors.

use super::{
    apply_dwm_style, create_segoe_font, draw_text, fill_round_rect, pt_in_rect, scale_px,
    show_centered_tool_dialog, window_dpi, with_backbuffer, ThemeColors,
    WM_APP_UPDATE_CANCEL_DOWNLOAD, WM_APP_UPDATE_RETRY_CHECK, WM_APP_UPDATE_START_DOWNLOAD,
};
use crate::config::to_wide_null;
use crate::updater::ReleaseInfo;
use std::cell::RefCell;
use std::rc::Rc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, InvalidateRect, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, HDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    IsWindowVisible, LoadCursorW, PostMessageW, RegisterClassExW, SetWindowLongPtrW, ShowWindow,
    CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, SW_HIDE, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WNDCLASSEXW, WS_CAPTION,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_OVERLAPPED, WS_SYSMENU,
};

const UPDATE_CLASS: &str = "TinySinkUpdateDialog";
const UPDATE_LOGICAL_W: i32 = 430;
const UPDATE_LOGICAL_H: i32 = 204;

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
            UpdateDialogView::Error { message } => ("Ошибка обновления".to_string(), message.clone(), None),
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

