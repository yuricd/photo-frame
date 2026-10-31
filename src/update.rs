use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

const LATEST_URL: &str = "https://api.github.com/repos/yuricd/photo-frame/releases/latest";
const ASSET_URL: &str =
    "https://github.com/yuricd/photo-frame/releases/latest/download/photo-frame-aarch64";
/// ureq's default cap is 10 MB. The release binary is about 14 MB.
const BODY_LIMIT: u64 = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Check GitHub for a newer release, replace this executable, then re-exec.
/// `Ok` means this build is already current. A successful install does not return.
pub fn apply() -> Result<(), String> {
    if !needs_update(&latest_tag()?, env!("CARGO_PKG_VERSION"))? {
        return Ok(());
    }
    if std::env::consts::ARCH != "aarch64" {
        return Err(format!(
            "updates install only on aarch64 (this is {})",
            std::env::consts::ARCH
        ));
    }
    let bytes = download(ASSET_URL)?;
    let exe = std::env::current_exe().map_err(|err| format!("running executable: {err}"))?;
    install_bytes(&exe, &bytes)?;
    let err = Command::new(&exe).args(std::env::args_os().skip(1)).exec();
    Err(format!("restart: {err}"))
}

fn latest_tag() -> Result<String, String> {
    let mut response = agent()
        .get(LATEST_URL)
        .header("User-Agent", "photo-frame")
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(404) => "no release published".to_string(),
            other => format!("check for updates: {other}"),
        })?;
    let release: GithubRelease = response
        .body_mut()
        .read_json()
        .map_err(|err| format!("check for updates: {err}"))?;
    Ok(release.tag_name)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let mut response = agent()
        .get(url)
        .header("User-Agent", "photo-frame")
        .call()
        .map_err(|err| format!("download update: {err}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_vec()
        .map_err(|err| format!("download update: {err}"))?;
    if bytes.is_empty() {
        return Err("download was empty".to_string());
    }
    Ok(bytes)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .build()
        .into()
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

fn needs_update(tag: &str, current: &str) -> Result<bool, String> {
    Ok(parse_version(tag)? > parse_version(current)?)
}

/// `0.1.0` or `v0.1.0`. Anything else, including a pre-release suffix, is rejected.
fn parse_version(tag: &str) -> Result<[u64; 3], String> {
    let stripped = tag.trim();
    let stripped = stripped.strip_prefix('v').unwrap_or(stripped);
    let err = || format!("release tag is not a version: {tag}");
    let mut parts = stripped.split('.');
    let major = number(parts.next()).ok_or_else(err)?;
    let minor = number(parts.next()).ok_or_else(err)?;
    let patch = number(parts.next()).ok_or_else(err)?;
    if parts.next().is_some() {
        return Err(err());
    }
    Ok([major, minor, patch])
}

fn number(part: Option<&str>) -> Option<u64> {
    let part = part?;
    if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

/// Write `photo-frame.new` beside `exe`, fsync, then rename over `exe`.
/// A power cut leaves the old binary or the new one.
fn install_bytes(exe: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() {
        return Err("download was empty".to_string());
    }
    let dir = exe
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| "executable has no directory".to_string())?;
    let tmp = dir.join("photo-frame.new");
    if let Err(err) = write_new(&tmp, bytes) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, exe) {
        let _ = fs::remove_file(&tmp);
        return Err(err.to_string());
    }
    File::open(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| err.to_string())
}

fn write_new(tmp: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = File::create(tmp).map_err(|err| err.to_string())?;
    file.write_all(bytes).map_err(|err| err.to_string())?;
    let mut permissions = file
        .metadata()
        .map_err(|err| err.to_string())?
        .permissions();
    permissions.set_mode(0o755);
    file.set_permissions(permissions)
        .map_err(|err| err.to_string())?;
    file.sync_all().map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numbers_not_text() {
        assert!(!needs_update("v0.1.0", "0.1.0").unwrap());
        assert!(!needs_update("v0.0.9", "0.1.0").unwrap());
        assert!(needs_update("v0.1.1", "0.1.0").unwrap());
        assert!(needs_update("v0.10.0", "0.9.0").unwrap());
        assert!(needs_update("  v1.0.0  ", "0.9.0").unwrap());
        for tag in ["", "v0.1", "v0.1.0-rc.1", "latest", "v1.2.3.4"] {
            assert!(needs_update(tag, "0.1.0").is_err(), "{tag}");
        }
    }

    #[test]
    fn install_replaces_the_executable() {
        let dir = scratch("install");
        let exe = dir.join("photo-frame");
        fs::write(&exe, b"old").unwrap();
        install_bytes(&exe, b"new-bytes").unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new-bytes");
        assert!(!dir.join("photo-frame.new").exists());
        let mode = fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn empty_download_leaves_the_executable() {
        let dir = scratch("empty");
        let exe = dir.join("photo-frame");
        fs::write(&exe, b"old").unwrap();
        assert_eq!(install_bytes(&exe, b"").unwrap_err(), "download was empty");
        assert_eq!(fs::read(&exe).unwrap(), b"old");
        assert!(!dir.join("photo-frame.new").exists());
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("photo-frame-update-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
