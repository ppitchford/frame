// Quick Access Overlay: a floating preview of the capture just taken, with
// save / copy / annotate / discard actions.

use eframe::egui;
use image::RgbaImage;

use crate::output;

/// Matches the `windowrule=isfloating:1,appid:^frame$` in the mango config. The
/// compositor tiles the overlay into the scroller without it.
const APP_ID: &str = "frame";

/// Largest preview drawn, in logical points. The display is 1440×960 logical
/// (2880×1920 at scale 2), so this holds a fullscreen grab to about half the
/// screen instead of filling it.
const MAX_PREVIEW_W: f32 = 720.0;
const MAX_PREVIEW_H: f32 = 480.0;

/// Room under the preview for the action row and its status line.
const ACTION_BAR_H: f32 = 64.0;

/// Enough width for four buttons, however narrow the capture was.
const MIN_WINDOW_W: f32 = 340.0;

/// Open the overlay on `img` and block until it closes.
pub fn show(img: RgbaImage, scale: i32) -> Result<(), String> {
    let preview = preview_size(&img, scale);
    let window = egui::vec2(preview.x.max(MIN_WINDOW_W), preview.y + ACTION_BAR_H);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_inner_size(window),
        ..Default::default()
    };

    eframe::run_native(
        "frame",
        options,
        Box::new(move |cc| Ok(Box::new(Qao::new(cc, img, preview)))),
    )
    .map_err(|e| format!("overlay failed: {e}"))
}

/// The grab is in physical pixels; egui lays out in logical points, and the
/// compositor reports `scale` pixels per point. Dividing draws the preview 1:1
/// against the captured pixels, then it shrinks to fit if it would be oversized.
fn preview_size(img: &RgbaImage, scale: i32) -> egui::Vec2 {
    let s = scale.max(1) as f32;
    let w = img.width() as f32 / s;
    let h = img.height() as f32 / s;
    let shrink = (MAX_PREVIEW_W / w).min(MAX_PREVIEW_H / h).min(1.0);
    egui::vec2(w * shrink, h * shrink)
}

struct Qao {
    img: RgbaImage,
    texture: egui::TextureHandle,
    preview: egui::Vec2,
    saved: bool,
    copied: bool,
    /// Outcome of the last action: `Ok` reports it, `Err` explains the failure.
    /// Both are shown; a silent failure here is what the clipboard bug looked like.
    status: Option<Result<String, String>>,
}

impl Qao {
    fn new(cc: &eframe::CreationContext<'_>, img: RgbaImage, preview: egui::Vec2) -> Self {
        // `from_rgba_unmultiplied` asserts the buffer matches the stated size, so
        // the dimensions must come from the image itself.
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [img.width() as usize, img.height() as usize],
            img.as_raw(),
        );
        let texture = cc
            .egui_ctx
            .load_texture("capture", color, egui::TextureOptions::LINEAR);

        Self {
            img,
            texture,
            preview,
            saved: false,
            copied: false,
            status: None,
        }
    }

    fn save(&mut self) {
        self.status = Some(match output::save(&self.img) {
            Ok(path) => {
                self.saved = true;
                Ok(format!("Saved {}", path.display()))
            }
            Err(e) => Err(e),
        });
    }

    fn copy(&mut self) {
        self.status = Some(match output::copy(&self.img) {
            Ok(()) => {
                self.copied = true;
                Ok("Copied to clipboard".to_string())
            }
            Err(e) => Err(e),
        });
    }
}

impl eframe::App for Qao {
    // egui 0.35 hands the app a `Ui` directly; the panel is the framework's job.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        ui.image((self.texture.id(), self.preview));
        ui.separator();

        ui.horizontal(|ui| {
            // Save and Copy stay enabled after they succeed: the tick reports what
            // happened, but re-copying is the only recovery if another client takes
            // the clipboard, and a second save just writes a new timestamped file.
            if ui.button(label("Save", self.saved)).clicked() {
                self.save();
            }
            if ui.button(label("Copy", self.copied)).clicked() {
                self.copy();
            }
            ui.add_enabled(false, egui::Button::new("Annotate"))
                .on_disabled_hover_text("No annotation editor yet.");
            if ui.button("Discard").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });

        match &self.status {
            Some(Ok(msg)) => {
                ui.colored_label(egui::Color32::from_rgb(0x9c, 0xcf, 0xd8), msg);
            }
            Some(Err(e)) => {
                ui.colored_label(egui::Color32::from_rgb(0xeb, 0x6f, 0x92), e);
            }
            None => {}
        }
    }
}

fn label(action: &str, done: bool) -> String {
    if done {
        format!("{action} ✓")
    } else {
        action.to_string()
    }
}
