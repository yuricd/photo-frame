use std::sync::Arc;
use std::thread;
use std::time::Instant;

use eframe::egui;

use crate::settings::Settings;

use super::{PhotoFrame, Screen, lock};

const MAX_FIELD_CHARS: usize = 512;

#[derive(Clone)]
pub(crate) struct Draft {
    immich_url: String,
    api_key: String,
    album_id: String,
    slide_seconds: String,
    poll_minutes: String,
}

impl Draft {
    pub(crate) fn from_settings(settings: &Settings) -> Self {
        Self {
            immich_url: settings.immich_url.clone(),
            api_key: settings.api_key.clone(),
            album_id: settings.album_id.clone(),
            slide_seconds: settings.slide_seconds.to_string(),
            poll_minutes: settings.poll_minutes.to_string(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Url,
    ApiKey,
    AlbumId,
    SlideSeconds,
    PollMinutes,
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
    CheckUpdates,
    Key(Key),
}

pub(crate) struct Update {
    busy: bool,
    text: Option<String>,
    error: bool,
}

impl Update {
    pub(crate) fn idle() -> Self {
        Self {
            busy: false,
            text: None,
            error: false,
        }
    }
}

impl PhotoFrame {
    pub(crate) fn open_settings(&mut self) {
        self.draft = Draft::from_settings(&self.settings);
        self.focus = Field::Url;
        self.shift = false;
        self.save_error = None;
        self.screen = Screen::Settings;
    }

    pub(crate) fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.apply_keys(&ctx);
        let poll_error = lock(&self.shared).last_error.clone();
        let save_error = self.save_error.clone();
        let (update_text, update_error, update_busy) = {
            let update = lock(&self.update);
            (update.text.clone(), update.error, update.busy)
        };
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
                if ui
                    .add_enabled(
                        !update_busy,
                        egui::Button::new("Check for updates").min_size(egui::vec2(0.0, 44.0)),
                    )
                    .clicked()
                {
                    press = Some(Press::CheckUpdates);
                }
                if update_error {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 90, 90),
                        update_text.as_deref().unwrap_or(" "),
                    );
                } else {
                    ui.label(update_text.as_deref().unwrap_or(" "));
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
            self.apply_press(&ctx, press);
        }
    }

    fn apply_press(&mut self, ctx: &egui::Context, press: Press) {
        match press {
            Press::Focus(field) => {
                self.focus = field;
                if matches!(field, Field::SlideSeconds | Field::PollMinutes) {
                    self.shift = false;
                }
            }
            Press::Save => self.save_settings(),
            Press::CheckUpdates => self.start_update(ctx),
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

    fn start_update(&mut self, ctx: &egui::Context) {
        {
            let mut update = lock(&self.update);
            if update.busy {
                return;
            }
            update.busy = true;
            update.text = Some("Checking…".to_string());
            update.error = false;
        }
        let update = Arc::clone(&self.update);
        let ctx = ctx.clone();
        ctx.request_repaint();
        if let Err(err) = thread::Builder::new()
            .name("photo-frame-update".to_string())
            .spawn(move || {
                let (text, error) = match crate::update::apply() {
                    Ok(()) => ("Up to date".to_string(), false),
                    Err(err) => (err, true),
                };
                let mut update = lock(&update);
                update.busy = false;
                update.text = Some(text);
                update.error = error;
                ctx.request_repaint();
            })
        {
            let mut update = lock(&self.update);
            update.busy = false;
            update.text = Some(format!("update thread: {err}"));
            update.error = true;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
