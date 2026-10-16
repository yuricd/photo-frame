use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const DEFAULT_SLIDE_SECONDS: u64 = 10;
pub const DEFAULT_POLL_MINUTES: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub immich_url: String,
    pub api_key: String,
    pub album_id: String,
    pub slide_seconds: u64,
    pub poll_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            immich_url: String::new(),
            api_key: String::new(),
            album_id: String::new(),
            slide_seconds: DEFAULT_SLIDE_SECONDS,
            poll_minutes: DEFAULT_POLL_MINUTES,
        }
    }
}

impl Settings {
    pub fn dir() -> Result<PathBuf, String> {
        let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
        Ok(PathBuf::from(home).join(".photo-frame"))
    }

    pub fn path() -> Result<PathBuf, String> {
        Ok(Self::dir()?.join("settings.toml"))
    }

    /// `Ok(None)` when the file is missing. That is a first launch, not an error.
    #[allow(dead_code)] // used once the settings screen exists
    pub fn load() -> Result<Option<Settings>, String> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Option<Settings>, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.to_string()),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|err| err.to_string())
    }

    #[allow(dead_code)] // used once the settings screen exists
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        if self.slide_seconds == 0 {
            return Err("slide seconds must be at least 1".to_string());
        }
        if self.poll_minutes == 0 {
            return Err("poll minutes must be at least 1".to_string());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|err| err.to_string())?;
        fs::write(path, text).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "photo-frame-settings-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn path_is_under_home() {
        let home = std::env::var("HOME").unwrap();
        let path = Settings::path().unwrap();
        assert_eq!(
            path,
            PathBuf::from(home)
                .join(".photo-frame")
                .join("settings.toml")
        );
    }

    #[test]
    fn missing_file_is_first_launch() {
        let path = scratch("missing").join("settings.toml");
        assert_eq!(Settings::load_from(&path).unwrap(), None);
    }

    #[test]
    fn round_trip() {
        let path = scratch("round").join("nested").join("settings.toml");
        let settings = Settings {
            immich_url: "https://immich.example".to_string(),
            api_key: "secret".to_string(),
            album_id: "album-1".to_string(),
            slide_seconds: 15,
            poll_minutes: 5,
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), Some(settings));
    }

    #[test]
    fn missing_fields_use_defaults() {
        let dir = scratch("partial");
        let path = dir.join("settings.toml");
        fs::write(&path, "immich_url = \"https://immich.example\"\n").unwrap();
        let settings = Settings::load_from(&path).unwrap().unwrap();
        assert_eq!(settings.immich_url, "https://immich.example");
        assert_eq!(settings.api_key, "");
        assert_eq!(settings.album_id, "");
        assert_eq!(settings.slide_seconds, DEFAULT_SLIDE_SECONDS);
        assert_eq!(settings.poll_minutes, DEFAULT_POLL_MINUTES);
    }

    #[test]
    fn zero_duration_is_rejected() {
        let path = scratch("zero").join("settings.toml");
        let mut settings = Settings::default();
        settings.slide_seconds = 0;
        assert_eq!(
            settings.save_to(&path).unwrap_err(),
            "slide seconds must be at least 1"
        );
        settings.slide_seconds = DEFAULT_SLIDE_SECONDS;
        settings.poll_minutes = 0;
        assert_eq!(
            settings.save_to(&path).unwrap_err(),
            "poll minutes must be at least 1"
        );
        assert!(!path.exists());
    }

    #[test]
    fn bad_toml_is_an_error() {
        let dir = scratch("bad");
        let path = dir.join("settings.toml");
        fs::write(&path, "slide_seconds = \"nope\"\n").unwrap();
        assert!(Settings::load_from(&path).is_err());
    }
}
