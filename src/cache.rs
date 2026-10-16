use std::collections::HashSet;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageDecoder, ImageReader};

use crate::settings::Settings;

pub const FRAME_WIDTH: u32 = 1024;
pub const FRAME_HEIGHT: u32 = 600;

pub fn images_dir() -> Result<PathBuf, String> {
    Ok(Settings::dir()?.join("images"))
}

pub fn image_path(dir: &Path, id: &str) -> Result<PathBuf, String> {
    check_id(id)?;
    Ok(dir.join(format!("{id}.jpg")))
}

/// Decode `bytes`, apply EXIF orientation, cover-crop to 1024×600, and write `{id}.jpg`.
#[allow(dead_code)] // sync thread writes the cache
pub fn store_preview(dir: &Path, id: &str, bytes: &[u8]) -> Result<(), String> {
    let path = image_path(dir, id)?;
    let cropped = cover_crop(decode_oriented(bytes)?);
    fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    cropped.save(&path).map_err(|err| err.to_string())
}

/// Delete `{id}.jpg` files whose ids are not in `keep`.
/// A missing directory is an empty cache. Call this only after a successful album
/// fetch. A failed fetch must leave the files in place.
#[allow(dead_code)] // sync thread prunes after a successful album fetch
pub fn prune(dir: &Path, keep: &HashSet<String>) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jpg") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !keep.contains(id) {
            fs::remove_file(&path).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}

/// Decode `bytes` and rotate the pixels to match the EXIF orientation.
/// `load_from_memory` leaves that tag unused, so phone photos stay sideways.
fn decode_oriented(bytes: &[u8]) -> Result<DynamicImage, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| err.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|err| err.to_string())?;
    let orientation = decoder.orientation().map_err(|err| err.to_string())?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|err| err.to_string())?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// Scale so the frame is covered, then center-crop to 1024×600.
fn cover_crop(image: DynamicImage) -> DynamicImage {
    image.resize_to_fill(
        FRAME_WIDTH,
        FRAME_HEIGHT,
        image::imageops::FilterType::Lanczos3,
    )
}

fn check_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid id: {id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("photo-frame-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn images_dir_is_under_photo_frame_home() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            images_dir().unwrap(),
            PathBuf::from(home).join(".photo-frame").join("images")
        );
    }

    #[test]
    fn cover_crop_is_frame_size() {
        for (width, height) in [(2000, 1000), (400, 2000), (1024, 600), (10, 10)] {
            let cropped = cover_crop(DynamicImage::new_rgb8(width, height));
            assert_eq!(
                (cropped.width(), cropped.height()),
                (FRAME_WIDTH, FRAME_HEIGHT),
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn prune_drops_ids_missing_from_album() {
        let dir = scratch("prune");
        for id in ["keep", "gone", "also-gone"] {
            fs::write(dir.join(format!("{id}.jpg")), b"jpeg").unwrap();
        }
        fs::write(dir.join("notes.txt"), b"leave").unwrap();
        fs::write(dir.join("not a safe id.jpg"), b"jpeg").unwrap();
        let keep = HashSet::from(["keep".to_string()]);
        prune(&dir, &keep).unwrap();
        assert!(dir.join("keep.jpg").is_file());
        assert!(!dir.join("gone.jpg").exists());
        assert!(!dir.join("also-gone.jpg").exists());
        assert!(!dir.join("not a safe id.jpg").exists());
        assert!(dir.join("notes.txt").is_file());
    }

    #[test]
    fn prune_missing_dir_is_empty() {
        let dir = scratch("absent").join("images");
        assert!(prune(&dir, &HashSet::new()).is_ok());
    }

    #[test]
    fn store_rejects_path_ids() {
        let dir = scratch("bad-id");
        assert_eq!(
            store_preview(&dir, "../x", b"").unwrap_err(),
            "invalid id: ../x"
        );
        assert!(fs::read_dir(&dir).unwrap().next().is_none());
    }
}
