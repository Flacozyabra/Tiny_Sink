//! Auto-check timers, tray balloon notifications, download progress, and restart handoff.

use super::{
    write_utf16_slice, TrayState, AUTO_UPDATE_INTERVAL_MS, AUTO_UPDATE_STARTUP_DELAY_MS,
    TIMER_AUTO_UPDATE_PERIODIC, TIMER_AUTO_UPDATE_STARTUP, TRAY_ICON_ID,
};
use crate::ui::{UpdateDialog, UpdateDialogView};
use crate::updater::{
    apply_staged_update_and_launch, now_unix_secs, spawn_check_thread, spawn_download_thread,
    DownloadOutcome, DownloadProgress, ReleaseInfo, UpdateCheckOutcome, APP_VERSION,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, KillTimer, SetTimer};

impl TrayState {
    pub(super) fn configure_auto_update_timers(&self) {
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

    pub(super) fn start_update_check(&mut self, manual: bool) {
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

    pub(super) fn open_available_update_dialog(&self) {
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

    pub(super) fn handle_update_check_done(&mut self, manual: bool, outcome: UpdateCheckOutcome) {
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
                let first_notice_for_tag =
                    self.notified_tag.as_deref() != Some(info.tag_name.as_str());
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

    pub(super) fn start_update_download(&mut self) {
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

    pub(super) fn handle_download_progress(&self, gen_id: usize, prog: DownloadProgress) {
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

    pub(super) fn cancel_update_download(&mut self) {
        self.download_gen = self.download_gen.wrapping_add(1);
        if let Some(handle) = self.download_handle.take() {
            handle.cancel();
        }
    }

    pub(super) fn handle_download_done(&mut self, gen_id: usize, outcome: DownloadOutcome) {
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
        write_utf16_slice(&mut nid.szInfoTitle, "Tiny_Sink — update available");
        let body = format!(
            "Version {} is ready. Click this notification to install.",
            info.tag_name
        );
        write_utf16_slice(&mut nid.szInfo, &body);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

