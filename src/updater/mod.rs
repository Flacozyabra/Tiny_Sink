//! Background and manual GitHub update checks, WinHTTP downloads, SHA-256 verification, and self-replacement.

mod github;
mod sha256;
mod winhttp;

use github::{is_newer_version, parse_github_release_json, verify_sha256_sums};
use sha256::sha256_file_hex;
use std::ffi::c_void;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use winhttp::{http_download_to_file, http_get_string};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASES_PAGE_URL: &str = "https://github.com/Flacozyabra/Tiny_Sink/releases";
const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/Flacozyabra/Tiny_Sink/releases/latest";

pub const WM_APP_UPDATE_CHECK_DONE: u32 = WM_APP + 30;
pub const WM_APP_UPDATE_PROGRESS: u32 = WM_APP + 31;
pub const WM_APP_UPDATE_DOWNLOAD_DONE: u32 = WM_APP + 32;

pub(crate) const EXPECTED_ASSET_NAME: &str = "Tiny_Sink.exe";
pub(crate) const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";
const STAGED_NEW_NAME: &str = "Tiny_Sink.exe.new";
const BACKUP_OLD_NAME: &str = "Tiny_Sink.exe.old";
const FINISH_UPDATE_FLAG: &str = "--finish-update";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: String,
    pub tag_name: String,
    pub html_url: String,
    pub exe_url: String,
    pub exe_name: String,
    pub sha256_url: Option<String>,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub enum UpdateCheckOutcome {
    UpToDate { current: String },
    Available(ReleaseInfo),
    Error(String),
}

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub speed_bps: f64,
}

#[derive(Debug, Clone)]
pub enum DownloadOutcome {
    Ready(PathBuf),
    Cancelled,
    Error(String),
}

#[derive(Clone)]
pub struct DownloadHandle {
    cancel_flag: Arc<AtomicBool>,
}

impl DownloadHandle {
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Release);
    }
}

/// Returns current Unix timestamp in seconds.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Checks GitHub for a newer release synchronously.
pub fn check_latest_release() -> UpdateCheckOutcome {
    let body = match http_get_string(
        LATEST_RELEASE_API_URL,
        Some("Accept: application/vnd.github+json\r\nX-GitHub-Api-Version: 2022-11-28\r\n"),
        512 * 1024,
    ) {
        Ok(b) => b,
        Err(err) => return UpdateCheckOutcome::Error(err),
    };

    let info = match parse_github_release_json(&body) {
        Ok(i) => i,
        Err(err) => return UpdateCheckOutcome::Error(err),
    };

    if is_newer_version(APP_VERSION, &info.version) {
        UpdateCheckOutcome::Available(info)
    } else {
        UpdateCheckOutcome::UpToDate {
            current: APP_VERSION.to_string(),
        }
    }
}

/// Spawns a background thread to check for updates and posts `WM_APP_UPDATE_CHECK_DONE`.
pub fn spawn_check_thread(hwnd: HWND, manual: bool) {
    let hwnd_raw = hwnd.0 as isize;
    thread::spawn(move || {
        let outcome = check_latest_release();
        let boxed = Box::into_raw(Box::new(outcome));
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd_raw as *mut c_void)),
                WM_APP_UPDATE_CHECK_DONE,
                WPARAM(usize::from(manual)),
                LPARAM(boxed as isize),
            );
        }
    });
}

/// Spawns a background thread that downloads `Tiny_Sink.exe`, verifies SHA-256, and notifies `hwnd`.
pub fn spawn_download_thread(
    hwnd: HWND,
    generation: usize,
    info: ReleaseInfo,
) -> Result<DownloadHandle, String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Не найден путь к исполняемому файлу: {e}"))?;
    let dest_dir = current_exe
        .parent()
        .ok_or_else(|| "Некорректный каталог исполняемого файла.".to_string())?;
    let staged_path = dest_dir.join(STAGED_NEW_NAME);

    // Fail fast if folder is read-only (e.g. Program Files without elevation).
    let probe_path = dest_dir.join(".tiny_sink_write_probe.tmp");
    if let Err(e) = File::create(&probe_path) {
        return Err(format!(
            "Нет прав записи в папку программы ({}). Переместите Tiny_Sink.exe в пользовательскую папку или обновите вручную:\n{e}",
            dest_dir.display()
        ));
    }
    let _ = fs::remove_file(&probe_path);

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_worker = Arc::clone(&cancel_flag);
    let hwnd_raw = hwnd.0 as isize;

    thread::spawn(move || {
        let target_hwnd = HWND(hwnd_raw as *mut c_void);
        let outcome = download_and_verify(&info, &staged_path, &cancel_worker, |prog| {
            let boxed = Box::into_raw(Box::new(prog));
            unsafe {
                let _ = PostMessageW(
                    Some(target_hwnd),
                    WM_APP_UPDATE_PROGRESS,
                    WPARAM(generation),
                    LPARAM(boxed as isize),
                );
            }
        });

        let boxed = Box::into_raw(Box::new(outcome));
        unsafe {
            let _ = PostMessageW(
                Some(target_hwnd),
                WM_APP_UPDATE_DOWNLOAD_DONE,
                WPARAM(generation),
                LPARAM(boxed as isize),
            );
        }
    });

    Ok(DownloadHandle { cancel_flag })
}

fn download_and_verify<F>(
    info: &ReleaseInfo,
    staged_path: &Path,
    cancel_flag: &AtomicBool,
    mut on_progress: F,
) -> DownloadOutcome
where
    F: FnMut(DownloadProgress),
{
    let max_retries = 3;
    let mut last_err = DownloadOutcome::Error("Неизвестная ошибка загрузки.".to_string());

    for attempt in 0..max_retries {
        if cancel_flag.load(Ordering::Acquire) {
            let _ = fs::remove_file(staged_path);
            return DownloadOutcome::Cancelled;
        }
        match http_download_to_file(&info.exe_url, staged_path, cancel_flag, &mut on_progress) {
            Ok(()) => {
                last_err = DownloadOutcome::Ready(staged_path.to_path_buf());
                break;
            }
            Err(DownloadOutcome::Cancelled) => {
                let _ = fs::remove_file(staged_path);
                return DownloadOutcome::Cancelled;
            }
            Err(err) => {
                let _ = fs::remove_file(staged_path);
                last_err = err;
                if attempt + 1 < max_retries {
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    }

    if !matches!(last_err, DownloadOutcome::Ready(_)) {
        return last_err;
    }

    if let Some(sha_url) = info.sha256_url.as_deref() {
        let sums = match http_get_string(sha_url, None, 64 * 1024) {
            Ok(s) => s,
            Err(err) => {
                let _ = fs::remove_file(staged_path);
                return DownloadOutcome::Error(format!(
                    "Не удалось скачать {CHECKSUMS_ASSET_NAME} для проверки целостности:\n{err}"
                ));
            }
        };
        let actual_sha = match sha256_file_hex(staged_path) {
            Ok(h) => h,
            Err(err) => {
                let _ = fs::remove_file(staged_path);
                return DownloadOutcome::Error(err);
            }
        };
        if let Err(err) = verify_sha256_sums(&sums, &info.exe_name, &actual_sha) {
            let _ = fs::remove_file(staged_path);
            return DownloadOutcome::Error(err);
        }
    }

    DownloadOutcome::Ready(staged_path.to_path_buf())
}

/// Replaces the running binary with `staged_new` and spawns the updated process with `--finish-update <pid>`.
pub fn apply_staged_update_and_launch(staged_new: &Path) -> Result<(), String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("Не найден путь текущего exe: {e}"))?;
    let dest_dir = current_exe
        .parent()
        .ok_or_else(|| "Некорректный каталог exe.".to_string())?;
    let old_exe = dest_dir.join(BACKUP_OLD_NAME);

    if old_exe.exists() {
        let _ = fs::remove_file(&old_exe);
    }

    fs::rename(&current_exe, &old_exe).map_err(|e| {
        let _ = fs::remove_file(staged_new);
        format!("Не удалось переименовать текущий исполняемый файл:\n{e}")
    })?;

    if let Err(e) = fs::rename(staged_new, &current_exe) {
        let _ = fs::rename(&old_exe, &current_exe);
        let _ = fs::remove_file(staged_new);
        return Err(format!("Не удалось переместить обновлённый файл:\n{e}"));
    }

    let pid_arg = std::process::id().to_string();
    if let Err(e) = Command::new(&current_exe)
        .args([FINISH_UPDATE_FLAG, &pid_arg])
        .current_dir(dest_dir)
        .spawn()
    {
        // Roll back cleanly so the user never ends up without an executable.
        let _ = fs::rename(&current_exe, staged_new);
        let _ = fs::rename(&old_exe, &current_exe);
        return Err(format!(
            "Новая версия применена, но не удалось запустить обновлённый процесс (выполнен откат):\n{e}"
        ));
    }

    Ok(())
}

/// Handles `--finish-update <old_pid>` handoff and cleans up leftover `.old`/`.new` files at startup.
pub fn handle_startup_update_args() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == FINISH_UPDATE_FLAG {
            if let Some(pid_str) = args.next() {
                if let Ok(old_pid) = pid_str.parse::<u32>() {
                    wait_for_process_exit(old_pid, 10_000);
                }
            }
            cleanup_old_binaries_with_retry();
            return;
        }
    }
    cleanup_old_binaries_once();
}

fn wait_for_process_exit(pid: u32, timeout_ms: u32) {
    if pid == 0 || pid == std::process::id() {
        return;
    }
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(handle, timeout_ms) == WAIT_OBJECT_0;
            let _ = CloseHandle(handle);
        }
    }
}

fn cleanup_old_binaries_with_retry() {
    for attempt in 0..8 {
        if cleanup_old_binaries_once() {
            return;
        }
        if attempt < 7 {
            thread::sleep(Duration::from_millis(120));
        }
    }
}

fn cleanup_old_binaries_once() -> bool {
    let Ok(current_exe) = std::env::current_exe() else {
        return true;
    };
    let Some(dir) = current_exe.parent() else {
        return true;
    };

    let mut all_removed = true;
    for name in [BACKUP_OLD_NAME, STAGED_NEW_NAME] {
        let p = dir.join(name);
        if p.exists() && fs::remove_file(&p).is_err() {
            all_removed = false;
        }
    }
    all_removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires live internet access to GitHub"]
    fn live_github_release_check_smoke() {
        match check_latest_release() {
            UpdateCheckOutcome::UpToDate { current } => assert_eq!(current, APP_VERSION),
            UpdateCheckOutcome::Available(info) => assert!(!info.exe_url.is_empty()),
            UpdateCheckOutcome::Error(err) => panic!("live GitHub check failed: {err}"),
        }
    }
}

