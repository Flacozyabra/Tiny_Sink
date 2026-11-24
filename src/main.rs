//! Entry point for Volume Smart Icon: COM initialization, single-instance guard, and tray loop.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod audio;
mod config;
mod icon;
mod policy_config;
mod tray;

use config::to_wide_null;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::CreateMutexW;

const SINGLE_INSTANCE_MUTEX: &str = "Local\\VolumeSmartIconSingleInstanceMutex";

struct ComGuard;

impl ComGuard {
    fn init() -> windows::core::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct MutexGuard(HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn acquire_single_instance() -> Option<MutexGuard> {
    let name = to_wide_null(SINGLE_INSTANCE_MUTEX);
    unsafe {
        let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr())).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(MutexGuard(handle))
    }
}

fn main() {
    let Some(_instance_guard) = acquire_single_instance() else {
        return;
    };
    let Ok(_com_guard) = ComGuard::init() else {
        return;
    };
    let _ = tray::run_app();
}

