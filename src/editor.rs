// Annotation editor: an ordered operation list with a pointer.
//
// The list is the whole model. Undo and redo move `pointer`; a new op after an
// undo truncates everything past it. Each op is self-contained — a type plus its
// parameters, no references to prior state — so rendering is a pure walk of the
// visible prefix from the base image. See `render` (module `editor::render`) and
// the locked decisions in ROADMAP.md.

use image::RgbaImage;
use tiny_skia::{
    FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

/// A point in base-image pixel space. `f32` so a drag can land sub-pixel; the
/// renderer works in the same space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// Straight RGBA, one byte per channel — the same convention as `RgbaImage`.
pub type Rgba = [u8; 4];

/// One annotation. Milestone 1 is the four two-point vector tools; freehand,
/// text, and the destructive ops join the enum in later milestones.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Arrow {
        a: Point,
        b: Point,
        color: Rgba,
        width: f32,
    },
    Line {
        a: Point,
        b: Point,
        color: Rgba,
        width: f32,
    },
    Rect {
        a: Point,
        b: Point,
        color: Rgba,
        width: f32,
        filled: bool,
    },
    Ellipse {
        a: Point,
        b: Point,
        color: Rgba,
        width: f32,
        filled: bool,
    },
}

/// Which tool a drag produces. Kept separate from `Op` because the toolbar picks
/// a tool before there is any geometry to make an op from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Arrow,
    Line,
    Rect,
    Ellipse,
}

impl Op {
    /// Build the op a completed drag produces: `a`→`b` in image pixels, with the
    /// toolbar's current colour, width, and fill. `filled` is ignored by the
    /// tools that have no interior.
    pub fn from_drag(tool: Tool, a: Point, b: Point, color: Rgba, width: f32, filled: bool) -> Op {
        match tool {
            Tool::Arrow => Op::Arrow { a, b, color, width },
            Tool::Line => Op::Line { a, b, color, width },
            Tool::Rect => Op::Rect { a, b, color, width, filled },
            Tool::Ellipse => Op::Ellipse { a, b, color, width, filled },
        }
    }
}

/// The edit state: an immutable base image plus the op list and its pointer.
///
/// `pointer` is the count of *applied* ops — `ops[..pointer]` is what renders.
/// Ops past the pointer are the redo tail, kept until the next `push` truncates
/// them.
pub struct Document {
    pub base: RgbaImage,
    ops: Vec<Op>,
    pointer: usize,
}

impl Document {
    pub fn new(base: RgbaImage) -> Self {
        Self {
            base,
            ops: Vec::new(),
            pointer: 0,
        }
    }

    /// Commit a new op. Anything past the pointer (a redo tail) is discarded
    /// first, so editing after an undo forks a new history.
    pub fn push(&mut self, op: Op) {
        self.ops.truncate(self.pointer);
        self.ops.push(op);
        self.pointer += 1;
    }

    pub fn undo(&mut self) {
        if self.pointer > 0 {
            self.pointer -= 1;
        }
    }

    pub fn redo(&mut self) {
        if self.pointer < self.ops.len() {
            self.pointer += 1;
        }
    }

    pub fn can_undo(&self) -> bool {
        self.pointer > 0
    }

    pub fn can_redo(&self) -> bool {
        self.pointer < self.ops.len()
    }

    /// The prefix that renders: every op up to the pointer, in order.
    pub fn visible_ops(&self) -> &[Op] {
        &self.ops[..self.pointer]
    }
}

/// Flatten the visible prefix onto the base image. Pure: same document in, same
/// pixels out. This is the whole render path — no diffing, no caching; the caller
/// decides *when* to call it (on edits, not every frame).
///
/// tiny-skia works in premultiplied RGBA. The base is an opaque screenshot, and
/// blending anything over opaque pixels leaves them opaque — so the result is
/// fully opaque, where premultiplied and straight RGBA coincide, and the buffer
/// copies straight back into an `RgbaImage`. (The same fact `base_pixmap` in
/// `overlay.rs` relies on, extended to the output.)
pub fn render(doc: &Document) -> RgbaImage {
    let (w, h) = (doc.base.width(), doc.base.height());
    let mut pm = Pixmap::new(w, h).expect("pixmap alloc");
    pm.data_mut().copy_from_slice(doc.base.as_raw());

    for op in doc.visible_ops() {
        draw_op(&mut pm, op);
    }

    RgbaImage::from_raw(w, h, pm.data().to_vec()).expect("pixmap buffer matches image dimensions")
}

fn draw_op(pm: &mut Pixmap, op: &Op) {
    match *op {
        Op::Line { a, b, color, width } => stroke_segment(pm, a, b, width, color),
        Op::Arrow { a, b, color, width } => draw_arrow(pm, a, b, width, color),
        Op::Rect { a, b, color, width, filled } => {
            if let Some(rect) = norm_rect(a, b) {
                let path = PathBuilder::from_rect(rect);
                if filled {
                    fill(pm, &path, color);
                }
                stroke(pm, &path, width, color);
            }
        }
        Op::Ellipse { a, b, color, width, filled } => {
            if let Some(path) = norm_rect(a, b).and_then(PathBuilder::from_oval) {
                if filled {
                    fill(pm, &path, color);
                }
                stroke(pm, &path, width, color);
            }
        }
    }
}

/// A solid-colour, anti-aliased paint. `'static` because a solid colour carries
/// no borrowed shader.
fn paint(color: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    p.anti_alias = true;
    p
}

/// Round caps and joins so segments and corners read as drawn strokes, not
/// blueprint lines.
fn stroke_spec(width: f32) -> Stroke {
    Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

fn stroke(pm: &mut Pixmap, path: &Path, width: f32, color: Rgba) {
    pm.stroke_path(
        path,
        &paint(color),
        &stroke_spec(width),
        Transform::identity(),
        None,
    );
}

fn fill(pm: &mut Pixmap, path: &Path, color: Rgba) {
    pm.fill_path(
        path,
        &paint(color),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn stroke_segment(pm: &mut Pixmap, a: Point, b: Point, width: f32, color: Rgba) {
    let mut pb = PathBuilder::new();
    pb.move_to(a.x, a.y);
    pb.line_to(b.x, b.y);
    if let Some(path) = pb.finish() {
        stroke(pm, &path, width, color);
    }
}

/// The axis-aligned rectangle spanned by two drag corners, in any order. `None`
/// for a degenerate (zero-area) drag, which `Rect::from_xywh` rejects.
fn norm_rect(a: Point, b: Point) -> Option<Rect> {
    Rect::from_xywh(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
}

/// An arrow `a → b`: a shaft that stops at the base of the head, then a filled
/// triangular head at `b`. The shaft stops short so its round cap sits *under*
/// the head instead of poking past the tip. The head is sized off the stroke
/// width with a floor, and never longer than the arrow itself.
fn draw_arrow(pm: &mut Pixmap, a: Point, b: Point, width: f32, color: Rgba) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        // No direction to point a head — draw the shaft alone.
        stroke_segment(pm, a, b, width, color);
        return;
    }
    let (ux, uy) = (dx / len, dy / len); // unit vector along the shaft
    let (px, py) = (-uy, ux); // perpendicular
    let head = (width * 4.0).max(12.0).min(len); // clamp so it can't overrun a short arrow
    let base = Point::new(b.x - ux * head, b.y - uy * head);

    // Shaft only when there is room ahead of the head.
    if len > head {
        stroke_segment(pm, a, base, width, color);
    }

    let half = head * 0.5;
    let mut pb = PathBuilder::new();
    pb.move_to(b.x, b.y);
    pb.line_to(base.x + px * half, base.y + py * half);
    pb.line_to(base.x - px * half, base.y - py * half);
    pb.close();
    if let Some(path) = pb.finish() {
        fill(pm, &path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::new(RgbaImage::new(1, 1))
    }

    fn op(width: f32) -> Op {
        Op::Line {
            a: Point::new(0.0, 0.0),
            b: Point::new(1.0, 1.0),
            color: [255, 0, 0, 255],
            width,
        }
    }

    #[test]
    fn push_advances_pointer() {
        let mut d = doc();
        assert!(!d.can_undo());
        d.push(op(1.0));
        assert_eq!(d.visible_ops().len(), 1);
        assert!(d.can_undo());
        assert!(!d.can_redo());
    }

    #[test]
    fn undo_and_redo_move_without_dropping_ops() {
        let mut d = doc();
        d.push(op(1.0));
        d.push(op(2.0));
        d.undo();
        assert_eq!(d.visible_ops().len(), 1);
        assert!(d.can_redo());
        d.redo();
        assert_eq!(d.visible_ops().len(), 2);
        assert!(!d.can_redo());
    }

    #[test]
    fn push_after_undo_truncates_the_tail() {
        let mut d = doc();
        d.push(op(1.0));
        d.push(op(2.0));
        d.undo();
        // The op with width 2.0 is now a redo tail; a new push must discard it.
        d.push(op(3.0));
        assert!(!d.can_redo());
        assert_eq!(d.visible_ops().len(), 2);
        assert_eq!(d.visible_ops()[1], op(3.0));
    }

    #[test]
    fn undo_and_redo_saturate_at_the_bounds() {
        let mut d = doc();
        d.undo(); // no-op at the empty bound
        assert_eq!(d.visible_ops().len(), 0);
        d.push(op(1.0));
        d.redo(); // no-op at the full bound
        assert_eq!(d.visible_ops().len(), 1);
    }

    fn filled_rect(a: Point, b: Point, color: Rgba) -> Op {
        Op::Rect {
            a,
            b,
            color,
            width: 1.0,
            filled: true,
        }
    }

    #[test]
    fn later_ops_render_on_top() {
        // Black base; a red rect, then a smaller green rect inside it.
        let base = RgbaImage::from_pixel(10, 10, image::Rgba([0, 0, 0, 255]));
        let mut d = Document::new(base);
        d.push(filled_rect(Point::new(0.0, 0.0), Point::new(9.0, 9.0), [255, 0, 0, 255]));
        d.push(filled_rect(Point::new(2.0, 2.0), Point::new(7.0, 7.0), [0, 255, 0, 255]));

        let out = render(&d);
        // (4,4) is solidly inside the green rect: green won because it is later.
        let center = out.get_pixel(4, 4);
        assert!(center[1] > 200, "green channel dominant, got {center:?}");
        assert!(center[0] < 64, "red suppressed by the op on top, got {center:?}");

        // A pixel inside the red rect but outside the green one stays red.
        let ring = out.get_pixel(1, 1);
        assert!(ring[0] > 200 && ring[1] < 64, "outer ring still red, got {ring:?}");
    }
}
