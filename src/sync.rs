use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;

use crate::settings::Settings;

use super::lock;

/// Playlist, settings generation, and the last sync error.
/// Save increments `generation` so the sync thread can run immediately.
pub(crate) struct Shared {
    pub(crate) playlist: Vec<String>,
    pub(crate) generation: u64,
    pub(crate) last_error: Option<String>,
}

pub(crate) fn cached_ids(dir: &Path) -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return ids;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jpg") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if crate::cache::image_path(dir, id).is_err() {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids
}

pub(crate) fn spawn_sync(
    ctx: egui::Context,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<Condvar>,
    images_dir: PathBuf,
) {
    let report = Arc::clone(&shared);
    let spawned = thread::Builder::new()
        .name("photo-frame-sync".to_string())
        .spawn(move || sync_loop(ctx, shared, wake, images_dir));
    if let Err(err) = spawned {
        lock(&report).last_error = Some(format!("sync thread: {err}"));
    }
}

/// Read settings, refresh the cache, then sleep until the poll interval or a Save.
/// Save bumps `generation` and notifies `wake`, so the wait ends early.
fn sync_loop(
    ctx: egui::Context,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<Condvar>,
    images_dir: PathBuf,
) {
    loop {
        let generation = lock(&shared).generation;
        let settings = match Settings::load() {
            Ok(Some(settings)) => settings,
            Ok(None) => {
                wait_for_generation(&shared, &wake, generation, None);
                continue;
            }
            Err(err) => {
                publish(&shared, &ctx, None, Some(err));
                wait_for_generation(&shared, &wake, generation, Some(Duration::from_secs(60)));
                continue;
            }
        };
        match sync_once(&settings, &images_dir) {
            Ok(ids) => publish(&shared, &ctx, Some(ids), None),
            Err(err) => publish(&shared, &ctx, None, Some(err)),
        }
        let minutes = settings.poll_minutes.max(1);
        wait_for_generation(
            &shared,
            &wake,
            generation,
            Some(Duration::from_secs(minutes.saturating_mul(60))),
        );
    }
}

fn sync_once(settings: &Settings, images_dir: &Path) -> Result<Vec<String>, String> {
    let client = crate::immich::Client::new(&settings.immich_url, &settings.api_key)?;
    let images = client.list_images(&settings.album_id)?;
    let ids: Vec<String> = images.into_iter().map(|image| image.id).collect();
    update_cache(images_dir, &ids, |id| client.download_preview(id))?;
    Ok(ids)
}

/// Download ids that are not already `{id}.jpg`, then prune.
/// A download or decode error returns before prune, so a failed poll leaves the disk alone.
fn update_cache(
    dir: &Path,
    ids: &[String],
    mut download: impl FnMut(&str) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    for id in ids {
        let path = crate::cache::image_path(dir, id)?;
        if path.is_file() {
            continue;
        }
        let bytes = download(id)?;
        crate::cache::store_preview(dir, id, &bytes)?;
    }
    let keep: HashSet<String> = ids.iter().cloned().collect();
    crate::cache::prune(dir, &keep)
}

fn publish(
    shared: &Mutex<Shared>,
    ctx: &egui::Context,
    playlist: Option<Vec<String>>,
    error: Option<String>,
) {
    let changed = {
        let mut guard = lock(shared);
        let mut changed = false;
        if let Some(playlist) = playlist
            && guard.playlist != playlist
        {
            guard.playlist = playlist;
            changed = true;
        }
        if guard.last_error != error {
            guard.last_error = error;
            changed = true;
        }
        changed
    };
    if changed {
        ctx.request_repaint();
    }
}

fn wait_for_generation(
    shared: &Mutex<Shared>,
    wake: &Condvar,
    generation: u64,
    timeout: Option<Duration>,
) {
    let guard = lock(shared);
    let unchanged = |state: &mut Shared| state.generation == generation;
    match timeout {
        Some(timeout) => {
            if let Err(poisoned) = wake.wait_timeout_while(guard, timeout, unchanged) {
                drop(poisoned.into_inner());
            }
        }
        None => {
            if let Err(poisoned) = wake.wait_while(guard, unchanged) {
                drop(poisoned.into_inner());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("photo-frame-sync-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn download_error_leaves_the_cache() {
        let dir = scratch("fail");
        fs::write(dir.join("old.jpg"), b"jpeg").unwrap();
        let err =
            update_cache(&dir, &["new".to_string()], |_| Err("offline".to_string())).unwrap_err();
        assert_eq!(err, "offline");
        assert!(dir.join("old.jpg").is_file());
        assert!(!dir.join("new.jpg").exists());
    }

    #[test]
    fn cached_file_skips_download_then_prune_drops_the_rest() {
        let dir = scratch("prune");
        fs::write(dir.join("keep.jpg"), b"jpeg").unwrap();
        fs::write(dir.join("gone.jpg"), b"jpeg").unwrap();
        update_cache(&dir, &["keep".to_string()], |_| {
            panic!("cached id should not download")
        })
        .unwrap();
        assert!(dir.join("keep.jpg").is_file());
        assert!(!dir.join("gone.jpg").exists());
    }

    #[test]
    fn bad_id_does_not_prune() {
        let dir = scratch("bad-id");
        fs::write(dir.join("keep.jpg"), b"jpeg").unwrap();
        let err = update_cache(&dir, &["../x".to_string()], |_| Ok(Vec::new())).unwrap_err();
        assert_eq!(err, "invalid id: ../x");
        assert!(dir.join("keep.jpg").is_file());
    }
}
