//! Compiles the Windows application icon and version resource into `Tiny_Sink.exe`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/Tiny_Sink.rc");
    println!("cargo:rerun-if-changed=assets/Tiny_Sink.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let res_path = out_dir.join("Tiny_Sink.res");
    let rc_exe = find_rc_exe().expect("Windows SDK rc.exe not found");

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_path)
        .arg("assets/Tiny_Sink.rc")
        .status()
        .expect("Failed to execute Windows SDK rc.exe");

    assert!(status.success(), "rc.exe exited with status {status}");
    println!("cargo:rustc-link-arg-bins={}", res_path.display());
}

fn find_rc_exe() -> Option<PathBuf> {
    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    let arch_dir = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "x64",
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    for env_key in ["WindowsSdkDir", "ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(val) = env::var_os(env_key) {
            let p = PathBuf::from(val);
            if env_key == "WindowsSdkDir" {
                roots.push(p);
            } else {
                roots.push(p.join("Windows Kits").join("10"));
            }
        }
    }

    for root in roots {
        let bin_dir = root.join("bin");
        if let Some(found) = search_sdk_bin(&bin_dir, arch_dir) {
            return Some(found);
        }
    }
    None
}

fn search_sdk_bin(bin_dir: &Path, arch_dir: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(bin_dir).ok()?;
    let mut versions: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    versions.reverse();

    for ver_dir in versions {
        let candidate = ver_dir.join(arch_dir).join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let direct = bin_dir.join(arch_dir).join("rc.exe");
    if direct.is_file() {
        return Some(direct);
    }
    None
}
