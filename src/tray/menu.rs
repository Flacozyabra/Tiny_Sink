//! Right-click tray context menu construction and command dispatch.

use super::{open_shell_target, TrayState};
use crate::audio::AudioDeviceInfo;
use crate::config::{
    is_autostart_enabled, set_autostart_enabled, to_wide_null, DeviceKind, IconOverride,
};
use crate::policy_config::set_default_audio_endpoint;
use crate::updater::{APP_VERSION, RELEASES_PAGE_URL};
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyWindow, HMENU, MF_CHECKED, MF_GRAYED, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
};

const CMD_OPEN_DEVICES: u32 = 4001;
const CMD_OPEN_WIN_TRAY_SETTINGS: u32 = 4002;
const CMD_TOGGLE_AUTOSTART: u32 = 4003;
const CMD_EXIT: u32 = 4004;
const CMD_UPDATE_INSTALL: u32 = 4010;
const CMD_UPDATE_CHECK_NOW: u32 = 4011;
const CMD_UPDATE_TOGGLE_AUTO: u32 = 4012;
const CMD_UPDATE_OPEN_RELEASES: u32 = 4013;

pub(super) enum MenuAction {
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

impl TrayState {
    pub(super) fn build_context_menu(&mut self) -> Option<(HMENU, Vec<(u32, MenuAction)>)> {
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
                    let check_wide = to_wide_null(&format!("Проверить сейчас (v{APP_VERSION})"));
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

    pub(super) fn execute_menu_action(&mut self, action: MenuAction) {
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
}

