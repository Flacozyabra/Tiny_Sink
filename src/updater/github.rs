//! GitHub Releases JSON parsing, semver comparison, and SHA256SUMS verification.

use super::{ReleaseInfo, CHECKSUMS_ASSET_NAME, EXPECTED_ASSET_NAME, RELEASES_PAGE_URL};
use serde::Deserialize;
use std::path::Path;

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
    let payload: GhReleasePayload = serde_json::from_str(json_text)
        .map_err(|e| format!("Failed to parse GitHub response: {e}"))?;

    if payload.draft || payload.prerelease {
        return Err("Latest release is marked as draft or prerelease.".to_string());
    }

    let tag_name = payload
        .tag_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "GitHub response is missing tag_name.".to_string())?;

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
        let Some(url) = asset
            .browser_download_url
            .filter(|s| s.starts_with("https://"))
        else {
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

    let (exe_name, exe_url) = exe_asset.or(fallback_exe).ok_or_else(|| {
        format!("Executable {EXPECTED_ASSET_NAME} not found in release {tag_name}.")
    })?;

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
            format!("Checksum for {target_filename} not found in {CHECKSUMS_ASSET_NAME}.")
        })?;

    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("Invalid SHA-256 format in {CHECKSUMS_ASSET_NAME}."));
    }

    if expected != actual_sha256.to_ascii_lowercase() {
        return Err(format!(
            "SHA-256 mismatch (expected {expected}, got {actual_sha256})."
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::sha256::Sha256;

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
}

