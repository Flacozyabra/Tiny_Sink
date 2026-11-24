//! Native WinHTTP HTTPS client with system proxy support and streaming download progress.

use super::{DownloadOutcome, DownloadProgress, APP_VERSION};
use crate::config::to_wide_null;
use std::ffi::c_void;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

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
        .ok_or_else(|| "Only HTTPS URLs are supported.".to_string())?;
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("Empty host in URL.".to_string());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !h.contains(']') => {
            let parsed = p
                .parse::<u16>()
                .map_err(|_| "Invalid port in URL.".to_string())?;
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

fn open_https_request(
    url: &str,
    headers: Option<&str>,
) -> Result<(HInternet, HInternet, HInternet), String> {
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
            return Err("Failed to initialize WinHTTP.".to_string());
        }
        let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);

        let connect = HInternet(WinHttpConnect(session.0, PCWSTR(host_w.as_ptr()), parsed.port, 0));
        if connect.0.is_null() {
            return Err(format!("Failed to connect to {}.", parsed.host));
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
            return Err("Failed to create HTTPS request.".to_string());
        }

        let header_units: Option<Vec<u16>> = headers.map(|h| h.encode_utf16().collect());
        WinHttpSendRequest(request.0, header_units.as_deref(), None, 0, 0, 0)
            .map_err(|e| format!("Network error sending request: {e}"))?;

        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|e| format!("Server did not respond in time: {e}"))?;

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
        .map_err(|e| format!("Failed to read HTTP status code: {e}"))?;

        if !(200..300).contains(&status) {
            return Err(match status {
                403 | 429 => "GitHub API rate limit exceeded. Please try again later.".to_string(),
                404 => "Release or asset not found on GitHub (HTTP 404).".to_string(),
                _ => format!("Server returned HTTP {status}."),
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

pub(crate) fn http_get_string(
    url: &str,
    headers: Option<&str>,
    max_bytes: usize,
) -> Result<String, String> {
    let (_session, _connect, request) = open_https_request(url, headers)?;
    let mut data = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let mut available: u32 = 0;
        unsafe {
            WinHttpQueryDataAvailable(request.0, &mut available)
                .map_err(|e| format!("Error reading response: {e}"))?;
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
                .map_err(|e| format!("Error receiving data: {e}"))?;
            }
            if read_now == 0 {
                break;
            }
            let n = read_now as usize;
            if data.len() + n > max_bytes {
                return Err("Server response exceeds maximum allowed size.".to_string());
            }
            data.extend_from_slice(&chunk[..n]);
            remaining = remaining.saturating_sub(n);
        }
    }

    String::from_utf8(data).map_err(|_| "Server response is not valid UTF-8.".to_string())
}

pub(crate) fn http_download_to_file<F>(
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
        .map_err(|e| DownloadOutcome::Error(format!("Failed to create temporary file: {e}")))?;

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
                .map_err(|e| DownloadOutcome::Error(format!("Network error while downloading: {e}")))?;
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
                .map_err(|e| DownloadOutcome::Error(format!("Failed reading download stream: {e}")))?;
            }
            if read_now == 0 {
                break;
            }
            let n = read_now as usize;
            file.write_all(&chunk[..n])
                .map_err(|e| DownloadOutcome::Error(format!("Failed writing update file to disk: {e}")))?;
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
        .map_err(|e| DownloadOutcome::Error(format!("Failed flushing update file to disk: {e}")))?;

    on_progress(DownloadProgress {
        downloaded_bytes,
        total_bytes: total_bytes.max(downloaded_bytes),
        speed_bps: last_speed_bps,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_urls() {
        assert_eq!(
            parse_https_url("https://api.github.com/repos/Flacozyabra/Tiny_Sink/releases/latest")
                .unwrap(),
            ParsedHttpsUrl {
                host: "api.github.com".into(),
                port: 443,
                path_and_query: "/repos/Flacozyabra/Tiny_Sink/releases/latest".into(),
            }
        );
        assert!(parse_https_url("http://example.com/file.exe").is_err());
    }
}

