mod cache;
mod immich;
mod settings;
mod settings_ui;
mod slideshow;
mod sync;
mod update;

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

use eframe::egui;
use settings::Settings;
use settings_ui::{Draft, Field, Update};
use slideshow::{Fade, Photo};
use sync::Shared;

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
    update: Arc<Mutex<Update>>,
    playlist: Vec<String>,
    index: usize,
    fade: Option<Fade>,
    current: Option<Photo>,
    incoming: Option<Photo>,
    slide_started: Option<Instant>,
    hold_started: Option<Instant>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Slideshow,
    Settings,
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
        let playlist = sync::cached_ids(&images_dir);
        let shared = Arc::new(Mutex::new(Shared {
            playlist: playlist.clone(),
            generation: 0,
            last_error: None,
        }));
        let wake = Arc::new(Condvar::new());
        if dir_error.is_none() {
            sync::spawn_sync(
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
            update: Arc::new(Mutex::new(Update::idle())),
            playlist,
            index: 0,
            fade: None,
            current: None,
            incoming: None,
            slide_started: None,
            hold_started: None,
        }
    }
}

fn lock<T>(shared: &Mutex<T>) -> MutexGuard<'_, T> {
    shared.lock().unwrap_or_else(|err| err.into_inner())
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
        // Cage centers a pointer on the screen. Clear it after widgets so none of them restore it.
        ui.ctx().set_cursor_icon(egui::CursorIcon::None);
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
