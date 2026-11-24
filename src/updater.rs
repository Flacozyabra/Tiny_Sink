//! Background and manual GitHub update checks, WinHTTP downloads, SHA-256 verification, and self-replacement.

use crate::config::to_wide_null;
use serde::Deserialize;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASES_PAGE_URL: &str = "https://github.com/Flacozyabra/Tiny_Sink/releases";
const LATEST_RELEASE_API_URL: &str = "https://api.github.com/repos/Flacozyabra/Tiny_Sink/releases/latest";

pub const WM_APP_UPDATE_CHECK_DONE: u32 = WM_APP + 30;
pub const WM_APP_UPDATE_PROGRESS: u32 = WM_APP + 31;
pub const WM_APP_UPDATE_DOWNLOAD_DONE: u32 = WM_APP + 32;

const EXPECTED_ASSET_NAME: &str = "Tiny_Sink.exe";
const CHECKSUMS_ASSET_NAME: &str = "SHA256SUMS.txt";
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

/// Parses a tag/version string like `v0.2.0` or `0.1.0-rc1` into numeric parts and prerelease flag.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedVersion {
    nums: Vec<u64>,
    prerelease: Option<String>,
}

fn parse_version(raw: &str) -> Option<ParsedVersion> {
    let trimmed = raw.trim().trim_start_matches(['v', 'V']);
    if trimmed.is_empty() {
        return None;
    }
    let (core, prerelease) = match trimmed.split_once('-') {
        Some((c, p)) => (c, Some(p.to_ascii_lowercase())),
        None => (trimmed, None),
    };
    let mut nums = Vec::new();
    for part in core.split('.') {
        if part.is_empty() {
            return None;
        }
        nums.push(part.parse::<u64>().ok()?);
    }
    while nums.len() > 1 && nums.last() == Some(&0) {
        nums.pop();
    }
    Some(ParsedVersion { nums, prerelease })
}

/// Returns true when `candidate` is strictly newer than `current` according to semver precedence.
pub fn is_newer_version(current: &str, candidate: &str) -> bool {
    let (Some(curr), Some(cand)) = (parse_version(current), parse_version(candidate)) else {
        return false;
    };
    let len = curr.nums.len().max(cand.nums.len());
    for i in 0..len {
        let a = curr.nums.get(i).copied().unwrap_or(0);
        let b = cand.nums.get(i).copied().unwrap_or(0);
        if b != a {
            return b > a;
        }
    }
    match (&curr.prerelease, &cand.prerelease) {
        (Some(_), None) => true,
        (Some(a), Some(b)) => b > a,
        _ => false,
    }
}

#[derive(Deserialize)]
struct GhReleasePayload {
    tag_name: Option<String>,
    html_url: Option<String>,
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAssetPayload>,
}

#[derive(Deserialize)]
struct GhAssetPayload {
    name: Option<String>,
    browser_download_url: Option<String>,
}

/// Parses GitHub `/releases/latest` JSON into validated `ReleaseInfo`.
pub fn parse_github_release_json(json_text: &str) -> Result<ReleaseInfo, String> {
    let payload: GhReleasePayload =
        serde_json::from_str(json_text).map_err(|e| format!("Ошибка разбора ответа GitHub: {e}"))?;

    if payload.draft || payload.prerelease {
        return Err("Последний релиз помечен как черновик или пререлиз.".to_string());
    }

    let tag_name = payload
        .tag_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "В ответе GitHub отсутствует tag_name.".to_string())?;

    let html_url = payload
        .html_url
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| RELEASES_PAGE_URL.to_string());

    let mut exe_asset: Option<(String, String)> = None;
    let mut fallback_exe: Option<(String, String)> = None;
    let mut sha256_url: Option<String> = None;

    for asset in payload.assets {
        let Some(name) = asset.name.filter(|s| !s.trim().is_empty()) else {
            continue;
        };
        let Some(url) = asset.browser_download_url.filter(|s| s.starts_with("https://")) else {
            continue;
        };
        if name.eq_ignore_ascii_case(EXPECTED_ASSET_NAME) {
            exe_asset = Some((name, url));
        } else if name.to_ascii_lowercase().ends_with(".exe") && fallback_exe.is_none() {
            fallback_exe = Some((name, url));
        } else if name.eq_ignore_ascii_case(CHECKSUMS_ASSET_NAME) {
            sha256_url = Some(url);
        }
    }

    let (exe_name, exe_url) = exe_asset
        .or(fallback_exe)
        .ok_or_else(|| format!("В релизе {tag_name} не найден исполняемый файл {EXPECTED_ASSET_NAME}."))?;

    let version = tag_name.trim_start_matches(['v', 'V']).to_string();
    let notes = payload
        .body
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("\n");

    Ok(ReleaseInfo {
        version,
        tag_name,
        html_url,
        exe_url,
        exe_name,
        sha256_url,
        notes,
    })
}

/// Parses `<hex_sha256> [*| ]<filename>` entries and verifies `actual_sha256` against `target_filename`.
pub fn verify_sha256_sums(
    sums_text: &str,
    target_filename: &str,
    actual_sha256: &str,
) -> Result<(), String> {
    let expected = sums_text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let mut parts = trimmed.splitn(2, |c: char| c.is_ascii_whitespace());
            let hash = parts.next()?.trim();
            let raw_name = parts.next()?.trim().trim_start_matches('*');
            let file_only = Path::new(raw_name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(raw_name);
            Some((hash.to_ascii_lowercase(), file_only.to_string()))
        })
        .find(|(_, name)| name.eq_ignore_ascii_case(target_filename))
        .map(|(hash, _)| hash)
        .ok_or_else(|| {
            format!("В {CHECKSUMS_ASSET_NAME} отсутствует контрольная сумма для {target_filename}.")
        })?;

    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("Некорректный формат хэша в {CHECKSUMS_ASSET_NAME}."));
    }

    if expected != actual_sha256.to_ascii_lowercase() {
        return Err(format!(
            "Контрольная сумма SHA-256 не совпадает (ожидалось {expected}, получено {actual_sha256})."
        ));
    }

    Ok(())
}

// ============================================================================
// Minimal pure-Rust SHA-256 (zero FFI/crate overhead)
// ============================================================================

struct Sha256 {
    state: [u32; 8],
    len_bits: u64,
    buf: [u8; 64],
    buf_len: usize,
}

impl Sha256 {
    const INIT: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INIT,
            len_bits: 0,
            buf: [0; 64],
            buf_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len_bits = self.len_bits.wrapping_add((data.len() as u64) * 8);
        while !data.is_empty() {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
    }

    fn finish_hex(mut self) -> String {
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;
        if self.buf_len > 56 {
            self.buf[self.buf_len..64].fill(0);
            let block = self.buf;
            self.compress(&block);
            self.buf_len = 0;
        }
        self.buf[self.buf_len..56].fill(0);
        self.buf[56..64].copy_from_slice(&self.len_bits.to_be_bytes());
        let block = self.buf;
        self.compress(&block);

        let mut out = String::with_capacity(64);
        for word in self.state {
            use std::fmt::Write;
            let _ = write!(out, "{word:08x}");
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            let j = i * 4;
            w[i] = u32::from_be_bytes([block[j], block[j + 1], block[j + 2], block[j + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(Self::K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        let add = [a, b, c, d, e, f, g, h];
        for (s, delta) in self.state.iter_mut().zip(add) {
            *s = s.wrapping_add(delta);
        }
    }
}

pub fn sha256_file_hex(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("Не удалось открыть файл для проверки SHA-256: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Ошибка чтения файла при расчёте SHA-256: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finish_hex())
}

// ============================================================================
// Native WinHTTP HTTPS client
// ============================================================================

struct HInternet(*mut c_void);

impl Drop for HInternet {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedHttpsUrl {
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_https_url(url: &str) -> Result<ParsedHttpsUrl, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "Поддерживаются только HTTPS URL.".to_string())?;
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("Пустой хост в URL.".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.contains(']') => {
            let parsed = p.parse::<u16>().map_err(|_| "Некорректный порт в URL.".to_string())?;
            (h.to_string(), parsed)
        }
        _ => (authority.to_string(), 443),
    };
    Ok(ParsedHttpsUrl {
        host,
        port,
        path_and_query: path_and_query.to_string(),
    })
}

fn open_https_request(url: &str, headers: Option<&str>) -> Result<(HInternet, HInternet, HInternet), String> {
    let parsed = parse_https_url(url)?;
    let agent_w = to_wide_null(&format!("Tiny_Sink/{APP_VERSION}"));
    let host_w = to_wide_null(&parsed.host);
    let verb_w = to_wide_null("GET");
    let path_w = to_wide_null(&parsed.path_and_query);

    unsafe {
        let session = HInternet(WinHttpOpen(
            PCWSTR(agent_w.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.0.is_null() {
            return Err("Не удалось инициализировать WinHTTP.".to_string());
        }
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);

        let connect = HInternet(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), parsed.port, 0));
        if connect.0.is_null() {
            return Err(format!("Не удалось подключиться к {}.", parsed.host));
        }

        let request = HInternet(WinHttpOpenRequest(
            connect.0,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ));
        if request.0.is_null() {
            return Err("Не удалось создать HTTPS-запрос.".to_string());
        }

        let header_units: Option<Vec<u16>> = headers.map(|h| h.encode_utf16().collect());
        WinHttpSendRequest(request.0, header_units.as_deref(), None, 0, 0, 0)
            .map_err(|e| format!("Ошибка сети при отправке запроса: {e}"))?;

        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| format!("Сервер не ответил вовремя: {e}"))?;

        let mut status: u32 = 0;
        let mut status_len = std::mem::size_of::<u32>() as u32;
        let mut idx = 0u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut status_len,
            &mut idx,
        )
        .map_err(|e| format!("Не удалось прочитать HTTP-статус: {e}"))?;

        if !(200..300).contains(&status) {
            return Err(match status {
                403 | 429 => "Превышен лимит запросов GitHub API. Попробуйте чуть позже.".to_string(),
                404 => "Релиз или файл обновления не найден на GitHub (HTTP 404).".to_string(),
                _ => format!("Сервер вернул HTTP {status}."),
            });
        }

        Ok((session, connect, request))
    }
}

fn query_content_length(request: &HInternet) -> u64 {
    let mut len32: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let mut idx = 0u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut len32 as *mut u32 as *mut c_void),
            &mut size,
            &mut idx,
        )
    };
    if ok.is_ok() {
        len32 as u64
    } else {
        0
    }
}

fn http_get_string(url: &str, headers: Option<&str>, max_bytes: usize) -> Result<String, String> {
    let (_session, _connect, request) = open_https_request(url, headers)?;
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let mut available: u32 = 0;
        unsafe {
            WinHttpQueryDataAvailable(request.0, &mut available)
                .map_err(|e| format!("Ошибка чтения ответа: {e}"))?;
        }
        if available == 0 {
            break;
        }
        let mut remaining = available as usize;
        while remaining > 0 {
            let to_read = remaining.min(chunk.len());
            let mut read_now: u32 = 0;
            unsafe {
                WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr() as *mut c_void,
                    to_read as u32,
                    &mut read_now,
                )
                .map_err(|e| format!("Ошибка получения данных: {e}"))?;
            }
            if read_now == 0 {
                break;
            }
            let n = read_now as usize;
            if data.len() + n > max_bytes {
                return Err("Ответ сервера превышает допустимый размер.".to_string());
            }
            data.extend_from_slice(&chunk[..n]);
            remaining = remaining.saturating_sub(n);
        }
    }

    String::from_utf8(data).map_err(|_| "Ответ сервера не является корректным UTF-8.".to_string())
}

fn http_download_to_file<F>(
    url: &str,
    dest: &Path,
    cancel_flag: &AtomicBool,
    mut on_progress: F,
) -> Result<(), DownloadOutcome>
where
    F: FnMut(DownloadProgress),
{
    let (_session, _connect, request) =
        open_https_request(url, None).map_err(DownloadOutcome::Error)?;
    let total_bytes = query_content_length(&request);

    let mut file = File::create(dest)
        .map_err(|e| DownloadOutcome::Error(format!("Не удалось создать временный файл: {e}")))?;

    let mut downloaded_bytes: u64 = 0;
    let mut last_emit = Instant::now();
    let mut speed_window_start = Instant::now();
    let mut speed_window_bytes: u64 = 0;
    let mut last_speed_bps: f64 = 0.0;
    let mut chunk = [0u8; 16 * 1024];

    loop {
        if cancel_flag.load(Ordering::Acquire) {
            return Err(DownloadOutcome::Cancelled);
        }

        let mut available: u32 = 0;
        unsafe {
            WinHttpQueryDataAvailable(request.0, &mut available)
                .map_err(|e| DownloadOutcome::Error(format!("Ошибка сети при загрузке: {e}")))?;
        }
        if available == 0 {
            break;
        }

        let mut remaining = available as usize;
        while remaining > 0 {
            if cancel_flag.load(Ordering::Acquire) {
                return Err(DownloadOutcome::Cancelled);
            }
            let to_read = remaining.min(chunk.len());
            let mut read_now: u32 = 0;
            unsafe {
                WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr() as *mut c_void,
                    to_read as u32,
                    &mut read_now,
                )
                .map_err(|e| DownloadOutcome::Error(format!("Ошибка загрузки данных: {e}")))?;
            }
            if read_now == 0 {
                break;
            }
            let n = read_now as usize;
            file.write_all(&chunk[..n])
                .map_err(|e| DownloadOutcome::Error(format!("Ошибка записи на диск: {e}")))?;
            downloaded_bytes += n as u64;
            speed_window_bytes += n as u64;
            remaining = remaining.saturating_sub(n);

            let speed_elapsed = speed_window_start.elapsed().as_secs_f64();
            if speed_elapsed >= 0.4 {
                last_speed_bps = (speed_window_bytes as f64) / speed_elapsed;
                speed_window_start = Instant::now();
                speed_window_bytes = 0;
            }

            if last_emit.elapsed() >= Duration::from_millis(120) {
                last_emit = Instant::now();
                on_progress(DownloadProgress {
                    downloaded_bytes,
                    total_bytes,
                    speed_bps: last_speed_bps,
                });
            }
        }
    }

    file.flush()
        .map_err(|e| DownloadOutcome::Error(format!("Ошибка сохранения файла на диск: {e}")))?;

    on_progress(DownloadProgress {
        downloaded_bytes,
        total_bytes: total_bytes.max(downloaded_bytes),
        speed_bps: last_speed_bps,
    });
    Ok(())
}

// ============================================================================
// Background workers & native self-replacement
// ============================================================================

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
    fn compares_semver_tags_correctly() {
        assert!(is_newer_version("0.1.0", "v0.1.1"));
        assert!(is_newer_version("0.1.0", "v0.2.0"));
        assert!(is_newer_version("0.1.0", "1.0.0"));
        assert!(!is_newer_version("0.1.0", "v0.1.0"));
        assert!(!is_newer_version("0.2.0", "v0.1.9"));
        assert!(!is_newer_version("0.1.0", "v0.1.0-beta.1"));
        assert!(is_newer_version("0.1.0-beta.1", "v0.1.0"));
    }

    #[test]
    fn parses_github_release_payload() {
        let json = r####"{
            "tag_name": "v0.2.0",
            "html_url": "https://github.com/Flacozyabra/Tiny_Sink/releases/tag/v0.2.0",
            "body": "### Added\n- Auto update\n- Dark dialog",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "Tiny_Sink-v0.2.0-windows-x64.zip",
                    "browser_download_url": "https://github.com/Flacozyabra/Tiny_Sink/releases/download/v0.2.0/Tiny_Sink-v0.2.0-windows-x64.zip"
                },
                {
                    "name": "Tiny_Sink.exe",
                    "browser_download_url": "https://github.com/Flacozyabra/Tiny_Sink/releases/download/v0.2.0/Tiny_Sink.exe"
                },
                {
                    "name": "SHA256SUMS.txt",
                    "browser_download_url": "https://github.com/Flacozyabra/Tiny_Sink/releases/download/v0.2.0/SHA256SUMS.txt"
                }
            ]
        }"####;

        let info = parse_github_release_json(json).expect("valid release");
        assert_eq!(info.version, "0.2.0");
        assert_eq!(info.tag_name, "v0.2.0");
        assert_eq!(info.exe_name, "Tiny_Sink.exe");
        assert!(info.exe_url.ends_with("/Tiny_Sink.exe"));
        assert!(info.sha256_url.as_deref().unwrap().ends_with("/SHA256SUMS.txt"));
        assert!(info.notes.contains("Auto update"));
    }

    #[test]
    fn computes_standard_sha256_vectors() {
        let mut empty = Sha256::new();
        empty.update(b"");
        assert_eq!(
            empty.finish_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            abc.finish_hex(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verifies_sha256_sums_file() {
        let mut h = Sha256::new();
        h.update(b"test binary");
        let digest = h.finish_hex();
        let sums = format!(
            "{digest}  Tiny_Sink.exe\r\n0000000000000000000000000000000000000000000000000000000000000000  Tiny_Sink-v0.2.0-windows-x64.zip\r\n"
        );

        verify_sha256_sums(&sums, "Tiny_Sink.exe", &digest).expect("matching hash");
        assert!(verify_sha256_sums(
            &sums,
            "Tiny_Sink.exe",
            "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        )
        .is_err());
    }

    #[test]
    fn parses_https_urls() {
        assert_eq!(
            parse_https_url("https://api.github.com/repos/Flacozyabra/Tiny_Sink/releases/latest").unwrap(),
            ParsedHttpsUrl {
                host: "api.github.com".into(),
                port: 443,
                path_and_query: "/repos/Flacozyabra/Tiny_Sink/releases/latest".into(),
            }
        );
        assert!(parse_https_url("http://example.com/file.exe").is_err());
    }

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
