use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

const LATEST_URL: &str = "https://api.github.com/repos/yuricd/photo-frame/releases/latest";
const ASSET_NAME: &str = "photo-frame-aarch64";
/// ureq's default cap is 10 MB. The release binary is about 14 MB.
const BODY_LIMIT: u64 = 32 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Check GitHub for a newer release, require that release's sha256,
/// replace this executable, then re-exec.
/// `Ok` means this build is already current. A successful install does not return.
pub fn apply() -> Result<(), String> {
    let release = latest_release()?;
    if !needs_update(&release.tag_name, env!("CARGO_PKG_VERSION"))? {
        return Ok(());
    }
    if std::env::consts::ARCH != "aarch64" {
        return Err(format!(
            "updates install only on aarch64 (this is {})",
            std::env::consts::ARCH
        ));
    }
    let url = download_url(&release.tag_name)?;
    let digest = asset_digest(&release.assets)?;
    let bytes = download(&url)?;
    check_digest(&bytes, &digest)?;
    let exe = std::env::current_exe().map_err(|err| format!("running executable: {err}"))?;
    install_bytes(&exe, &bytes)?;
    let err = Command::new(&exe).args(std::env::args_os().skip(1)).exec();
    Err(format!("restart: {err}"))
}

fn latest_release() -> Result<GithubRelease, String> {
    let mut response = agent()
        .get(LATEST_URL)
        .header("User-Agent", "photo-frame")
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(404) => "no release published".to_string(),
            other => format!("check for updates: {other}"),
        })?;
    response
        .body_mut()
        .read_json()
        .map_err(|err| format!("check for updates: {err}"))
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
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    digest: Option<String>,
}

/// `v1.2.3` only. The tag is placed in the download path, so anything else is rejected.
fn release_tag(tag: &str) -> Result<String, String> {
    let trimmed = tag.trim();
    if !trimmed.starts_with('v') {
        return Err(format!("release tag is not a version: {tag}"));
    }
    parse_version(trimmed)?;
    Ok(trimmed.to_string())
}

fn download_url(tag: &str) -> Result<String, String> {
    let tag = release_tag(tag)?;
    Ok(format!(
        "https://github.com/yuricd/photo-frame/releases/download/{tag}/{ASSET_NAME}"
    ))
}

fn asset_digest(assets: &[GithubAsset]) -> Result<String, String> {
    let mut matches = assets.iter().filter(|asset| asset.name == ASSET_NAME);
    let asset = matches
        .next()
        .ok_or_else(|| format!("release has no {ASSET_NAME}"))?;
    if matches.next().is_some() {
        return Err(format!("release lists {ASSET_NAME} more than once"));
    }
    let digest = asset
        .digest
        .as_deref()
        .ok_or_else(|| format!("release asset {ASSET_NAME} has no digest"))?;
    parse_sha256(digest)?;
    Ok(digest.to_string())
}

/// `sha256:` plus 64 lowercase hex digits.
fn parse_sha256(digest: &str) -> Result<&str, String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err("release digest is not a sha256".to_string());
    };
    if hex.len() == 64
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        Ok(hex)
    } else {
        Err("release digest is not a sha256".to_string())
    }
}

fn check_digest(bytes: &[u8], digest: &str) -> Result<(), String> {
    let expected = parse_sha256(digest)?;
    if sha256_hex(bytes) != expected {
        return Err("download does not match the release digest".to_string());
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    let mut hex = String::with_capacity(hash.len() * 2);
    for byte in hash {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
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
    fn download_url_uses_the_release_tag() {
        let url =
            "https://github.com/yuricd/photo-frame/releases/download/v1.2.3/photo-frame-aarch64";
        assert_eq!(download_url("v1.2.3").unwrap(), url);
        assert_eq!(download_url("  v1.2.3\n").unwrap(), url);
        for tag in [
            "1.2.3",
            "v1.2",
            "v1.2.3-rc.1",
            "v1.2.3/photo-frame",
            "latest",
            "",
        ] {
            assert!(download_url(tag).is_err(), "{tag}");
        }
    }

    #[test]
    fn digest_must_be_sha256() {
        let hex = "ab".repeat(32);
        assert_eq!(parse_sha256(&format!("sha256:{hex}")).unwrap(), hex);
        for digest in [
            hex.clone(),
            format!("sha256:{}", "ab".repeat(31)),
            format!("sha256:{}", "AB".repeat(32)),
            format!("sha512:{hex}"),
            "sha256:".to_string(),
        ] {
            assert_eq!(
                parse_sha256(&digest).unwrap_err(),
                "release digest is not a sha256",
                "{digest}"
            );
        }
    }

    #[test]
    fn asset_digest_requires_one_checksum() {
        let digest = format!("sha256:{}", "cd".repeat(32));
        let assets = [
            GithubAsset {
                name: "notes.txt".to_string(),
                digest: None,
            },
            GithubAsset {
                name: ASSET_NAME.to_string(),
                digest: Some(digest.clone()),
            },
        ];
        assert_eq!(asset_digest(&assets).unwrap(), digest);
        assert_eq!(
            asset_digest(&[]).unwrap_err(),
            format!("release has no {ASSET_NAME}")
        );
        assert_eq!(
            asset_digest(&[GithubAsset {
                name: ASSET_NAME.to_string(),
                digest: None,
            }])
            .unwrap_err(),
            format!("release asset {ASSET_NAME} has no digest")
        );
        assert_eq!(
            asset_digest(&[
                GithubAsset {
                    name: ASSET_NAME.to_string(),
                    digest: Some(digest.clone()),
                },
                GithubAsset {
                    name: ASSET_NAME.to_string(),
                    digest: Some(digest),
                },
            ])
            .unwrap_err(),
            format!("release lists {ASSET_NAME} more than once")
        );
    }

    #[test]
    fn checksum_must_match_the_download() {
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        check_digest(b"abc", digest).unwrap();
        let wrong = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ae";
        assert_eq!(
            check_digest(b"abc", wrong).unwrap_err(),
            "download does not match the release digest"
        );
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
