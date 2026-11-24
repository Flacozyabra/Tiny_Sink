//! Win32 tray lifecycle, LMB volume flyout, mouse-wheel hook, double-click switching, and RMB menu.

use crate::audio::{
    AudioDeviceInfo, AudioMonitor, AudioSnapshot, WM_APP_DEVICES_CHANGED, WM_APP_VOLUME_CHANGED,
};
use crate::config::{
    default_config_path, is_autostart_enabled, load_config, save_config, set_autostart_enabled,
    to_wide_null, AppConfig, DeviceKind, EndpointSummary, IconOverride,
};
use crate::icon::create_tray_hicon;
use crate::policy_config::set_default_audio_endpoint;
use crate::ui::{
    DevicesDialog, VolumeFlyout, WM_APP_DEVICES_TOGGLE_ID, WM_APP_FLYOUT_SET_VOLUME,
    WM_APP_FLYOUT_TOGGLE_MUTE, WM_APP_WHEEL_VOLUME,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{BOOL, GUID, PCSTR, PCWSTR, Result};
use windows::Win32::Foundation::{FreeLibrary, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::PtInRect;
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CallNextHookEx, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetClassNameW, GetCursorPos, GetMessageW,
    KillTimer, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SetForegroundWindow, SetTimer, SetWindowsHookExW, TrackPopupMenuEx,
    TranslateMessage, UnhookWindowsHookEx, WindowFromPoint, CW_USEDEFAULT, HHOOK, HICON, HMENU,
    IDC_ARROW, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG,
    MSLLHOOKSTRUCT, SM_CXSMICON, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WH_MOUSE_LL, WM_APP, WM_DESTROY, WM_DPICHANGED, WM_ENDSESSION,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP,
    WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_TRAY: u32 = WM_APP + 10;
const TRAY_ICON_ID: u32 = 1;
const TIMER_LMB_SINGLE_CLICK: usize = 1;
const LMB_FLYOUT_DELAY_MS: u32 = 350;

const WINDOW_CLASS_NAME: &str = "TinySinkTrayWindow";
const PERSONALIZE_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const LIGHT_TASKBAR_VALUE: &str = "SystemUsesLightTheme";

const CMD_OPEN_DEVICES: u32 = 4001;
const CMD_OPEN_WIN_TRAY_SETTINGS: u32 = 4002;
const CMD_TOGGLE_AUTOSTART: u32 = 4003;
const CMD_EXIT: u32 = 4004;

/// Target tray window handle read by the low-level mouse hook for wheel scrolling.
static HOOK_TRAY_HWND: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderKey {
    kind: DeviceKind,
    volume: u8,
    muted: bool,
    light_taskbar: bool,
    icon_size: i32,
}

enum MenuAction {
    SelectDevice(String),
    OpenDevicesDialog,
    SetIconOverride(String, IconOverride),
    OpenSystemVolumeSettings,
    ToggleAutostart,
    Exit,
}

struct TrayState {
    hwnd: HWND,
    taskbar_created_msg: u32,
    config_path: Option<PathBuf>,
    config: AppConfig,
    audio: AudioMonitor,
    snapshot: AudioSnapshot,
    current_hicon: Option<HICON>,
    last_render_key: Option<RenderKey>,
    last_tooltip: String,
    icon_added: bool,
    mouse_hook: Option<HHOOK>,
    flyout: Option<VolumeFlyout>,
    devices_dialog: Option<DevicesDialog>,
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

        HOOK_TRAY_HWND.store(hwnd.0 as usize, Ordering::Release);
        let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), None, 0).ok() };
        let flyout = VolumeFlyout::create(hwnd);
        let devices_dialog = DevicesDialog::create(hwnd);

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
        };
        state.sync_child_windows();
        state.update_tray_icon(true);
        Ok(state)
    }

    fn save_config_to_disk(&self) {
        if let Some(path) = self.config_path.as_deref() {
            let _ = save_config(path, &self.config);
        }
    }

    fn sync_child_windows(&self) {
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
    }

    fn refresh_audio_and_tray(&mut self, force_icon: bool) {
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

    fn update_tray_icon(&mut self, force: bool) {
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
        write_tooltip_utf16(&mut nid.szTip, &tooltip);

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

    fn build_context_menu(&mut self) -> Option<(HMENU, Vec<(u32, MenuAction)>)> {
        self.snapshot = self.audio.snapshot();
        let mut actions: Vec<(u32, MenuAction)> = Vec::new();
        let mut next_id: u32 = 1000;

        let selected_devices: Vec<&AudioDeviceInfo> = self
            .snapshot
            .devices
            .iter()
            .filter(|d| self.config.is_in_cycle(&d.id))
            .collect();

        unsafe {
            let menu = CreatePopupMenu().ok()?;

            if selected_devices.is_empty() {
                let msg = if self.snapshot.devices.is_empty() {
                    "Нет активных устройств вывода"
                } else {
                    "Устройства не выбраны (см. «Устройства...»)"
                };
                let wide = to_wide_null(msg);
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(wide.as_ptr()));
            } else {
                for dev in &selected_devices {
                    let cmd_id = next_id;
                    next_id += 1;
                    let kind = self.config.effective_kind(&dev.id, dev.detected_kind);
                    let kind_tag = match kind {
                        DeviceKind::Headphones => "Наушники",
                        DeviceKind::Speakers => "Колонки",
                    };
                    let label = format!("{} [{}]", dev.name, kind_tag);
                    let wide = to_wide_null(&label);
                    let flags = MF_STRING | if dev.is_default { MF_CHECKED } else { MF_UNCHECKED };
                    let _ = AppendMenuW(menu, flags, cmd_id as usize, PCWSTR(wide.as_ptr()));
                    actions.push((cmd_id, MenuAction::SelectDevice(dev.id.clone())));
                }
            }

            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

            // Non-closing checklist dialog for selecting managed devices.
            let devices_label = to_wide_null("Устройства...");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_OPEN_DEVICES as usize,
                PCWSTR(devices_label.as_ptr()),
            );
            actions.push((CMD_OPEN_DEVICES, MenuAction::OpenDevicesDialog));

            // Submenu: manual icon override per selected (or active) device.
            let icon_targets: &[&AudioDeviceInfo] = &selected_devices;
            if let Ok(icons_menu) = CreatePopupMenu() {
                let fallback_all: Vec<&AudioDeviceInfo>;
                let list = if icon_targets.is_empty() {
                    fallback_all = self.snapshot.devices.iter().collect();
                    &fallback_all[..]
                } else {
                    icon_targets
                };

                for dev in list {
                    if let Ok(dev_menu) = CreatePopupMenu() {
                        let current_ovr = self.config.override_for(&dev.id);
                        let auto_kind = match dev.detected_kind {
                            DeviceKind::Headphones => "Авто (Наушники)",
                            DeviceKind::Speakers => "Авто (Колонки)",
                        };
                        for (label_str, ovr) in [
                            (auto_kind, IconOverride::Auto),
                            ("Наушники", IconOverride::Headphones),
                            ("Колонки", IconOverride::Speakers),
                        ] {
                            let cmd_id = next_id;
                            next_id += 1;
                            let wide = to_wide_null(label_str);
                            let checked = if current_ovr == ovr {
                                MF_CHECKED
                            } else {
                                MF_UNCHECKED
                            };
                            let _ = AppendMenuW(
                                dev_menu,
                                MF_STRING | checked,
                                cmd_id as usize,
                                PCWSTR(wide.as_ptr()),
                            );
                            actions.push((cmd_id, MenuAction::SetIconOverride(dev.id.clone(), ovr)));
                        }
                        let dev_title = to_wide_null(&dev.name);
                        let _ = AppendMenuW(
                            icons_menu,
                            MF_POPUP,
                            dev_menu.0 as usize,
                            PCWSTR(dev_title.as_ptr()),
                        );
                    }
                }
                if list.is_empty() {
                    let empty = to_wide_null("Нет устройств");
                    let _ = AppendMenuW(
                        icons_menu,
                        MF_STRING | MF_GRAYED,
                        0,
                        PCWSTR(empty.as_ptr()),
                    );
                }
                let title = to_wide_null("Тип значка устройства");
                let _ = AppendMenuW(
                    menu,
                    MF_POPUP,
                    icons_menu.0 as usize,
                    PCWSTR(title.as_ptr()),
                );
            }

            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

            // Safe link to Windows 10/11 system tray icons toggle page.
            let sys_tray_label = to_wide_null("Системный значок громкости Windows...");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_OPEN_WIN_TRAY_SETTINGS as usize,
                PCWSTR(sys_tray_label.as_ptr()),
            );
            actions.push((CMD_OPEN_WIN_TRAY_SETTINGS, MenuAction::OpenSystemVolumeSettings));

            // Autostart with Windows.
            let autostart_label = to_wide_null("Запускать с Windows");
            let autostart_flags = MF_STRING
                | if is_autostart_enabled() {
                    MF_CHECKED
                } else {
                    MF_UNCHECKED
                };
            let _ = AppendMenuW(
                menu,
                autostart_flags,
                CMD_TOGGLE_AUTOSTART as usize,
                PCWSTR(autostart_label.as_ptr()),
            );
            actions.push((CMD_TOGGLE_AUTOSTART, MenuAction::ToggleAutostart));

            let exit_label = to_wide_null("Выход");
            let _ = AppendMenuW(
                menu,
                MF_STRING,
                CMD_EXIT as usize,
                PCWSTR(exit_label.as_ptr()),
            );
            actions.push((CMD_EXIT, MenuAction::Exit));
            Some((menu, actions))
        }
    }

    fn execute_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::SelectDevice(id) => {
                if set_default_audio_endpoint(&id).is_ok() {
                    self.refresh_audio_and_tray(false);
                }
            }
            MenuAction::OpenDevicesDialog => {
                self.snapshot = self.audio.snapshot();
                if let Some(dlg) = &self.devices_dialog {
                    dlg.show(&self.snapshot.devices, &self.config);
                }
            }
            MenuAction::SetIconOverride(id, ovr) => {
                self.config.set_override(&id, ovr);
                self.save_config_to_disk();
                self.sync_child_windows();
                self.update_tray_icon(true);
            }
            MenuAction::OpenSystemVolumeSettings => {
                open_windows_system_icons_settings();
            }
            MenuAction::ToggleAutostart => {
                let target = !is_autostart_enabled();
                let _ = set_autostart_enabled(target);
            }
            MenuAction::Exit => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
        }
    }

    fn cleanup_resources(&mut self) {
        HOOK_TRAY_HWND.store(0, Ordering::Release);
        if let Some(hhook) = self.mouse_hook.take() {
            unsafe {
                let _ = UnhookWindowsHookEx(hhook);
            }
        }
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

fn open_windows_system_icons_settings() {
    let verb = to_wide_null("open");
    let file = to_wide_null("explorer.exe");
    let params = to_wide_null("shell:::{05d7b0f4-2121-4eff-bf6b-ed3f69b894d9}");
    unsafe {
        let res = ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if (res.0 as isize) <= 32 {
            let fallback = to_wide_null("ms-settings:taskbar");
            let _ = ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(fallback.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
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
        let mut buf = [0u16; 64];
        let len = GetClassNameW(target, &mut buf);
        if len <= 0 {
            return false;
        }
        String::from_utf16_lossy(&buf[..len as usize]) == "ToolbarWindow32"
    }
}

unsafe extern "system" fn mouse_ll_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode >= 0 && wparam.0 as u32 == WM_MOUSEWHEEL && lparam.0 != 0 {
        let tray_raw = HOOK_TRAY_HWND.load(Ordering::Relaxed);
        if tray_raw != 0 {
            let ms = &*(lparam.0 as *const MSLLHOOKSTRUCT);
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
                    && is_cursor_over_tray_toolbar(ms.pt)
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
    CallNextHookEx(None, ncode, wparam, lparam)
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

fn write_tooltip_utf16(dest: &mut [u16; 128], text: &str) {
    dest.fill(0);
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

fn query_tray_icon_size(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = {
            let win_dpi = GetDpiForWindow(hwnd);
            if win_dpi > 0 {
                win_dpi
            } else {
                GetDpiForSystem()
            }
        };
        let metric = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        metric.max(16)
    }
}

fn is_light_taskbar_theme() -> bool {
    let subkey = to_wide_null(PERSONALIZE_SUBKEY);
    let value_name = to_wide_null(LIGHT_TASKBAR_VALUE);
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

fn enable_adaptive_dark_mode(hwnd: Option<HWND>) {
    let lib_name = to_wide_null("uxtheme.dll");
    unsafe {
        let Ok(hmod) = LoadLibraryExW(
            PCWSTR(lib_name.as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        ) else {
            return;
        };

        if let Some(f) = GetProcAddress(hmod, PCSTR(135usize as *const u8)) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 =
                std::mem::transmute(f);
            let _ = set_preferred_app_mode(1); // PreferredAppMode::AllowDark
        }

        if let Some(target_hwnd) = hwnd {
            if let Some(f) = GetProcAddress(hmod, PCSTR(133usize as *const u8)) {
                let allow_dark_for_window: unsafe extern "system" fn(HWND, BOOL) -> BOOL =
                    std::mem::transmute(f);
                let _ = allow_dark_for_window(target_hwnd, true.into());
            }
        }

        if let Some(f) = GetProcAddress(hmod, PCSTR(136usize as *const u8)) {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }

        let _ = FreeLibrary(hmod);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWLP_USERDATA};

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
                if wparam.0 == TIMER_LMB_SINGLE_CLICK {
                    let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.show_volume_flyout();
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
                    WM_LBUTTONUP => {
                        if let Ok(state) = state_rc.try_borrow() {
                            if state.is_flyout_visible() {
                                let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                                state.hide_volume_flyout();
                            } else {
                                let _ = SetTimer(
                                    Some(hwnd),
                                    TIMER_LMB_SINGLE_CLICK,
                                    LMB_FLYOUT_DELAY_MS,
                                    None,
                                );
                            }
                        }
                    }
                    WM_LBUTTONDBLCLK => {
                        let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            state.hide_volume_flyout();
                            state.cycle_next_device();
                        }
                    }
                    WM_RBUTTONUP => {
                        let _ = KillTimer(Some(hwnd), TIMER_LMB_SINGLE_CLICK);
                        if let Ok(state) = state_rc.try_borrow() {
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

        use windows::Win32::UI::WindowsAndMessaging::{SetWindowLongPtrW, GWLP_USERDATA};
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
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
