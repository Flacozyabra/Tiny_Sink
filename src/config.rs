//! Persistent configuration, device classification overrides, cycle selection, and autostart management.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const APP_DIR_NAME: &str = "Tiny_Sink";
const CONFIG_FILE_NAME: &str = "config.json";
const RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE_NAME: &str = "Tiny_Sink";

/// Visual category of an audio output endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Headphones,
    Speakers,
}

/// User-selectable icon override for a specific endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IconOverride {
    #[default]
    Auto,
    Headphones,
    Speakers,
}

/// Minimal endpoint descriptor used for configuration and device cycling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointSummary {
    pub id: String,
    pub is_display_output: bool,
}

/// Persisted user settings stored under `%APPDATA%\Tiny_Sink\config.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// True once initial default device selection has been established.
    pub cycle_initialized: bool,
    /// Ordered or selected endpoint IDs included in double-click cycling.
    pub cycle_device_ids: Vec<String>,
    /// Per-endpoint manual icon overrides.
    pub icon_overrides: HashMap<String, IconOverride>,
    /// Whether automatic background update checks run on startup and every 24 hours.
    pub auto_check_updates: bool,
    /// Unix timestamp (seconds) of the last completed update check attempt.
    pub last_update_check_unix: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            cycle_initialized: false,
            cycle_device_ids: Vec::new(),
            icon_overrides: HashMap::new(),
            auto_check_updates: true,
            last_update_check_unix: 0,
        }
    }
}

impl AppConfig {
    /// Resolves the effective icon kind for an endpoint, applying manual override if set.
    pub fn effective_kind(&self, device_id: &str, detected: DeviceKind) -> DeviceKind {
        match self.icon_overrides.get(device_id).copied().unwrap_or_default() {
            IconOverride::Auto => detected,
            IconOverride::Headphones => DeviceKind::Headphones,
            IconOverride::Speakers => DeviceKind::Speakers,
        }
    }

    /// Returns the configured icon override for an endpoint.
    pub fn override_for(&self, device_id: &str) -> IconOverride {
        self.icon_overrides.get(device_id).copied().unwrap_or_default()
    }

    /// Sets or clears a manual icon override for an endpoint.
    pub fn set_override(&mut self, device_id: &str, value: IconOverride) {
        if value == IconOverride::Auto {
            self.icon_overrides.remove(device_id);
        } else {
            self.icon_overrides.insert(device_id.to_string(), value);
        }
    }

    /// Initializes the SoundSwitch-style cycle list on first run if not yet initialized.
    ///
    /// When more than two active outputs exist and at least two non-display outputs are present
    /// (e.g., speakers + headphones alongside an unused monitor HDMI output), display outputs are
    /// excluded by default so the initial cycle focuses on real audio peripherals.
    pub fn ensure_cycle_initialized(&mut self, active_endpoints: &[EndpointSummary]) -> bool {
        if self.cycle_initialized {
            return false;
        }
        let non_display: Vec<String> = active_endpoints
            .iter()
            .filter(|e| !e.is_display_output)
            .map(|e| e.id.clone())
            .collect();

        self.cycle_device_ids = if active_endpoints.len() > 2 && non_display.len() >= 2 {
            non_display
        } else {
            active_endpoints.iter().map(|e| e.id.clone()).collect()
        };
        self.cycle_initialized = true;
        true
    }

    /// Returns whether an endpoint ID is currently included in double-click cycling.
    pub fn is_in_cycle(&self, device_id: &str) -> bool {
        self.cycle_device_ids.iter().any(|id| id == device_id)
    }

    /// Toggles membership of an endpoint ID in the double-click cycle list.
    pub fn toggle_cycle_device(&mut self, device_id: &str) -> bool {
        self.cycle_initialized = true;
        if let Some(pos) = self.cycle_device_ids.iter().position(|id| id == device_id) {
            self.cycle_device_ids.remove(pos);
            false
        } else {
            self.cycle_device_ids.push(device_id.to_string());
            true
        }
    }

    /// Computes the next active endpoint ID to switch to on double-click.
    ///
    /// Preserves the active endpoint enumeration order among checked devices.
    pub fn next_cycle_target(
        &self,
        active_ids: &[String],
        current_default_id: Option<&str>,
    ) -> Option<String> {
        let candidates: Vec<&String> = active_ids
            .iter()
            .filter(|id| self.is_in_cycle(id))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        if let Some(current) = current_default_id {
            if let Some(idx) = candidates.iter().position(|id| id.as_str() == current) {
                if candidates.len() == 1 {
                    return None;
                }
                let next_idx = (idx + 1) % candidates.len();
                return Some(candidates[next_idx].clone());
            }
        }

        Some(candidates[0].clone())
    }
}

/// Returns the default configuration file path inside `%APPDATA%\VolumeSmartIcon`.
pub fn default_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| {
        PathBuf::from(dir)
            .join(APP_DIR_NAME)
            .join(CONFIG_FILE_NAME)
    })
}

/// Loads configuration from disk, returning defaults if missing or unreadable.
pub fn load_config(path: &Path) -> AppConfig {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => AppConfig::default(),
    }
}

/// Saves configuration to disk, creating parent directories if needed.
pub fn save_config(path: &Path, config: &AppConfig) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    let tmp_path = path.with_extension("json.tmp");
    fs::write(&tmp_path, json)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Encodes a Rust string slice as a null-terminated UTF-16 buffer for Win32 calls.
pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Checks whether the application is configured to start automatically with Windows.
pub fn is_autostart_enabled() -> bool {
    let subkey = to_wide_null(RUN_SUBKEY);
    let value_name = to_wide_null(RUN_VALUE_NAME);
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
        let exists = RegQueryValueExW(
            hkey,
            PCWSTR(value_name.as_ptr()),
            None,
            None,
            None,
            None,
        )
        .is_ok();
        let _ = RegCloseKey(hkey);
        exists
    }
}

/// Enables or disables automatic startup in `HKCU\...\Run` for the current executable.
pub fn set_autostart_enabled(enabled: bool) -> windows::core::Result<()> {
    let subkey = to_wide_null(RUN_SUBKEY);
    let value_name = to_wide_null(RUN_VALUE_NAME);
    let mut hkey = HKEY::default();

    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .ok()?;

        let result = if enabled {
            let exe_path = current_exe_quoted()?;
            let wide = to_wide_null(&exe_path);
            let bytes = std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * std::mem::size_of::<u16>(),
            );
            RegSetValueExW(hkey, PCWSTR(value_name.as_ptr()), Some(0), REG_SZ, Some(bytes)).ok()
        } else {
            let err = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
            if err.is_ok() || err == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                err.ok()
            }
        };

        let _ = RegCloseKey(hkey);
        result
    }
}

fn current_exe_quoted() -> windows::core::Result<String> {
    let mut buf = vec![0u16; 1024];
    let len = unsafe { GetModuleFileNameW(None, &mut buf) };
    if len == 0 {
        return Err(windows::core::Error::from_thread());
    }
    let path = unsafe { PWSTR(buf.as_mut_ptr()).to_string()? };
    Ok(format!("\"{path}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_prefers_non_display_when_multiple_exist() {
        let mut cfg = AppConfig::default();
        let endpoints = vec![
            EndpointSummary {
                id: "hdmi".into(),
                is_display_output: true,
            },
            EndpointSummary {
                id: "speakers".into(),
                is_display_output: false,
            },
            EndpointSummary {
                id: "headphones".into(),
                is_display_output: false,
            },
        ];

        assert!(cfg.ensure_cycle_initialized(&endpoints));
        assert_eq!(cfg.cycle_device_ids, vec!["speakers", "headphones"]);
        assert!(!cfg.ensure_cycle_initialized(&endpoints));
    }

    #[test]
    fn first_run_includes_all_when_only_two_devices_exist() {
        let mut cfg = AppConfig::default();
        let endpoints = vec![
            EndpointSummary {
                id: "hdmi".into(),
                is_display_output: true,
            },
            EndpointSummary {
                id: "headphones".into(),
                is_display_output: false,
            },
        ];

        assert!(cfg.ensure_cycle_initialized(&endpoints));
        assert_eq!(cfg.cycle_device_ids, vec!["hdmi", "headphones"]);
    }

    #[test]
    fn cycle_wraps_and_handles_unselected_current_device() {
        let mut cfg = AppConfig::default();
        cfg.cycle_initialized = true;
        cfg.cycle_device_ids = vec!["a".into(), "c".into()];

        let active = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(
            cfg.next_cycle_target(&active, Some("a")),
            Some("c".into())
        );
        assert_eq!(
            cfg.next_cycle_target(&active, Some("c")),
            Some("a".into())
        );
        assert_eq!(
            cfg.next_cycle_target(&active, Some("b")),
            Some("a".into())
        );
    }

    #[test]
    fn cycle_returns_none_when_fewer_than_two_active_checked_or_same_active() {
        let mut cfg = AppConfig::default();
        cfg.cycle_initialized = true;
        cfg.cycle_device_ids = vec!["a".into()];

        let active = vec!["a".into(), "b".into()];
        assert_eq!(cfg.next_cycle_target(&active, Some("a")), None);
        assert_eq!(
            cfg.next_cycle_target(&active, Some("b")),
            Some("a".into())
        );
    }

    #[test]
    fn override_falls_back_to_detected_when_auto() {
        let mut cfg = AppConfig::default();
        assert_eq!(
            cfg.effective_kind("dev1", DeviceKind::Headphones),
            DeviceKind::Headphones
        );
        cfg.set_override("dev1", IconOverride::Speakers);
        assert_eq!(
            cfg.effective_kind("dev1", DeviceKind::Headphones),
            DeviceKind::Speakers
        );
        cfg.set_override("dev1", IconOverride::Auto);
        assert_eq!(
            cfg.effective_kind("dev1", DeviceKind::Headphones),
            DeviceKind::Headphones
        );
    }

    #[test]
    fn config_round_trips_json() {
        let dir = std::env::temp_dir().join("tiny_sink_test_cfg");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.json");

        let mut cfg = AppConfig::default();
        assert!(cfg.auto_check_updates);
        cfg.cycle_initialized = true;
        cfg.cycle_device_ids = vec!["{0.0.0.0}.{123}".into()];
        cfg.set_override("{0.0.0.0}.{123}", IconOverride::Headphones);
        cfg.auto_check_updates = false;
        cfg.last_update_check_unix = 1_700_000_000;

        save_config(&path, &cfg).expect("save config");
        let loaded = load_config(&path);
        assert!(loaded.cycle_initialized);
        assert_eq!(loaded.cycle_device_ids, cfg.cycle_device_ids);
        assert_eq!(
            loaded.override_for("{0.0.0.0}.{123}"),
            IconOverride::Headphones
        );
        assert!(!loaded.auto_check_updates);
        assert_eq!(loaded.last_update_check_unix, 1_700_000_000);

        let partial: AppConfig = serde_json::from_str("{}").expect("partial config");
        assert!(partial.auto_check_updates);
        assert_eq!(partial.last_update_check_unix, 0);

        let _ = fs::remove_dir_all(&dir);
    }
}

