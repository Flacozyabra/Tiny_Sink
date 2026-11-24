//! Win32 tray lifecycle, icon updates, LMB flyout, double-click switching, and message dispatch.

mod menu;
mod theme;
mod update_flow;
mod wheel_hook;

use crate::audio::{
    AudioDeviceInfo, AudioMonitor, AudioSnapshot, WM_APP_DEVICES_CHANGED, WM_APP_VOLUME_CHANGED,
};
use crate::config::{default_config_path, load_config, save_config, to_wide_null, AppConfig, DeviceKind, EndpointSummary};
use crate::icon::create_tray_hicon;
use crate::policy_config::set_default_audio_endpoint;
use crate::ui::{
    DevicesDialog, UpdateDialog, VolumeFlyout, WM_APP_DEVICES_TOGGLE_ID, WM_APP_FLYOUT_SET_VOLUME,
    WM_APP_FLYOUT_TOGGLE_MUTE, WM_APP_UPDATE_CANCEL_DOWNLOAD, WM_APP_UPDATE_RETRY_CHECK,
    WM_APP_UPDATE_START_DOWNLOAD, WM_APP_WHEEL_VOLUME,
};
use crate::updater::{
    DownloadHandle, DownloadOutcome, DownloadProgress, ReleaseInfo, UpdateCheckOutcome,
    WM_APP_UPDATE_CHECK_DONE, WM_APP_UPDATE_DOWNLOAD_DONE, WM_APP_UPDATE_PROGRESS,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use theme::{enable_adaptive_dark_mode, is_light_taskbar_theme, query_tray_icon_size};
use wheel_hook::{install_wheel_hook, uninstall_wheel_hook, wheel_step_from_delta};
use windows::core::{PCWSTR, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DispatchMessageW, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    TrackPopupMenuEx, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, HHOOK, HICON, IDC_ARROW, MSG,
    SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_DESTROY, WM_DPICHANGED, WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_TRAY: u32 = WM_APP + 10;
pub(super) const TRAY_ICON_ID: u32 = 1;
const TIMER_LMB_SINGLE_CLICK: usize = 1;
pub(super) const TIMER_AUTO_UPDATE_STARTUP: usize = 2;
pub(super) const TIMER_AUTO_UPDATE_PERIODIC: usize = 3;
pub(super) const AUTO_UPDATE_STARTUP_DELAY_MS: u32 = 15_000;
pub(super) const AUTO_UPDATE_INTERVAL_MS: u32 = 86_400_000;

const WINDOW_CLASS_NAME: &str = "TinySinkTrayWindow";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    kind: DeviceKind,
    volume: u8,
    muted: bool,
    light_taskbar: bool,
    icon_size: i32,
}

pub(super) struct TrayState {
    pub(super) hwnd: HWND,
    taskbar_created_msg: u32,
    config_path: Option<PathBuf>,
    pub(super) config: AppConfig,
    audio: AudioMonitor,
    pub(super) snapshot: AudioSnapshot,
    current_hicon: Option<HICON>,
    last_render_key: Option<RenderKey>,
    last_tooltip: String,
    pub(super) icon_added: bool,
    mouse_hook: Option<HHOOK>,
    flyout: Option<VolumeFlyout>,
    pub(super) devices_dialog: Option<DevicesDialog>,
    pub(super) update_dialog: Option<UpdateDialog>,
    pub(super) available_update: Option<ReleaseInfo>,
    pub(super) notified_tag: Option<String>,
    pub(super) update_checking: bool,
    pub(super) pending_manual_feedback: bool,
    pub(super) download_handle: Option<DownloadHandle>,
    pub(super) download_gen: usize,
    pub(super) applying_update: bool,
    ignore_next_lbutton_up: bool,
    suppress_flyout_on_up: bool,
}

impl TrayState {
    fn new(hwnd: HWND, taskbar_created_msg: u32) -> Result<Self> {
        let config_path = default_config_path();
        let mut config = config_path
            .as_deref()
            .map(load_config)
            .unwrap_or_default();

        let mut audio = AudioMonitor::new(hwnd)?;
        let snapshot = audio.snapshot();

        let summaries: Vec<EndpointSummary> = snapshot
            .devices
            .iter()
            .map(AudioDeviceInfo::to_summary)
            .collect();
        if config.ensure_cycle_initialized(&summaries) {
            if let Some(path) = config_path.as_deref() {
                let _ = save_config(path, &config);
            }
        }

        let mouse_hook = install_wheel_hook(hwnd);
        let flyout = VolumeFlyout::create(hwnd);
        let devices_dialog = DevicesDialog::create(hwnd);
        let update_dialog = UpdateDialog::create(hwnd);

        let mut state = Self {
            hwnd,
            taskbar_created_msg,
            config_path,
            config,
            audio,
            snapshot,
            current_hicon: None,
            last_render_key: None,
            last_tooltip: String::new(),
            icon_added: false,
            mouse_hook,
            flyout,
            devices_dialog,
            update_dialog,
            available_update: None,
            notified_tag: None,
            update_checking: false,
            pending_manual_feedback: false,
            download_handle: None,
            download_gen: 0,
            applying_update: false,
            ignore_next_lbutton_up: false,
            suppress_flyout_on_up: false,
        };
        state.sync_child_windows();
        state.update_tray_icon(true);
        state.configure_auto_update_timers();
        Ok(state)
    }

    pub(super) fn save_config_to_disk(&self) {
        if let Some(path) = self.config_path.as_deref() {
            let _ = save_config(path, &self.config);
        }
    }

    pub(super) fn sync_child_windows(&self) {
        let kind = self.effective_default_kind();
        if let Some(flyout) = &self.flyout {
            flyout.sync_state(
                &self.snapshot.default_device_name,
                kind,
                self.snapshot.volume_percent,
                self.snapshot.muted,
            );
        }
        if let Some(dlg) = &self.devices_dialog {
            dlg.sync_devices(&self.snapshot.devices, &self.config);
        }
        if let Some(upd) = &self.update_dialog {
            upd.refresh_theme();
        }
    }

    pub(super) fn refresh_audio_and_tray(&mut self, force_icon: bool) {
        self.snapshot = self.audio.snapshot();
        let summaries: Vec<EndpointSummary> = self
            .snapshot
            .devices
            .iter()
            .map(AudioDeviceInfo::to_summary)
            .collect();
        if self.config.ensure_cycle_initialized(&summaries) {
            self.save_config_to_disk();
        }
        self.sync_child_windows();
        self.update_tray_icon(force_icon);
    }

    fn refresh_volume_only(&mut self) {
        let (vol, muted) = self.audio.read_volume_and_mute();
        self.snapshot.volume_percent = vol;
        self.snapshot.muted = muted;
        self.sync_child_windows();
        self.update_tray_icon(false);
    }

    fn effective_default_kind(&self) -> DeviceKind {
        match self.snapshot.default_device_id.as_deref() {
            Some(id) => self.config.effective_kind(id, self.snapshot.detected_kind),
            None => self.snapshot.detected_kind,
        }
    }

    pub(super) fn update_tray_icon(&mut self, force: bool) {
        let kind = self.effective_default_kind();
        let volume = self.snapshot.volume_percent;
        let muted = self.snapshot.muted;
        let light_taskbar = is_light_taskbar_theme();
        let icon_size = query_tray_icon_size(self.hwnd);

        let key = RenderKey {
            kind,
            volume,
            muted,
            light_taskbar,
            icon_size,
        };

        let tooltip = build_tooltip(&self.snapshot, kind);

        if !force
            && self.icon_added
            && self.last_render_key.as_ref() == Some(&key)
            && self.last_tooltip == tooltip
        {
            return;
        }

        let need_new_icon =
            force || !self.icon_added || self.last_render_key.as_ref() != Some(&key);
        let mut new_hicon: Option<HICON> = None;

        if need_new_icon {
            match create_tray_hicon(icon_size, kind, volume, muted, light_taskbar) {
                Ok(hicon) => new_hicon = Some(hicon),
                Err(_) => return,
            }
        }

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_TIP | NIF_ICON,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: new_hicon.or(self.current_hicon).unwrap_or_default(),
            ..Default::default()
        };
        write_utf16_slice(&mut nid.szTip, &tooltip);

        let action = if self.icon_added { NIM_MODIFY } else { NIM_ADD };
        let ok = unsafe { Shell_NotifyIconW(action, &nid).as_bool() };
        if ok {
            self.icon_added = true;
            self.last_render_key = Some(key);
            self.last_tooltip = tooltip;
            if let Some(hicon) = new_hicon {
                if let Some(old) = self.current_hicon.replace(hicon) {
                    unsafe {
                        let _ = DestroyIcon(old);
                    }
                }
            }
        } else if let Some(hicon) = new_hicon {
            unsafe {
                let _ = DestroyIcon(hicon);
            }
        }
    }

    fn show_volume_flyout(&mut self) {
        self.refresh_audio_and_tray(false);
        if let Some(flyout) = &self.flyout {
            flyout.show_near_tray(self.hwnd, TRAY_ICON_ID);
        }
    }

    fn hide_volume_flyout(&self) {
        if let Some(flyout) = &self.flyout {
            flyout.hide();
        }
    }

    fn is_flyout_visible(&self) -> bool {
        self.flyout.as_ref().is_some_and(VolumeFlyout::is_visible)
    }

    fn flyout_was_recently_deactivated(&self) -> bool {
        self.flyout
            .as_ref()
            .is_some_and(|f| f.was_recently_deactivated(Duration::from_millis(250)))
    }

    fn cycle_next_device(&mut self) {
        self.snapshot = self.audio.snapshot();
        let active_ids: Vec<String> = self.snapshot.devices.iter().map(|d| d.id.clone()).collect();
        if let Some(target_id) = self
            .config
            .next_cycle_target(&active_ids, self.snapshot.default_device_id.as_deref())
        {
            if set_default_audio_endpoint(&target_id).is_ok() {
                self.refresh_audio_and_tray(false);
            }
        }
    }

    pub(super) fn cleanup_resources(&mut self) {
        if let Some(handle) = self.download_handle.take() {
            handle.cancel();
        }
        uninstall_wheel_hook(&mut self.mouse_hook);
        self.remove_tray_icon();
    }

    fn remove_tray_icon(&mut self) {
        if self.icon_added {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: TRAY_ICON_ID,
                ..Default::default()
            };
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.icon_added = false;
        }
        if let Some(hicon) = self.current_hicon.take() {
            unsafe {
                let _ = DestroyIcon(hicon);
            }
        }
    }
}

impl Drop for TrayState {
    fn drop(&mut self) {
        self.cleanup_resources();
    }
}

pub(super) fn open_shell_target(target_uri: &str) {
    let verb = to_wide_null("open");
    let target = to_wide_null(target_uri);
    unsafe {
        // Use modern UWP Settings (SystemSettings.exe) instead of legacy taskbarcpl.dll
        // which triggers an explorer.exe crash on minute ticks in Windows 10 19041.
        let _ = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn single_click_delay_ms() -> u32 {
    unsafe { GetDoubleClickTime() }.clamp(350, 500)
}

fn build_tooltip(snapshot: &AudioSnapshot, kind: DeviceKind) -> String {
    let kind_label = match kind {
        DeviceKind::Headphones => "Наушники",
        DeviceKind::Speakers => "Колонки",
    };
    if snapshot.muted {
        format!(
            "{}% (Без звука) — {} • {}",
            snapshot.volume_percent, kind_label, snapshot.default_device_name
        )
    } else {
        format!(
            "{}% — {} • {}",
            snapshot.volume_percent, kind_label, snapshot.default_device_name
        )
    }
}

pub(super) fn write_utf16_slice(dest: &mut [u16], text: &str) {
    dest.fill(0);
    if dest.is_empty() {
        return;
    }
    let mut written = 0usize;
    for unit in text.encode_utf16() {
        if written >= dest.len() - 1 {
            break;
        }
        dest[written] = unit;
        written += 1;
    }
    if written > 0 && (0xD800..=0xDBFF).contains(&dest[written - 1]) {
        dest[written - 1] = 0;
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<TrayState>;

    if !ptr.is_null() {
        Rc::increment_strong_count(ptr);
        let state_rc = Rc::from_raw(ptr);

        let taskbar_match = if let Ok(state) = state_rc.try_borrow() {
            msg == state.taskbar_created_msg && state.taskbar_created_msg != 0
        } else {
            false
        };

        if taskbar_match {
            if let Ok(mut state) = state_rc.try_borrow_mut() {
                state.icon_added = false;
                state.update_tray_icon(true);
            }
            return LRESULT(0);
        }

        match msg {
            WM_APP_VOLUME_CHANGED => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.refresh_volume_only();
                }
                return LRESULT(0);
            }
            WM_APP_DEVICES_CHANGED => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.refresh_audio_and_tray(false);
                }
                return LRESULT(0);
            }
            WM_APP_FLYOUT_SET_VOLUME => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    let pct = wparam.0.min(100) as u8;
                    state.audio.set_volume_percent(pct, true);
                    state.refresh_volume_only();
                }
                return LRESULT(0);
            }
            WM_APP_FLYOUT_TOGGLE_MUTE => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.audio.toggle_mute();
                    state.refresh_volume_only();
                }
                return LRESULT(0);
            }
            WM_APP_WHEEL_VOLUME => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    let delta = (wparam.0 as isize) as i16;
                    let step = wheel_step_from_delta(delta);
                    state.audio.adjust_volume_percent(step);
                    state.refresh_volume_only();
                }
                return LRESULT(0);
            }
            WM_APP_DEVICES_TOGGLE_ID => {
                if lparam.0 != 0 {
                    let device_id = *Box::from_raw(lparam.0 as *mut String);
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.config.toggle_cycle_device(&device_id);
                        state.save_config_to_disk();
                        state.sync_child_windows();
                    }
                }
                return LRESULT(0);
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_LMB_SINGLE_CLICK => {
                        let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            state.show_volume_flyout();
                        }
                    }
                    TIMER_AUTO_UPDATE_STARTUP => {
                        let _ = KillTimer(Some(hwnd), TIMER_AUTO_UPDATE_STARTUP);
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            if state.config.auto_check_updates {
                                state.start_update_check(false);
                            }
                        }
                    }
                    TIMER_AUTO_UPDATE_PERIODIC => {
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            if state.config.auto_check_updates {
                                state.start_update_check(false);
                            }
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_CHECK_DONE => {
                if lparam.0 != 0 {
                    let outcome = *Box::from_raw(lparam.0 as *mut UpdateCheckOutcome);
                    let manual = wparam.0 != 0;
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.handle_update_check_done(manual, outcome);
                    }
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_START_DOWNLOAD => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.start_update_download();
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_CANCEL_DOWNLOAD => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.cancel_update_download();
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_RETRY_CHECK => {
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.start_update_check(true);
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_PROGRESS => {
                if lparam.0 != 0 {
                    let prog = *Box::from_raw(lparam.0 as *mut DownloadProgress);
                    let gen_id = wparam.0;
                    if let Ok(state) = state_rc.try_borrow() {
                        state.handle_download_progress(gen_id, prog);
                    }
                }
                return LRESULT(0);
            }
            WM_APP_UPDATE_DOWNLOAD_DONE => {
                if lparam.0 != 0 {
                    let outcome = *Box::from_raw(lparam.0 as *mut DownloadOutcome);
                    let gen_id = wparam.0;
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.handle_download_done(gen_id, outcome);
                    }
                }
                return LRESULT(0);
            }
            WM_SETTINGCHANGE | WM_DPICHANGED => {
                if msg == WM_SETTINGCHANGE {
                    enable_adaptive_dark_mode(Some(hwnd));
                }
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.sync_child_windows();
                    state.update_tray_icon(false);
                }
                return LRESULT(0);
            }
            WM_APP_TRAY => {
                let mouse_event = (lparam.0 as u32) & 0xFFFF;
                match mouse_event {
                    WM_LBUTTONDOWN => {
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            state.ignore_next_lbutton_up = false;
                            state.suppress_flyout_on_up =
                                state.is_flyout_visible() || state.flyout_was_recently_deactivated();
                        }
                    }
                    WM_LBUTTONUP => {
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            if state.ignore_next_lbutton_up {
                                state.ignore_next_lbutton_up = false;
                                state.suppress_flyout_on_up = false;
                            } else if state.suppress_flyout_on_up {
                                state.suppress_flyout_on_up = false;
                                let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                                state.hide_volume_flyout();
                            } else {
                                let _ = SetTimer(
                                    Some(hwnd),
                                    TIMER_LMB_SINGLE_CLICK,
                                    single_click_delay_ms(),
                                    None,
                                );
                            }
                        }
                    }
                    WM_LBUTTONDBLCLK => {
                        let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            // Suppress the trailing WM_LBUTTONUP from releasing the second click.
                            state.ignore_next_lbutton_up = true;
                            state.suppress_flyout_on_up = false;
                            state.hide_volume_flyout();
                            state.cycle_next_device();
                        }
                    }
                    NIN_BALLOONUSERCLICK => {
                        if let Ok(state) = state_rc.try_borrow() {
                            state.open_available_update_dialog();
                        }
                    }
                    WM_RBUTTONUP => {
                        let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            state.ignore_next_lbutton_up = false;
                            state.suppress_flyout_on_up = false;
                            state.hide_volume_flyout();
                        }
                        let built = state_rc
                            .try_borrow_mut()
                            .ok()
                            .and_then(|mut s| s.build_context_menu());
                        if let Some((menu, actions)) = built {
                            let mut pt = POINT::default();
                            let _ = GetCursorPos(&mut pt);
                            let _ = SetForegroundWindow(hwnd);
                            let cmd = TrackPopupMenuEx(
                                menu,
                                (TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD)
                                    .0,
                                pt.x,
                                pt.y,
                                hwnd,
                                None,
                            );
                            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
                            let _ = DestroyMenu(menu);

                            if cmd.as_bool() {
                                let picked_id = cmd.0 as u32;
                                if let Some((_, action)) =
                                    actions.into_iter().find(|(id, _)| *id == picked_id)
                                {
                                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                                        state.execute_menu_action(action);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_QUERYENDSESSION => return LRESULT(1),
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.cleanup_resources();
                    }
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                let _ = KillTimer(Some(hwnd), TIMER_AUTO_UPDATE_STARTUP);
                let _ = KillTimer(Some(hwnd), TIMER_AUTO_UPDATE_PERIODIC);
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.cleanup_resources();
                }
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _owned = Rc::from_raw(ptr);
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Starts the hidden tray window and runs the Win32 message loop until exit.
pub fn run_app() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        enable_adaptive_dark_mode(None);

        let hinstance = GetModuleHandleW(None)?;
        let class_name = to_wide_null(WINDOW_CLASS_NAME);
        let cursor = LoadCursorW(None, IDC_ARROW).ok();

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hCursor: cursor.unwrap_or_default(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };

        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let title = to_wide_null("Tiny_Sink");
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;
        enable_adaptive_dark_mode(Some(hwnd));

        let taskbar_msg_name = to_wide_null("TaskbarCreated");
        let taskbar_created_msg = RegisterWindowMessageW(PCWSTR(taskbar_msg_name.as_ptr()));

        let state = Rc::new(RefCell::new(TrayState::new(hwnd, taskbar_created_msg)?));
        let state_ptr = Rc::into_raw(state);

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}

