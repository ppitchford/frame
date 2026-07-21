// Quick Access Overlay: a floating preview of the capture just taken, with
// save / copy / annotate / discard actions.
//
// One `eframe` window, two modes. `Preview` is the landing screen — the capture
// plus the action row. `Annotate` swaps the same window into `Edit`, the
// annotation editor, reusing the loaded image and the save/copy paths rather
// than launching a second window.

use eframe::egui;
use image::RgbaImage;

use crate::editor::{self, Document};
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

/// Extra height the editor's two toolbar rows add above the canvas.
const TOOLBAR_H: f32 = 76.0;

/// Enough width for the editor's tool row — seven tools, the fill checkbox and
/// undo/redo — however narrow the capture was. The action row is far shorter.
/// The row wraps rather than overflowing if this is ever outgrown.
const MIN_WINDOW_W: f32 = 580.0;

/// The drag outline for a destructive op. Fixed and neutral (Rosé Pine text):
/// the palette does not apply to an op that adds no ink, and previewing the
/// selected colour would imply that it did.
const DESTRUCTIVE_PREVIEW: egui::Color32 = egui::Color32::from_rgb(0xe0, 0xde, 0xf4);

/// egui family name for the embedded annotation font. Registering it under its
/// own name — rather than replacing the default proportional family — sets the
/// canvas preview in the face the commit will use while leaving the toolbar and
/// status line in egui's own font.
const ANNOTATION_FONT: &str = "annotation";

/// Register the annotation font with egui under `ANNOTATION_FONT`.
fn install_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        ANNOTATION_FONT.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(editor::FONT)),
    );
    fonts.families.insert(
        egui::FontFamily::Name(ANNOTATION_FONT.into()),
        vec![ANNOTATION_FONT.to_owned()],
    );
    ctx.set_fonts(fonts);
}

/// A text entry in progress: where it will land, and what has been typed so far.
/// Present only between the click that opens it and the key that commits or
/// cancels it.
struct TextEntry {
    pos: editor::Point,
    buffer: String,
}

/// The fixed annotation palette (Rosé Pine): love, gold, foam, iris, near-white
/// text, near-black base. No free colour picker — this is the whole choice.
const PALETTE: [editor::Rgba; 6] = [
    [0xeb, 0x6f, 0x92, 0xff], // love
    [0xf6, 0xc1, 0x77, 0xff], // gold
    [0x9c, 0xcf, 0xd8, 0xff], // foam
    [0xc4, 0xa7, 0xe7, 0xff], // iris
    [0xe0, 0xde, 0xf4, 0xff], // text
    [0x19, 0x17, 0x24, 0xff], // base
];

/// Default colour (love, `PALETTE[0]`) and stroke width for a fresh editor, plus
/// the width bounds the toolbar's numeric field clamps to.
const DEFAULT_COLOR: editor::Rgba = PALETTE[0];
const DEFAULT_WIDTH: f32 = 4.0;
const MIN_WIDTH: f32 = 1.0;
const MAX_WIDTH: f32 = 100.0;

/// Open the overlay on `img` and block until it closes.
pub fn show(img: RgbaImage, scale: i32) -> Result<(), String> {
    let preview = preview_size(&img, scale);
    let window = egui::vec2(preview.x.max(MIN_WINDOW_W), preview.y + ACTION_BAR_H + TOOLBAR_H);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_inner_size(window),
        ..Default::default()
    };

    eframe::run_native(
        "frame",
        options,
        Box::new(move |cc| Ok(Box::new(Qao::new(cc, img, scale, preview)))),
    )
    .map_err(|e| format!("overlay failed: {e}"))
}

/// The preview is drawn at `preview` logical points, so the texture behind it
/// never needs to exceed the preview's physical size — and a full-resolution
/// grab (2880×1920 fullscreen) overruns the GPU's maximum texture side. Resize a
/// copy down to `preview × scale` for the texture, leaving the full-resolution
/// image untouched for save and copy. Region grabs already fit, so this returns
/// them unchanged.
fn downscale_for_texture(img: &RgbaImage, preview: egui::Vec2, scale: i32) -> RgbaImage {
    let s = scale.max(1) as f32;
    let (tw, th) = ((preview.x * s).round() as u32, (preview.y * s).round() as u32);
    if tw >= img.width() && th >= img.height() {
        return img.clone();
    }
    image::imageops::resize(
        img,
        tw.max(1),
        th.max(1),
        image::imageops::FilterType::Triangle,
    )
}

/// Load `img` as a texture sized to the preview. Shared by the initial preview
/// and every editor re-render.
fn make_texture(
    ctx: &egui::Context,
    name: &str,
    img: &RgbaImage,
    preview: egui::Vec2,
    scale: i32,
) -> egui::TextureHandle {
    let src = downscale_for_texture(img, preview, scale);
    let color = egui::ColorImage::from_rgba_unmultiplied(
        [src.width() as usize, src.height() as usize],
        src.as_raw(),
    );
    ctx.load_texture(name, color, egui::TextureOptions::LINEAR)
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
    /// The original, unannotated capture — full resolution, for preview save/copy.
    img: RgbaImage,
    scale: i32,
    preview: egui::Vec2,
    saved: bool,
    copied: bool,
    /// Outcome of the last action: `Ok` reports it, `Err` explains the failure.
    /// Both are shown; a silent failure here is what the clipboard bug looked like.
    status: Option<Result<String, String>>,
    mode: Mode,
}

enum Mode {
    Preview { texture: egui::TextureHandle },
    Edit(EditorState),
}

/// Live state of the annotation editor: the document plus the current tool
/// settings and the flattened texture on screen.
struct EditorState {
    doc: Document,
    tool: editor::Tool,
    color: editor::Rgba,
    width: f32,
    size: f32,
    filled: bool,
    /// The open text entry, if any. While this is `Some`, the editor is in a
    /// typing mode that takes `Escape` and `Ctrl+Z` away from their usual jobs.
    typing: Option<TextEntry>,
    /// The in-progress drag, sampled in base-image pixels, present only mid-drag.
    /// A drag is always a point sequence — the pencil consumes all of it, the
    /// two-point tools read only its ends.
    drag: Option<Vec<editor::Point>>,
    /// The flattened render, downscaled for the GPU. Rebuilt when `dirty`.
    texture: egui::TextureHandle,
    dirty: bool,
}

impl Qao {
    fn new(
        cc: &eframe::CreationContext<'_>,
        img: RgbaImage,
        scale: i32,
        preview: egui::Vec2,
    ) -> Self {
        install_font(&cc.egui_ctx);
        let texture = make_texture(&cc.egui_ctx, "capture", &img, preview, scale);
        Self {
            img,
            scale,
            preview,
            saved: false,
            copied: false,
            status: None,
            mode: Mode::Preview { texture },
        }
    }

    /// Enter the editor: a fresh document over a copy of the capture, rendered
    /// once (an empty document renders to the base image).
    fn enter_editor(&mut self, ctx: &egui::Context) {
        let doc = Document::new(self.img.clone());
        let texture = make_texture(ctx, "editor", &editor::render(&doc), self.preview, self.scale);
        self.mode = Mode::Edit(EditorState {
            doc,
            tool: editor::Tool::Arrow,
            color: DEFAULT_COLOR,
            width: DEFAULT_WIDTH,
            size: editor::DEFAULT_TEXT_SIZE,
            filled: false,
            typing: None,
            drag: None,
            texture,
            dirty: false,
        });
        // Action state carries no meaning across the mode switch.
        self.saved = false;
        self.copied = false;
        self.status = None;
    }

    /// The image the action row acts on: the raw capture in Preview, the
    /// flattened document in Edit. Owned so the borrow on `self.mode` ends before
    /// the caller writes `self.status`.
    fn flattened(&self) -> RgbaImage {
        match &self.mode {
            Mode::Preview { .. } => self.img.clone(),
            Mode::Edit(ed) => editor::render(&ed.doc),
        }
    }

    fn save(&mut self) {
        let img = self.flattened();
        self.status = Some(match output::save(&img) {
            Ok(path) => {
                self.saved = true;
                Ok(format!("Saved {}", path.display()))
            }
            Err(e) => Err(e),
        });
    }

    fn copy(&mut self) {
        let img = self.flattened();
        self.status = Some(match output::copy(&img) {
            Ok(()) => {
                self.copied = true;
                Ok("Copied to clipboard".to_string())
            }
            Err(e) => Err(e),
        });
    }

    fn preview_ui(&mut self, ui: &mut egui::Ui) {
        // Copy the id out so the `&self.mode` borrow ends before the Annotate
        // handler reassigns `self.mode`.
        let Mode::Preview { texture } = &self.mode else {
            return;
        };
        let tex_id = texture.id();

        ui.image((tex_id, self.preview));
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
            if ui.button("Annotate").clicked() {
                self.enter_editor(ui.ctx());
            }
            if ui.button("Discard").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });

        self.status_line(ui);
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let preview = self.preview;
        let scale = self.scale;
        // Read the action ticks out before borrowing `self.mode`.
        let (saved, copied) = (self.saved, self.copied);

        // The action row can't call `self.save`/`copy` while `ed` borrows
        // `self.mode`, so it records intent and we act once the borrow ends.
        let mut do_save = false;
        let mut do_copy = false;
        let mut do_close = false;

        if let Mode::Edit(ed) = &mut self.mode {
            toolbar(ui, ed);
            ui.separator();

            // The image is a click-and-drag canvas: press anchors, drag previews,
            // release commits an op.
            let (rect, response) =
                ui.allocate_exact_size(preview, egui::Sense::click_and_drag());
            let painter = ui.painter_at(rect);
            painter.image(
                ed.texture.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            let (img_w, img_h) = (ed.doc.base.width() as f32, ed.doc.base.height() as f32);
            // Screen points ↔ base-image pixels. The texture fills `rect`
            // regardless of its own resolution, so the rect↔image ratio is the
            // whole mapping — scale never enters here.
            let to_image = |p: egui::Pos2| {
                let fx = ((p.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                let fy = ((p.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
                editor::Point::new(fx * img_w, fy * img_h)
            };
            let to_screen = |pt: editor::Point| {
                egui::pos2(
                    rect.min.x + pt.x / img_w * rect.width(),
                    rect.min.y + pt.y / img_h * rect.height(),
                )
            };

            // A click with the text tool opens an entry where it landed. If one
            // is already open, it commits first, so clicking around the canvas
            // places a series of labels rather than losing each previous one.
            if ed.tool == editor::Tool::Text
                && response.clicked()
                && let Some(p) = response.interact_pointer_pos()
            {
                commit_text(ed);
                ed.typing = Some(TextEntry {
                    pos: to_image(p),
                    buffer: String::new(),
                });
            }
            if ed.typing.is_some() {
                handle_typing(ui, ed);
            }

            if response.drag_started() {
                ed.drag = Some(Vec::new());
            }

            // Sample every frame the drag is live. The pencil needs the whole
            // path; the two-point tools only ever read its ends, for which the
            // newest sample is the cursor.
            if let (Some(path), Some(cursor)) = (ed.drag.as_mut(), response.interact_pointer_pos())
            {
                path.push(to_image(cursor));
            }

            // Live preview via the painter — feedback only. The committed pixels
            // still come from `render()` walking the op list in order.
            if let Some(path) = ed.drag.as_ref() {
                let screen: Vec<egui::Pos2> = path.iter().copied().map(to_screen).collect();
                let disp_width = ed.width * rect.width() / img_w;
                draw_preview(&painter, ed.tool, &screen, ed.color, disp_width, ed.filled);
            }

            // The open entry, drawn in the same face the commit will use. egui
            // does its own layout while the commit uses skrifa advances, so
            // spacing differs slightly — the same class of accepted mismatch as
            // the pencil settling onto its smoothed curve.
            if let Some(entry) = ed.typing.as_ref() {
                let anchor = to_screen(entry.pos);
                let disp_size = ed.size * rect.width() / img_w;
                let galley = painter.layout_no_wrap(
                    entry.buffer.clone(),
                    egui::FontId::new(disp_size, egui::FontFamily::Name(ANNOTATION_FONT.into())),
                    color32(ed.color),
                );
                let height = galley.size().y.max(disp_size);
                let caret_x = anchor.x + galley.size().x;
                painter.galley(anchor, galley, color32(ed.color));
                // Solid, not blinking: a blink would need repaint scheduling for
                // no real gain.
                painter.line_segment(
                    [
                        egui::pos2(caret_x, anchor.y),
                        egui::pos2(caret_x, anchor.y + height),
                    ],
                    egui::Stroke::new(1.5, DESTRUCTIVE_PREVIEW),
                );
            }

            // `from_drag` yields `None` for a drag with no samples at all, which
            // describes no geometry and should not enter the history.
            if response.drag_stopped()
                && let Some(path) = ed.drag.take()
                && let Some(op) =
                    editor::Op::from_drag(ed.tool, &path, ed.color, ed.width, ed.filled)
            {
                ed.doc.push(op);
                ed.dirty = true;
            }

            // Re-render only on an edit, never per frame.
            if ed.dirty {
                let flat = editor::render(&ed.doc);
                ed.texture = make_texture(ui.ctx(), "editor", &flat, preview, scale);
                ed.dirty = false;
            }

            ui.separator();
            ui.horizontal(|ui| {
                // Save and Copy act on the flattened render, with the same
                // non-terminal semantics as Preview. No round-trip back to
                // Preview this milestone — Discard closes.
                if ui.button(label("Save", saved)).clicked() {
                    do_save = true;
                }
                if ui.button(label("Copy", copied)).clicked() {
                    do_copy = true;
                }
                if ui.button("Discard").clicked() {
                    do_close = true;
                }
            });
        }

        if do_save {
            self.save();
        }
        if do_copy {
            self.copy();
        }
        if do_close {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        self.status_line(ui);
    }

    fn status_line(&self, ui: &mut egui::Ui) {
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

impl eframe::App for Qao {
    // egui 0.35 hands the app a `Ui` directly; the panel is the framework's job.
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Escape closes the overlay — except while a text entry is open, where
        // it cancels that entry instead. The typing state takes the key first;
        // otherwise a mistyped label would close the window and lose the whole
        // annotation.
        let typing = matches!(&self.mode, Mode::Edit(ed) if ed.typing.is_some());
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        match self.mode {
            Mode::Preview { .. } => self.preview_ui(ui),
            Mode::Edit(_) => self.editor_ui(ui),
        }
    }
}

/// The editor's two toolbar rows: tools, fill, and undo/redo; then the colour
/// palette and stroke-width presets. Also services the `Ctrl+Z` / `Ctrl+Shift+Z`
/// shortcuts. Anything that changes the document sets `dirty` so the canvas
/// re-renders this frame.
fn toolbar(ui: &mut egui::Ui, ed: &mut EditorState) {
    // `command` is Ctrl on Linux. Redo is the shifted chord. Both are suppressed
    // while a text entry is open: Ctrl+Z mid-word should not reach back and undo
    // a previous op.
    let (undo_key, redo_key) = if ed.typing.is_some() {
        (false, false)
    } else {
        ui.input(|i| {
            let z = i.key_pressed(egui::Key::Z) && i.modifiers.command;
            (z && !i.modifiers.shift, z && i.modifiers.shift)
        })
    };
    if undo_key && ed.doc.can_undo() {
        ed.doc.undo();
        ed.dirty = true;
    }
    if redo_key && ed.doc.can_redo() {
        ed.doc.redo();
        ed.dirty = true;
    }

    let tool_before = ed.tool;

    // Wrapped, so a tool row that outgrows the window folds onto a second line
    // instead of running off the edge.
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(&mut ed.tool, editor::Tool::Arrow, "Arrow");
        ui.selectable_value(&mut ed.tool, editor::Tool::Line, "Line");
        ui.selectable_value(&mut ed.tool, editor::Tool::Rect, "Rect");
        ui.selectable_value(&mut ed.tool, editor::Tool::Ellipse, "Oval");
        ui.selectable_value(&mut ed.tool, editor::Tool::Pencil, "Pencil");
        ui.selectable_value(&mut ed.tool, editor::Tool::Highlight, "Mark");
        ui.selectable_value(&mut ed.tool, editor::Tool::Blur, "Blur");
        ui.selectable_value(&mut ed.tool, editor::Tool::Text, "Text");
        ui.separator();
        // Fill only means something for the tools with an interior. The
        // highlighter is always filled and the pencil never is, so neither is
        // fillable in the toolbar's sense.
        let fillable = matches!(ed.tool, editor::Tool::Rect | editor::Tool::Ellipse);
        ui.add_enabled(fillable, egui::Checkbox::new(&mut ed.filled, "Fill"));
        ui.separator();
        if ui
            .add_enabled(ed.doc.can_undo(), egui::Button::new("Undo"))
            .clicked()
        {
            ed.doc.undo();
            ed.dirty = true;
        }
        if ui
            .add_enabled(ed.doc.can_redo(), egui::Button::new("Redo"))
            .clicked()
        {
            ed.doc.redo();
            ed.dirty = true;
        }
    });

    // Switching tools commits an open entry rather than dropping it — a
    // half-typed label should not vanish because the next tool was clicked.
    if ed.tool != tool_before {
        commit_text(ed);
    }

    ui.horizontal_wrapped(|ui| {
        for color in PALETTE {
            if color_swatch(ui, color, ed.color == color) {
                ed.color = color;
            }
        }
        ui.separator();
        ui.label("Width");
        ui.add(
            egui::DragValue::new(&mut ed.width)
                .speed(0.25)
                .range(MIN_WIDTH..=MAX_WIDTH)
                .max_decimals(0)
                .suffix(" px"),
        );
        ui.label("Size");
        ui.add(
            egui::DragValue::new(&mut ed.size)
                .speed(0.5)
                .range(editor::MIN_TEXT_SIZE..=editor::MAX_TEXT_SIZE)
                .max_decimals(0)
                .suffix(" px"),
        );
    });
}

/// Consume this frame's keystrokes into the open text entry.
///
/// `Escape` cancels, `Enter` commits, `Backspace` deletes, everything printable
/// appends. This runs only while an entry is open, and the two shortcuts it
/// shadows — `Escape` closing the window, `Ctrl+Z` undoing — are suppressed at
/// their own sites for exactly that window.
fn handle_typing(ui: &egui::Ui, ed: &mut EditorState) {
    let (typed, backspace, enter, escape) = ui.input(|i| {
        let mut typed = String::new();
        for event in &i.events {
            if let egui::Event::Text(s) = event {
                typed.push_str(s);
            }
        }
        (
            typed,
            i.key_pressed(egui::Key::Backspace),
            i.key_pressed(egui::Key::Enter),
            i.key_pressed(egui::Key::Escape),
        )
    });

    if escape {
        // Discard outright — the entry was never in the document to undo.
        ed.typing = None;
        return;
    }
    if enter {
        commit_text(ed);
        return;
    }
    let Some(entry) = ed.typing.as_mut() else {
        return;
    };
    if backspace {
        entry.buffer.pop();
    }
    entry.buffer.push_str(&typed);
}

/// Commit the open text entry to the document, if there is one worth keeping.
///
/// An empty buffer commits nothing: clicking to place a caret and then clicking
/// elsewhere should leave no trace, and an empty op would still consume an undo
/// step.
fn commit_text(ed: &mut EditorState) {
    let Some(entry) = ed.typing.take() else {
        return;
    };
    if entry.buffer.is_empty() {
        return;
    }
    ed.doc.push(editor::Op::Text {
        pos: entry.pos,
        text: entry.buffer,
        color: ed.color,
        size: ed.size,
    });
    ed.dirty = true;
}

fn color32(color: editor::Rgba) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}

/// A clickable colour square; a white frame marks the active one. Returns
/// whether it was clicked.
fn color_swatch(ui: &mut egui::Ui, color: editor::Rgba, selected: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::click());
    let col = egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3]);
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, col);
    if selected {
        painter.rect_stroke(
            rect,
            3.0,
            egui::Stroke::new(2.0, egui::Color32::WHITE),
            egui::StrokeKind::Middle,
        );
    }
    response.clicked()
}

/// Paint the in-progress op in screen space with the egui painter, from the drag
/// sampled so far. Fidelity to the committed render matters less than
/// responsiveness — this is drag feedback, redrawn every frame, never saved. The
/// pencil preview is the raw polyline, so a stroke visibly settles onto its
/// smoothed curve when it commits.
fn draw_preview(
    painter: &egui::Painter,
    tool: editor::Tool,
    path: &[egui::Pos2],
    color: editor::Rgba,
    width: f32,
    filled: bool,
) {
    // The same split `Op::from_drag` makes: the two-point tools span the drag,
    // the pencil follows every sample.
    let (Some(&a), Some(&b)) = (path.first(), path.last()) else {
        return;
    };
    let col = egui::Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3]);
    let stroke = egui::Stroke::new(width.max(1.0), col);
    match tool {
        editor::Tool::Arrow => painter.arrow(a, b - a, stroke),
        editor::Tool::Line => {
            painter.line_segment([a, b], stroke);
        }
        editor::Tool::Rect => {
            let rect = egui::Rect::from_two_pos(a, b);
            if filled {
                painter.rect_filled(rect, 0.0, col);
            }
            painter.rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Middle);
        }
        editor::Tool::Ellipse => {
            let pts = ellipse_points(a, b);
            if filled {
                painter.add(egui::Shape::convex_polygon(pts, col, stroke));
            } else {
                painter.add(egui::Shape::closed_line(pts, stroke));
            }
        }
        editor::Tool::Pencil => {
            painter.add(egui::Shape::line(path.to_vec(), stroke));
        }
        editor::Tool::Highlight => {
            let tint = egui::Color32::from_rgba_unmultiplied(
                color[0],
                color[1],
                color[2],
                editor::HIGHLIGHT_ALPHA,
            );
            painter.rect_filled(egui::Rect::from_two_pos(a, b), 0.0, tint);
        }
        // A marquee, not a stroke: fixed hairline width, no palette colour, and
        // no live blur — running the filter every frame would be the first thing
        // in this editor to cost real time. The effect lands on release.
        editor::Tool::Blur => {
            painter.rect_stroke(
                egui::Rect::from_two_pos(a, b),
                0.0,
                egui::Stroke::new(1.0, DESTRUCTIVE_PREVIEW),
                egui::StrokeKind::Middle,
            );
        }
        // Text is placed by clicking and typing, so there is no drag to preview.
        // The open entry is drawn separately, next to the canvas image.
        editor::Tool::Text => {}
    }
}

/// Points around the ellipse inscribed in the `a`–`b` bounding box, for the
/// preview path. egui has no ellipse primitive, so sample one.
fn ellipse_points(a: egui::Pos2, b: egui::Pos2) -> Vec<egui::Pos2> {
    const SEGMENTS: usize = 48;
    let center = egui::pos2((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
    let (rx, ry) = ((a.x - b.x).abs() / 2.0, (a.y - b.y).abs() / 2.0);
    (0..SEGMENTS)
        .map(|i| {
            let t = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            egui::pos2(center.x + rx * t.cos(), center.y + ry * t.sin())
        })
        .collect()
}

fn label(action: &str, done: bool) -> String {
    if done {
        format!("{action} ✓")
    } else {
        action.to_string()
    }
}
