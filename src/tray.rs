//! Win32 hidden message window, system tray icon lifecycle, double-click switching, and context menu.

use crate::audio::{
    AudioDeviceInfo, AudioMonitor, AudioSnapshot, WM_APP_DEVICES_CHANGED, WM_APP_VOLUME_CHANGED,
};
use crate::config::{
    default_config_path, is_autostart_enabled, load_config, save_config, set_autostart_enabled,
    to_wide_null, AppConfig, DeviceKind, EndpointSummary, IconOverride,
};
use crate::icon::create_tray_hicon;
use crate::policy_config::set_default_audio_endpoint;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use windows::core::{BOOL, PCSTR, PCWSTR, Result};
use windows::Win32::Foundation::{FreeLibrary, HWND, LPARAM, LRESULT, POINT, WPARAM};
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
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow,
    TrackPopupMenuEx, TranslateMessage, CW_USEDEFAULT, HICON, HMENU, IDC_ARROW, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MSG, SM_CXSMICON, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY, WM_DPICHANGED,
    WM_ENDSESSION, WM_LBUTTONDBLCLK, WM_NULL, WM_QUERYENDSESSION, WM_RBUTTONUP, WM_SETTINGCHANGE,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_APP_TRAY: u32 = WM_APP + 10;
const TRAY_ICON_ID: u32 = 1;
const WINDOW_CLASS_NAME: &str = "TinySinkTrayWindow";
const PERSONALIZE_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const LIGHT_TASKBAR_VALUE: &str = "SystemUsesLightTheme";

const CMD_TOGGLE_MUTE: u32 = 4001;
const CMD_TOGGLE_AUTOSTART: u32 = 4002;
const CMD_EXIT: u32 = 4003;

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
    ToggleCycleDevice(String),
    SetIconOverride(String, IconOverride),
    ToggleMute,
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
        };
        state.update_tray_icon(true);
        Ok(state)
    }

    fn save_config_to_disk(&self) {
        if let Some(path) = self.config_path.as_deref() {
            let _ = save_config(path, &self.config);
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
        self.update_tray_icon(force_icon);
    }

    fn refresh_volume_only(&mut self) {
        let (vol, muted) = self.audio.read_volume_and_mute();
        self.snapshot.volume_percent = vol;
        self.snapshot.muted = muted;
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

        let need_new_icon = force || !self.icon_added || self.last_render_key.as_ref() != Some(&key);
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

        unsafe {
            let menu = CreatePopupMenu().ok()?;

            if self.snapshot.devices.is_empty() {
                let empty = to_wide_null("Нет активных устройств вывода");
                let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(empty.as_ptr()));
            } else {
                for dev in &self.snapshot.devices {
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

            // Mute toggle for current device.
            let mute_label = to_wide_null("Без звука (Mute)");
            let mute_flags =
                MF_STRING | if self.snapshot.muted { MF_CHECKED } else { MF_UNCHECKED };
            let _ = AppendMenuW(
                menu,
                mute_flags,
                CMD_TOGGLE_MUTE as usize,
                PCWSTR(mute_label.as_ptr()),
            );
            actions.push((CMD_TOGGLE_MUTE, MenuAction::ToggleMute));

            // Submenu: SoundSwitch-style cycle list.
            if let Ok(cycle_menu) = CreatePopupMenu() {
                for dev in &self.snapshot.devices {
                    let cmd_id = next_id;
                    next_id += 1;
                    let wide = to_wide_null(&dev.name);
                    let checked = if self.config.is_in_cycle(&dev.id) {
                        MF_CHECKED
                    } else {
                        MF_UNCHECKED
                    };
                    let _ = AppendMenuW(
                        cycle_menu,
                        MF_STRING | checked,
                        cmd_id as usize,
                        PCWSTR(wide.as_ptr()),
                    );
                    actions.push((cmd_id, MenuAction::ToggleCycleDevice(dev.id.clone())));
                }
                if self.snapshot.devices.is_empty() {
                    let empty = to_wide_null("Нет устройств");
                    let _ = AppendMenuW(cycle_menu, MF_STRING | MF_GRAYED, 0, PCWSTR(empty.as_ptr()));
                }
                let title = to_wide_null("Цикл переключения (2x клик)");
                let _ = AppendMenuW(
                    menu,
                    MF_POPUP,
                    cycle_menu.0 as usize,
                    PCWSTR(title.as_ptr()),
                );
            }

            // Submenu: manual icon override per device.
            if let Ok(icons_menu) = CreatePopupMenu() {
                for dev in &self.snapshot.devices {
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
                if self.snapshot.devices.is_empty() {
                    let empty = to_wide_null("Нет устройств");
                    let _ = AppendMenuW(icons_menu, MF_STRING | MF_GRAYED, 0, PCWSTR(empty.as_ptr()));
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
            MenuAction::ToggleCycleDevice(id) => {
                self.config.toggle_cycle_device(&id);
                self.save_config_to_disk();
            }
            MenuAction::SetIconOverride(id, ovr) => {
                self.config.set_override(&id, ovr);
                self.save_config_to_disk();
                self.update_tray_icon(true);
            }
            MenuAction::ToggleMute => {
                self.audio.toggle_mute();
                self.refresh_volume_only();
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
        self.remove_tray_icon();
    }
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
    // Avoid dangling high surrogate if truncated mid-pair.
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
    // Undocumented uxtheme.dll ordinals (133/135/136) used since Windows 10 1903
    // to let native Win32 popup menus follow Windows Dark/Light app theme.
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
        // Clone an owned Rc guard so TrayState remains valid even if WM_DESTROY fires inside a modal menu loop.
        Rc::increment_strong_count(ptr);
        let state_rc = Rc::from_raw(ptr);

        let taskbar_match = {
            if let Ok(state) = state_rc.try_borrow() {
                msg == state.taskbar_created_msg && state.taskbar_created_msg != 0
            } else {
                false
            }
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
            WM_SETTINGCHANGE | WM_DPICHANGED => {
                if msg == WM_SETTINGCHANGE {
                    enable_adaptive_dark_mode(Some(hwnd));
                }
                if let Ok(mut state) = state_rc.try_borrow_mut() {
                    state.update_tray_icon(false);
                }
                return LRESULT(0);
            }
            WM_APP_TRAY => {
                let mouse_event = (lparam.0 as u32) & 0xFFFF;
                match mouse_event {
                    WM_LBUTTONDBLCLK => {
                        if let Ok(mut state) = state_rc.try_borrow_mut() {
                            state.cycle_next_device();
                        }
                    }
                    WM_RBUTTONUP => {
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
            WM_QUERYENDSESSION => {
                return LRESULT(1);
            }
            WM_ENDSESSION => {
                if wparam.0 != 0 {
                    if let Ok(mut state) = state_rc.try_borrow_mut() {
                        state.remove_tray_icon();
                    }
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                // Release the ownership reference stored in GWLP_USERDATA.
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

