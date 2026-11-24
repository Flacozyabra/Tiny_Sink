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
    DevicesDialog, UpdateDialog, UpdateDialogView, VolumeFlyout, WM_APP_DEVICES_TOGGLE_ID,
    WM_APP_FLYOUT_SET_VOLUME, WM_APP_FLYOUT_TOGGLE_MUTE, WM_APP_UPDATE_CANCEL_DOWNLOAD,
    WM_APP_UPDATE_RETRY_CHECK, WM_APP_UPDATE_START_DOWNLOAD, WM_APP_WHEEL_VOLUME,
};
use crate::updater::{
    apply_staged_update_and_launch, now_unix_secs, spawn_check_thread, spawn_download_thread,
    DownloadHandle, DownloadOutcome, DownloadProgress, ReleaseInfo, UpdateCheckOutcome, APP_VERSION,
    RELEASES_PAGE_URL, WM_APP_UPDATE_CHECK_DONE, WM_APP_UPDATE_DOWNLOAD_DONE,
    WM_APP_UPDATE_PROGRESS,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
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
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIN_BALLOONUSERCLICK, NOTIFYICONDATAW,
    NOTIFYICONIDENTIFIER,
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
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEWHEEL, WM_NULL, WM_QUERYENDSESSION,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_TRAY: u32 = WM_APP + 10;
const TRAY_ICON_ID: u32 = 1;
const TIMER_LMB_SINGLE_CLICK: usize = 1;
const TIMER_AUTO_UPDATE_STARTUP: usize = 2;
const TIMER_AUTO_UPDATE_PERIODIC: usize = 3;
const AUTO_UPDATE_STARTUP_DELAY_MS: u32 = 15_000;
const AUTO_UPDATE_INTERVAL_MS: u32 = 86_400_000;

const WINDOW_CLASS_NAME: &str = "TinySinkTrayWindow";
const PERSONALIZE_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const LIGHT_TASKBAR_VALUE: &str = "SystemUsesLightTheme";

const CMD_OPEN_DEVICES: u32 = 4001;
const CMD_OPEN_WIN_TRAY_SETTINGS: u32 = 4002;
const CMD_TOGGLE_AUTOSTART: u32 = 4003;
const CMD_EXIT: u32 = 4004;
const CMD_UPDATE_INSTALL: u32 = 4010;
const CMD_UPDATE_CHECK_NOW: u32 = 4011;
const CMD_UPDATE_TOGGLE_AUTO: u32 = 4012;
const CMD_UPDATE_OPEN_RELEASES: u32 = 4013;

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
    InstallAvailableUpdate,
    CheckForUpdatesManual,
    ToggleAutoUpdates,
    OpenReleasesPage,
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
    update_dialog: Option<UpdateDialog>,
    available_update: Option<ReleaseInfo>,
    notified_tag: Option<String>,
    update_checking: bool,
    pending_manual_feedback: bool,
    download_handle: Option<DownloadHandle>,
    download_gen: usize,
    applying_update: bool,
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

        HOOK_TRAY_HWND.store(hwnd.0 as usize, Ordering::Release);
        let mouse_hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_ll_proc), None, 0).ok() };
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

    fn save_config_to_disk(&self) {
        if let Some(path) = self.config_path.as_deref() {
            let _ = save_config(path, &self.config);
        }
    }

    fn configure_auto_update_timers(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_AUTO_UPDATE_STARTUP);
            let _ = KillTimer(Some(self.hwnd), TIMER_AUTO_UPDATE_PERIODIC);
            if self.config.auto_check_updates {
                let _ = SetTimer(
                    Some(self.hwnd),
                    TIMER_AUTO_UPDATE_STARTUP,
                    AUTO_UPDATE_STARTUP_DELAY_MS,
                    None,
                );
                let _ = SetTimer(
                    Some(self.hwnd),
                    TIMER_AUTO_UPDATE_PERIODIC,
                    AUTO_UPDATE_INTERVAL_MS,
                    None,
                );
            }
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
        if let Some(upd) = &self.update_dialog {
            upd.refresh_theme();
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

            // Single top-level "Update" row with all update actions inside its dropdown submenu.
            if let Ok(update_menu) = CreatePopupMenu() {
                if let Some(info) = &self.available_update {
                    let install_text = if self.download_handle.is_some() {
                        format!("Открыть загрузку {}...", info.tag_name)
                    } else {
                        format!("Установить {}...", info.tag_name)
                    };
                    let wide = to_wide_null(&install_text);
                    let _ = AppendMenuW(
                        update_menu,
                        MF_STRING,
                        CMD_UPDATE_INSTALL as usize,
                        PCWSTR(wide.as_ptr()),
                    );
                    actions.push((CMD_UPDATE_INSTALL, MenuAction::InstallAvailableUpdate));
                    let _ = AppendMenuW(update_menu, MF_SEPARATOR, 0, PCWSTR::null());
                }

                if self.update_checking {
                    let checking_wide = to_wide_null("Идёт проверка обновлений...");
                    let _ = AppendMenuW(
                        update_menu,
                        MF_STRING | MF_GRAYED,
                        0,
                        PCWSTR(checking_wide.as_ptr()),
                    );
                } else {
                    let check_wide =
                        to_wide_null(&format!("Проверить сейчас (v{APP_VERSION})"));
                    let _ = AppendMenuW(
                        update_menu,
                        MF_STRING,
                        CMD_UPDATE_CHECK_NOW as usize,
                        PCWSTR(check_wide.as_ptr()),
                    );
                    actions.push((CMD_UPDATE_CHECK_NOW, MenuAction::CheckForUpdatesManual));
                }

                let auto_wide = to_wide_null("Проверять автоматически");
                let auto_flags = MF_STRING
                    | if self.config.auto_check_updates {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    };
                let _ = AppendMenuW(
                    update_menu,
                    auto_flags,
                    CMD_UPDATE_TOGGLE_AUTO as usize,
                    PCWSTR(auto_wide.as_ptr()),
                );
                actions.push((CMD_UPDATE_TOGGLE_AUTO, MenuAction::ToggleAutoUpdates));

                let _ = AppendMenuW(update_menu, MF_SEPARATOR, 0, PCWSTR::null());
                let releases_wide = to_wide_null("Открыть релизы на GitHub...");
                let _ = AppendMenuW(
                    update_menu,
                    MF_STRING,
                    CMD_UPDATE_OPEN_RELEASES as usize,
                    PCWSTR(releases_wide.as_ptr()),
                );
                actions.push((CMD_UPDATE_OPEN_RELEASES, MenuAction::OpenReleasesPage));

                let top_title = if self.available_update.is_some() {
                    "Update • доступно обновление"
                } else {
                    "Update"
                };
                let top_wide = to_wide_null(top_title);
                let _ = AppendMenuW(
                    menu,
                    MF_POPUP,
                    update_menu.0 as usize,
                    PCWSTR(top_wide.as_ptr()),
                );
            }

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
                open_shell_target("ms-settings:taskbar");
            }
            MenuAction::ToggleAutostart => {
                let target = !is_autostart_enabled();
                let _ = set_autostart_enabled(target);
            }
            MenuAction::InstallAvailableUpdate => {
                self.open_available_update_dialog();
            }
            MenuAction::CheckForUpdatesManual => {
                self.start_update_check(true);
            }
            MenuAction::ToggleAutoUpdates => {
                self.config.auto_check_updates = !self.config.auto_check_updates;
                self.save_config_to_disk();
                self.configure_auto_update_timers();
            }
            MenuAction::OpenReleasesPage => {
                let url = self
                    .available_update
                    .as_ref()
                    .map(|u| u.html_url.as_str())
                    .unwrap_or(RELEASES_PAGE_URL);
                open_shell_target(url);
            }
            MenuAction::Exit => unsafe {
                let _ = DestroyWindow(self.hwnd);
            },
        }
    }

    fn start_update_check(&mut self, manual: bool) {
        if self.applying_update {
            return;
        }
        if self.download_handle.is_some() {
            if manual {
                self.open_available_update_dialog();
            }
            return;
        }
        if manual {
            if let Some(upd) = &self.update_dialog {
                upd.set_view(
                    UpdateDialogView::Checking {
                        current_version: APP_VERSION.to_string(),
                    },
                    true,
                );
            }
        }
        if self.update_checking {
            if manual {
                self.pending_manual_feedback = true;
            }
            return;
        }
        self.update_checking = true;
        self.pending_manual_feedback = manual;
        spawn_check_thread(self.hwnd, manual);
    }

    fn open_available_update_dialog(&self) {
        let Some(upd) = &self.update_dialog else {
            return;
        };
        if self.download_handle.is_some() {
            let ver = self
                .available_update
                .as_ref()
                .map(|i| i.version.clone())
                .unwrap_or_else(|| APP_VERSION.to_string());
            upd.set_view(
                UpdateDialogView::Downloading {
                    version: ver,
                    percent: 0,
                    downloaded_bytes: 0,
                    total_bytes: 0,
                    speed_bps: 0.0,
                },
                true,
            );
            return;
        }
        if let Some(info) = self.available_update.clone() {
            upd.set_view(
                UpdateDialogView::Available {
                    current_version: APP_VERSION.to_string(),
                    info,
                },
                true,
            );
        }
    }

    fn handle_update_check_done(&mut self, manual: bool, outcome: UpdateCheckOutcome) {
        self.update_checking = false;
        let show_ui = manual
            || std::mem::take(&mut self.pending_manual_feedback)
            || self.update_dialog.as_ref().is_some_and(UpdateDialog::is_visible);

        if matches!(
            outcome,
            UpdateCheckOutcome::UpToDate { .. } | UpdateCheckOutcome::Available(_)
        ) {
            self.config.last_update_check_unix = now_unix_secs();
            self.save_config_to_disk();
        }

        match outcome {
            UpdateCheckOutcome::Available(info) => {
                let first_notice_for_tag = self.notified_tag.as_deref() != Some(info.tag_name.as_str());
                self.available_update = Some(info.clone());
                if show_ui {
                    if let Some(upd) = &self.update_dialog {
                        upd.set_view(
                            UpdateDialogView::Available {
                                current_version: APP_VERSION.to_string(),
                                info,
                            },
                            true,
                        );
                    }
                } else if first_notice_for_tag {
                    self.notified_tag = Some(info.tag_name.clone());
                    self.show_update_balloon(&info);
                }
            }
            UpdateCheckOutcome::UpToDate { current } => {
                self.available_update = None;
                self.notified_tag = None;
                if show_ui {
                    if let Some(upd) = &self.update_dialog {
                        upd.set_view(
                            UpdateDialogView::UpToDate {
                                current_version: current,
                            },
                            true,
                        );
                    }
                }
            }
            UpdateCheckOutcome::Error(message) => {
                if show_ui {
                    if let Some(upd) = &self.update_dialog {
                        upd.set_view(UpdateDialogView::Error { message }, true);
                    }
                }
            }
        }
    }

    fn start_update_download(&mut self) {
        if self.download_handle.is_some() || self.applying_update {
            return;
        }
        let Some(info) = self.available_update.clone() else {
            return;
        };
        let version = info.version.clone();
        let next_gen = self.download_gen.wrapping_add(1).max(1);
        match spawn_download_thread(self.hwnd, next_gen, info) {
            Ok(handle) => {
                self.download_gen = next_gen;
                self.download_handle = Some(handle);
                if let Some(upd) = &self.update_dialog {
                    upd.set_view(
                        UpdateDialogView::Downloading {
                            version,
                            percent: 0,
                            downloaded_bytes: 0,
                            total_bytes: 0,
                            speed_bps: 0.0,
                        },
                        true,
                    );
                }
            }
            Err(message) => {
                if let Some(upd) = &self.update_dialog {
                    upd.set_view(UpdateDialogView::Error { message }, true);
                }
            }
        }
    }

    fn handle_download_progress(&self, gen_id: usize, prog: DownloadProgress) {
        if gen_id != self.download_gen || self.download_handle.is_none() || self.applying_update {
            return;
        }
        let percent = if prog.total_bytes > 0 {
            ((prog.downloaded_bytes * 100) / prog.total_bytes).min(100) as u8
        } else {
            0
        };
        let version = self
            .available_update
            .as_ref()
            .map(|i| i.version.clone())
            .unwrap_or_else(|| APP_VERSION.to_string());
        if let Some(upd) = &self.update_dialog {
            upd.set_view(
                UpdateDialogView::Downloading {
                    version,
                    percent,
                    downloaded_bytes: prog.downloaded_bytes,
                    total_bytes: prog.total_bytes,
                    speed_bps: prog.speed_bps,
                },
                false,
            );
        }
    }

    fn cancel_update_download(&mut self) {
        self.download_gen = self.download_gen.wrapping_add(1);
        if let Some(handle) = self.download_handle.take() {
            handle.cancel();
        }
    }

    fn handle_download_done(&mut self, gen_id: usize, outcome: DownloadOutcome) {
        if gen_id != self.download_gen {
            return;
        }
        self.download_handle = None;
        match outcome {
            DownloadOutcome::Ready(staged_path) => {
                self.applying_update = true;
                let version = self
                    .available_update
                    .as_ref()
                    .map(|i| i.version.clone())
                    .unwrap_or_else(|| APP_VERSION.to_string());
                if let Some(upd) = &self.update_dialog {
                    upd.set_view(UpdateDialogView::Applying { version }, true);
                }
                match apply_staged_update_and_launch(&staged_path) {
                    Ok(()) => {
                        self.cleanup_resources();
                        unsafe {
                            let _ = DestroyWindow(self.hwnd);
                        }
                    }
                    Err(message) => {
                        self.applying_update = false;
                        if let Some(upd) = &self.update_dialog {
                            upd.set_view(UpdateDialogView::Error { message }, true);
                        }
                    }
                }
            }
            DownloadOutcome::Cancelled => {}
            DownloadOutcome::Error(message) => {
                if let Some(upd) = &self.update_dialog {
                    upd.set_view(UpdateDialogView::Error { message }, true);
                }
            }
        }
    }

    fn show_update_balloon(&self, info: &ReleaseInfo) {
        if !self.icon_added {
            return;
        }
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_INFO,
            dwInfoFlags: NIIF_INFO,
            ..Default::default()
        };
        write_utf16_slice(&mut nid.szInfoTitle, "Tiny_Sink — доступно обновление");
        let body = format!(
            "Вышла версия {}. Нажмите на уведомление, чтобы установить.",
            info.tag_name
        );
        write_utf16_slice(&mut nid.szInfo, &body);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn cleanup_resources(&mut self) {
        if let Some(handle) = self.download_handle.take() {
            handle.cancel();
        }
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

fn open_shell_target(target_uri: &str) {
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

const TOOLBAR_CLASS_UTF16: [u16; 15] = [
    b'T' as u16, b'o' as u16, b'o' as u16, b'l' as u16, b'b' as u16, b'a' as u16, b'r' as u16,
    b'W' as u16, b'i' as u16, b'n' as u16, b'd' as u16, b'o' as u16, b'w' as u16, b'3' as u16,
    b'2' as u16,
];

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

fn write_utf16_slice(dest: &mut [u16], text: &str) {
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
