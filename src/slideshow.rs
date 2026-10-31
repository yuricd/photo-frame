use std::fs;
use std::io;
use std::time::{Duration, Instant};

use eframe::egui;

use super::{PhotoFrame, Screen, lock};

const FADE: Duration = Duration::from_millis(400);
const HOLD_SETTINGS: Duration = Duration::from_secs(3);

pub(crate) struct Fade {
    target: usize,
    started: Instant,
}

pub(crate) struct Photo {
    id: String,
    texture: egui::TextureHandle,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Prev,
    Next,
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
    pub(crate) fn advance(&mut self, ctx: &egui::Context) {
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
        let path = crate::cache::image_path(&self.images_dir, id).ok()?;
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

    pub(crate) fn slideshow_ui(&mut self, ui: &mut egui::Ui) {
        let screen = ui.max_rect();
        self.paint_slideshow(ui, screen);
        let press = ui.interact(
            screen,
            ui.scope_id().with("slide"),
            egui::Sense::click_and_drag(),
        );
        let still_down = ui.input(|input| input.pointer.primary_down());
        let holding =
            press.is_pointer_button_down_on() || (self.hold_started.is_some() && still_down);
        if holding {
            self.hold_started.get_or_insert_with(Instant::now);
        }
        if still_down
            && self
                .hold_started
                .is_some_and(|started| started.elapsed() >= HOLD_SETTINGS)
        {
            self.hold_started = None;
            self.open_settings();
            return;
        }
        if holding {
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
}
