mod cache;
mod immich;
mod settings;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use settings::Settings;

const FADE: Duration = Duration::from_millis(400);
const HOLD_SETTINGS: Duration = Duration::from_secs(5);
const MAX_FIELD_CHARS: usize = 512;

/// Playlist, settings generation, and the last sync error.
/// Save increments `generation` so the sync thread can run immediately.
struct Shared {
    playlist: Vec<String>,
    generation: u64,
    last_error: Option<String>,
}

struct PhotoFrame {
    shared: Arc<Mutex<Shared>>,
    wake: Arc<Condvar>,
    images_dir: PathBuf,
    settings: Settings,
    screen: Screen,
    draft: Draft,
    focus: Field,
    shift: bool,
    save_error: Option<String>,
    playlist: Vec<String>,
    index: usize,
    fade: Option<Fade>,
    current: Option<Photo>,
    incoming: Option<Photo>,
    slide_started: Option<Instant>,
    hold_started: Option<Instant>,
}

struct Fade {
    target: usize,
    started: Instant,
}

struct Photo {
    id: String,
    texture: egui::TextureHandle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Slideshow,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Url,
    ApiKey,
    AlbumId,
    SlideSeconds,
    PollMinutes,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Prev,
    Next,
}

#[derive(Clone)]
struct Draft {
    immich_url: String,
    api_key: String,
    album_id: String,
    slide_seconds: String,
    poll_minutes: String,
}

impl Draft {
    fn from_settings(settings: &Settings) -> Self {
        Self {
            immich_url: settings.immich_url.clone(),
            api_key: settings.api_key.clone(),
            album_id: settings.album_id.clone(),
            slide_seconds: settings.slide_seconds.to_string(),
            poll_minutes: settings.poll_minutes.to_string(),
        }
    }
}

#[derive(Clone, Copy)]
enum Key {
    Char(char),
    Backspace,
    Shift,
}

enum Press {
    Focus(Field),
    Save,
    Back,
    Key(Key),
}

/// A tap during a fade snaps to that target, then starts the next fade.
fn tap(len: usize, index: usize, fade_to: Option<usize>, dir: Dir) -> (usize, Option<usize>) {
    if len < 2 {
        return (index, fade_to);
    }
    let index = fade_to.unwrap_or(index);
    let target = match dir {
        Dir::Next => (index + 1) % len,
        Dir::Prev => (index + len - 1) % len,
    };
    (index, Some(target))
}

impl PhotoFrame {
    fn new(ctx: &egui::Context) -> Self {
        tune_style(ctx);
        let (images_dir, dir_error) = match cache::images_dir() {
            Ok(dir) => (dir, None),
            Err(err) => (PathBuf::new(), Some(err)),
        };
        let (settings, missing_file, load_error) = match Settings::load() {
            Ok(Some(settings)) => (settings, false, None),
            Ok(None) => (Settings::default(), true, None),
            Err(err) => (Settings::default(), true, Some(err)),
        };
        // Cached files show immediately. The sync thread replaces this after a fetch.
        let playlist = cached_ids(&images_dir);
        let shared = Arc::new(Mutex::new(Shared {
            playlist: playlist.clone(),
            generation: 0,
            last_error: None,
        }));
        let wake = Arc::new(Condvar::new());
        if dir_error.is_none() {
            spawn_sync(
                ctx.clone(),
                Arc::clone(&shared),
                Arc::clone(&wake),
                images_dir.clone(),
            );
        }
        Self {
            shared,
            wake,
            images_dir,
            draft: Draft::from_settings(&settings),
            settings,
            screen: if missing_file || load_error.is_some() || dir_error.is_some() {
                Screen::Settings
            } else {
                Screen::Slideshow
            },
            focus: Field::Url,
            shift: false,
            save_error: load_error.or(dir_error),
            playlist,
            index: 0,
            fade: None,
            current: None,
            incoming: None,
            slide_started: None,
            hold_started: None,
        }
    }

    fn advance(&mut self, ctx: &egui::Context) {
        self.sync_playlist();
        self.tick(ctx);
        self.ensure_textures(ctx);
        self.request_frame(ctx);
    }

    fn sync_playlist(&mut self) {
        let playlist = lock(&self.shared).playlist.clone();
        if playlist == self.playlist {
            return;
        }
        let current_id = self.playlist.get(self.index).cloned();
        self.playlist = playlist;
        self.index = current_id
            .and_then(|id| self.playlist.iter().position(|item| item == &id))
            .unwrap_or(0);
        self.fade = None;
        self.current = None;
        self.incoming = None;
        self.slide_started = Some(Instant::now());
    }

    fn tick(&mut self, ctx: &egui::Context) {
        if let Some(fade) = &self.fade {
            if fade.started.elapsed() >= FADE {
                self.finish_fade();
            }
            return;
        }
        if self.screen != Screen::Slideshow || self.playlist.len() < 2 {
            return;
        }
        let started = *self.slide_started.get_or_insert_with(Instant::now);
        let limit = Duration::from_secs(self.settings.slide_seconds.max(1));
        if started.elapsed() >= limit {
            self.go(ctx, Dir::Next);
        }
    }

    fn finish_fade(&mut self) {
        if let Some(fade) = self.fade.take() {
            self.index = fade.target;
        }
        if let Some(photo) = self.incoming.take() {
            self.current = Some(photo);
        }
        self.slide_started = Some(Instant::now());
    }

    fn ensure_textures(&mut self, ctx: &egui::Context) {
        if self.playlist.is_empty() {
            self.index = 0;
            self.fade = None;
            self.current = None;
            self.incoming = None;
            return;
        }
        if self.index >= self.playlist.len() {
            self.index = 0;
        }
        let current_id = self.playlist[self.index].clone();
        if self.current.as_ref().map(|photo| photo.id.as_str()) != Some(current_id.as_str()) {
            self.current = self.load(ctx, &current_id);
        }
        if self.fade.is_some() {
            return;
        }
        if self.playlist.len() < 2 {
            self.incoming = None;
            return;
        }
        let next = (self.index + 1) % self.playlist.len();
        let next_id = self.playlist[next].clone();
        if self.incoming.as_ref().map(|photo| photo.id.as_str()) != Some(next_id.as_str()) {
            self.incoming = self.load(ctx, &next_id);
        }
    }

    fn load(&self, ctx: &egui::Context, id: &str) -> Option<Photo> {
        let path = cache::image_path(&self.images_dir, id).ok()?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
            Err(err) => {
                lock(&self.shared).last_error = Some(err.to_string());
                return None;
            }
        };
        let image = match image::load_from_memory(&bytes) {
            Ok(image) => image,
            Err(err) => {
                lock(&self.shared).last_error = Some(format!("decode {id}: {err}"));
                return None;
            }
        };
        let rgba = image.to_rgba8();
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [rgba.width() as usize, rgba.height() as usize],
            rgba.as_raw(),
        );
        Some(Photo {
            id: id.to_string(),
            texture: ctx.load_texture(format!("frame-{id}"), color, egui::TextureOptions::LINEAR),
        })
    }

    fn go(&mut self, ctx: &egui::Context, dir: Dir) {
        let fade_to = self.fade.as_ref().map(|fade| fade.target);
        let snapping = fade_to.is_some();
        let (index, target) = tap(self.playlist.len(), self.index, fade_to, dir);
        let Some(target) = target else {
            return;
        };
        if snapping {
            self.current = self
                .incoming
                .take()
                .or_else(|| self.load(ctx, &self.playlist[index]));
        }
        self.index = index;
        let target_id = self.playlist[target].clone();
        let Some(incoming) = self
            .incoming
            .take()
            .filter(|photo| photo.id == target_id)
            .or_else(|| self.load(ctx, &target_id))
        else {
            self.fade = None;
            self.slide_started = Some(Instant::now());
            return;
        };
        self.incoming = Some(incoming);
        self.fade = Some(Fade {
            target,
            started: Instant::now(),
        });
        self.slide_started = None;
        ctx.request_repaint();
    }

    fn request_frame(&self, ctx: &egui::Context) {
        if self.fade.is_some() {
            ctx.request_repaint();
            return;
        }
        if self.screen != Screen::Slideshow {
            return;
        }
        let mut wait = Duration::from_millis(200);
        if self.playlist.len() >= 2
            && let Some(started) = self.slide_started
        {
            let limit = Duration::from_secs(self.settings.slide_seconds.max(1));
            wait = wait.min(limit.saturating_sub(started.elapsed()));
        }
        if wait.is_zero() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(wait);
        }
    }

    fn slideshow_ui(&mut self, ui: &mut egui::Ui) {
        let screen = ui.max_rect();
        self.paint_slideshow(ui, screen);
        // Click and drag so a still finger keeps the press through egui's click window.
        let press = ui.interact(
            screen,
            ui.scope_id().with("slide"),
            egui::Sense::click_and_drag(),
        );
        if press.is_pointer_button_down_on() {
            self.hold_started.get_or_insert_with(Instant::now);
        }
        if self
            .hold_started
            .is_some_and(|started| started.elapsed() >= HOLD_SETTINGS)
        {
            self.hold_started = None;
            self.open_settings();
            return;
        }
        if press.is_pointer_button_down_on() {
            if let Some(started) = self.hold_started {
                let left = HOLD_SETTINGS.saturating_sub(started.elapsed());
                ui.ctx().request_repaint_after(left);
            }
        } else if press.clicked() {
            self.hold_started = None;
            if let Some(pos) = press.interact_pointer_pos() {
                let dir = if pos.x < screen.center().x {
                    Dir::Prev
                } else {
                    Dir::Next
                };
                self.go(ui.ctx(), dir);
            }
        } else {
            self.hold_started = None;
        }
    }

    fn paint_slideshow(&self, ui: &egui::Ui, screen: egui::Rect) {
        if let Some(fade) = &self.fade {
            let t = (fade.started.elapsed().as_secs_f32() / FADE.as_secs_f32()).clamp(0.0, 1.0);
            // Outgoing stays opaque. Src-over keeps the rest while the next photo fades in.
            paint_photo(ui, screen, self.current.as_ref(), 255);
            let alpha = (t * 255.0).round() as u8;
            paint_photo(ui, screen, self.incoming.as_ref(), alpha);
        } else {
            paint_photo(ui, screen, self.current.as_ref(), 255);
        }
        if self.playlist.is_empty() {
            ui.painter().text(
                screen.center(),
                egui::Align2::CENTER_CENTER,
                "No photos",
                egui::FontId::proportional(32.0),
                egui::Color32::WHITE,
            );
        }
    }

    fn open_settings(&mut self) {
        self.draft = Draft::from_settings(&self.settings);
        self.focus = Field::Url;
        self.shift = false;
        self.save_error = None;
        self.screen = Screen::Settings;
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        self.apply_keys(ui.ctx());
        let poll_error = lock(&self.shared).last_error.clone();
        let save_error = self.save_error.clone();
        let focus = self.focus;
        let shift = self.shift;
        let draft = self.draft.clone();
        let numeric = matches!(focus, Field::SlideSeconds | Field::PollMinutes);
        let mut press = None;

        egui::Frame::NONE
            .inner_margin(12.0)
            .fill(egui::Color32::BLACK)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Settings").size(28.0));
                for (field, label, value, password) in [
                    (Field::Url, "Immich URL", draft.immich_url.as_str(), false),
                    (Field::ApiKey, "API key", draft.api_key.as_str(), true),
                    (Field::AlbumId, "Album id", draft.album_id.as_str(), false),
                    (
                        Field::SlideSeconds,
                        "Slide seconds",
                        draft.slide_seconds.as_str(),
                        false,
                    ),
                    (
                        Field::PollMinutes,
                        "Poll minutes",
                        draft.poll_minutes.as_str(),
                        false,
                    ),
                ] {
                    if field_row(ui, label, value, password, focus == field) {
                        press = Some(Press::Focus(field));
                    }
                }
                if let Some(err) = save_error.as_deref().or(poll_error.as_deref()) {
                    ui.colored_label(egui::Color32::from_rgb(220, 90, 90), err);
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_sized([140.0, 44.0], egui::Button::new("Save"))
                        .clicked()
                    {
                        press = Some(Press::Save);
                    }
                    if ui
                        .add_sized([140.0, 44.0], egui::Button::new("Back"))
                        .clicked()
                    {
                        press = Some(Press::Back);
                    }
                });
                ui.add_space(8.0);
                if let Some(key) = key_grid(ui, numeric, shift) {
                    press = Some(Press::Key(key));
                }
            });

        if let Some(press) = press {
            self.apply_press(press);
        }
    }

    fn apply_press(&mut self, press: Press) {
        match press {
            Press::Focus(field) => {
                self.focus = field;
                if matches!(field, Field::SlideSeconds | Field::PollMinutes) {
                    self.shift = false;
                }
            }
            Press::Save => self.save_settings(),
            Press::Back => {
                self.save_error = None;
                self.screen = Screen::Slideshow;
                self.slide_started = Some(Instant::now());
            }
            Press::Key(Key::Char(ch)) => self.insert_char(ch),
            Press::Key(Key::Backspace) => {
                self.focused_text().pop();
            }
            Press::Key(Key::Shift) => self.shift = !self.shift,
        }
    }

    fn save_settings(&mut self) {
        let slide_seconds = match parse_count(&self.draft.slide_seconds, "slide seconds") {
            Ok(value) => value,
            Err(err) => {
                self.save_error = Some(err);
                return;
            }
        };
        let poll_minutes = match parse_count(&self.draft.poll_minutes, "poll minutes") {
            Ok(value) => value,
            Err(err) => {
                self.save_error = Some(err);
                return;
            }
        };
        let settings = Settings {
            immich_url: self.draft.immich_url.trim().to_string(),
            api_key: self.draft.api_key.trim().to_string(),
            album_id: self.draft.album_id.trim().to_string(),
            slide_seconds,
            poll_minutes,
        };
        if let Err(err) = settings.save() {
            self.save_error = Some(err);
            return;
        }
        self.draft = Draft::from_settings(&settings);
        self.settings = settings;
        {
            let mut shared = lock(&self.shared);
            shared.generation = shared.generation.wrapping_add(1);
        }
        self.wake.notify_one();
        self.save_error = None;
        self.screen = Screen::Slideshow;
        self.slide_started = Some(Instant::now());
    }

    fn apply_keys(&mut self, ctx: &egui::Context) {
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    for ch in text.chars() {
                        self.insert_char(ch);
                    }
                }
                egui::Event::Key {
                    key: egui::Key::Backspace,
                    pressed: true,
                    ..
                } => {
                    self.focused_text().pop();
                }
                _ => {}
            }
        }
    }

    fn insert_char(&mut self, ch: char) {
        if !char_allowed(self.focus, ch) {
            return;
        }
        let text = self.focused_text();
        if text.chars().count() >= MAX_FIELD_CHARS {
            return;
        }
        text.push(ch);
    }

    fn focused_text(&mut self) -> &mut String {
        match self.focus {
            Field::Url => &mut self.draft.immich_url,
            Field::ApiKey => &mut self.draft.api_key,
            Field::AlbumId => &mut self.draft.album_id,
            Field::SlideSeconds => &mut self.draft.slide_seconds,
            Field::PollMinutes => &mut self.draft.poll_minutes,
        }
    }
}

fn char_allowed(field: Field, ch: char) -> bool {
    match field {
        Field::SlideSeconds | Field::PollMinutes => ch.is_ascii_digit(),
        Field::Url | Field::ApiKey | Field::AlbumId => {
            ch.is_ascii_alphanumeric() || matches!(ch, ':' | '/' | '.' | '_' | '-')
        }
    }
}

fn parse_count(text: &str, label: &str) -> Result<u64, String> {
    let value = text
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{label} must be a whole number"))?;
    if value == 0 {
        Err(format!("{label} must be at least 1"))
    } else {
        Ok(value)
    }
}

fn field_row(ui: &mut egui::Ui, label: &str, value: &str, password: bool, focused: bool) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([170.0, 40.0], egui::Label::new(label));
        ui.add_sized(
            [ui.available_width(), 40.0],
            egui::Button::new(field_preview(value, password)).selected(focused),
        )
        .clicked()
    })
    .inner
}

fn field_preview(value: &str, password: bool) -> String {
    if value.is_empty() {
        return " ".to_string();
    }
    if password {
        return "*".repeat(value.chars().count().min(24));
    }
    let count = value.chars().count();
    if count <= 42 {
        return value.to_string();
    }
    let tail: String = value.chars().skip(count - 41).collect();
    format!("...{tail}")
}

fn key_grid(ui: &mut egui::Ui, numeric: bool, shift: bool) -> Option<Key> {
    let rows = if numeric {
        vec![
            chars("123"),
            chars("456"),
            chars("789"),
            vec![Key::Char('0'), Key::Backspace],
        ]
    } else {
        vec![
            chars("1234567890"),
            chars("qwertyuiop"),
            chars("asdfghjkl"),
            chars("zxcvbnm-_"),
            vec![
                Key::Char(':'),
                Key::Char('/'),
                Key::Char('.'),
                Key::Shift,
                Key::Backspace,
            ],
        ]
    };
    let gap_y = ui.spacing().item_spacing.y;
    let height = ((ui.available_height() - gap_y * (rows.len().saturating_sub(1) as f32))
        / rows.len() as f32)
        .clamp(36.0, 72.0);
    let mut pressed = None;
    for row in &rows {
        ui.horizontal(|ui| {
            let count = row.len() as f32;
            let gap_x = ui.spacing().item_spacing.x;
            let width = ((ui.available_width() - gap_x * (count - 1.0)) / count).max(1.0);
            for key in row {
                let label = match key {
                    Key::Char(ch) if shift && ch.is_ascii_alphabetic() => {
                        ch.to_ascii_uppercase().to_string()
                    }
                    Key::Char(ch) => ch.to_string(),
                    Key::Backspace => "Del".to_string(),
                    Key::Shift => "Aa".to_string(),
                };
                if ui
                    .add_sized(
                        [width, height],
                        egui::Button::new(label).selected(matches!(key, Key::Shift) && shift),
                    )
                    .clicked()
                {
                    pressed = Some(match key {
                        Key::Char(ch) if shift && ch.is_ascii_alphabetic() => {
                            Key::Char(ch.to_ascii_uppercase())
                        }
                        other => *other,
                    });
                }
            }
        });
    }
    pressed
}

fn chars(text: &str) -> Vec<Key> {
    text.chars().map(Key::Char).collect()
}

fn paint_photo(ui: &egui::Ui, rect: egui::Rect, photo: Option<&Photo>, alpha: u8) {
    let Some(photo) = photo else {
        return;
    };
    if alpha == 0 {
        return;
    }
    egui::Image::new(&photo.texture)
        .tint(egui::Color32::from_white_alpha(alpha))
        .paint_at(ui, rect);
}

fn tune_style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.all_styles_mut(|style| {
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(20.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(20.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(28.0));
        style.spacing.button_padding = egui::vec2(10.0, 8.0);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    });
}

fn cached_ids(dir: &Path) -> Vec<String> {
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
        if cache::image_path(dir, id).is_err() {
            continue;
        }
        ids.push(id.to_string());
    }
    ids.sort();
    ids
}

fn spawn_sync(
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
    let client = immich::Client::new(&settings.immich_url, &settings.api_key)?;
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
        let path = cache::image_path(dir, id)?;
        if path.is_file() {
            continue;
        }
        let bytes = download(id)?;
        cache::store_preview(dir, id, &bytes)?;
    }
    let keep: HashSet<String> = ids.iter().cloned().collect();
    cache::prune(dir, &keep)
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

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|err| err.into_inner())
}

impl eframe::App for PhotoFrame {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.advance(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let frame = egui::Frame::NONE.fill(egui::Color32::BLACK);
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            if self.screen == Screen::Slideshow {
                self.slideshow_ui(ui);
            } else {
                self.settings_ui(ui);
            }
        });
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 1.0]
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Photo Frame")
            .with_inner_size([1024.0, 600.0])
            .with_fullscreen(true),
        renderer: eframe::Renderer::Glow,
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "photo-frame",
        native_options,
        Box::new(|cc| Ok(Box::new(PhotoFrame::new(&cc.egui_ctx)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_next_fades_to_the_following_photo() {
        let (index, fade_to) = tap(3, 0, None, Dir::Next);
        assert_eq!((index, fade_to), (0, Some(1)));
        assert_eq!(fade_to.unwrap_or(index), 1);
    }

    #[test]
    fn tap_during_fade_snaps_then_fades_again() {
        let (index, fade_to) = tap(4, 0, None, Dir::Next);
        let (index, fade_to) = tap(4, index, fade_to, Dir::Next);
        assert_eq!((index, fade_to), (1, Some(2)));
        assert_eq!(tap(4, index, fade_to, Dir::Prev), (2, Some(1)));
    }

    #[test]
    fn prev_wraps_and_single_photo_ignores_taps() {
        assert_eq!(tap(3, 0, None, Dir::Prev).1, Some(2));
        assert_eq!(tap(1, 0, None, Dir::Next), (0, None));
        assert_eq!(tap(0, 0, None, Dir::Prev), (0, None));
    }

    #[test]
    fn keyboard_chars_match_the_field() {
        assert!(char_allowed(Field::Url, 'a'));
        assert!(char_allowed(Field::Url, 'Z'));
        assert!(char_allowed(Field::ApiKey, '7'));
        for ch in [':', '/', '.', '_', '-'] {
            assert!(char_allowed(Field::AlbumId, ch), "{ch}");
        }
        assert!(!char_allowed(Field::Url, ' '));
        assert!(!char_allowed(Field::Url, '?'));
        assert!(char_allowed(Field::SlideSeconds, '3'));
        assert!(!char_allowed(Field::PollMinutes, 'a'));
    }

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
