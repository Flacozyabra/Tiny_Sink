//! Taskbar theme detection, DPI tray icon metrics, and adaptive dark popup menu support.

use crate::config::to_wide_null;
use windows::core::{BOOL, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, HWND};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON;

const PERSONALIZE_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const LIGHT_TASKBAR_VALUE: &str = "SystemUsesLightTheme";

pub(super) fn query_tray_icon_size(hwnd: HWND) -> i32 {
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

pub(super) fn is_light_taskbar_theme() -> bool {
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

pub(super) fn enable_adaptive_dark_mode(hwnd: Option<HWND>) {
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
