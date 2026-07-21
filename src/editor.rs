// Annotation editor: an ordered operation list with a pointer.
//
// The list is the whole model. Undo and redo move `pointer`; a new op after an
// undo truncates everything past it. Each op is self-contained — a type plus its
// parameters, no references to prior state — so rendering is a pure walk of the
// visible prefix from the base image. See `render` (module `editor::render`) and
// the locked decisions in ROADMAP.md.

use image::RgbaImage;
use skrifa::{
    FontRef, MetadataProvider,
    instance::{LocationRef, Size},
    outline::{DrawSettings, OutlinePen},
};
use tiny_skia::{
    FillRule, IntRect, IntSize, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, PixmapPaint,
    Rect, Stroke, Transform,
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

/// The highlighter's opacity. Fixed, like the palette: those two constants are
/// the whole colour choice, and neither is a toolbar control. The op stores the
/// colour that was picked; this alpha is substituted in at render.
pub const HIGHLIGHT_ALPHA: u8 = 0x66;

/// How far apart two freehand samples must be, in base-image pixels, to both
/// survive `decimate`.
const MIN_SAMPLE_DIST: f32 = 2.0;

/// Blur strength, as a Gaussian sigma in base-image pixels. Absolute rather than
/// scaled to the region: redaction is binary — either the text is unreadable or
/// the op failed — and region, window and fullscreen captures are all cropped
/// from the same physical grab, so a pixel means the same thing in each.
const BLUR_SIGMA: f32 = 12.0;

/// Atkinson Hyperlegible Regular, embedded. Chosen for legibility over arbitrary
/// screenshot pixels — it disambiguates `O`/`0` and `I`/`l`/`1` by design — and
/// it happens to be the smallest sans-serif available at 29 KB. SIL OFL; the
/// licence sits beside it in `assets/`.
pub const FONT: &[u8] = include_bytes!("../assets/AtkinsonHyperlegible-Regular.otf");

/// Starting text size in base-image pixels.
pub const DEFAULT_TEXT_SIZE: f32 = 32.0;
pub const MIN_TEXT_SIZE: f32 = 8.0;
pub const MAX_TEXT_SIZE: f32 = 200.0;

/// One annotation. Milestone 1 is the four two-point vector tools; milestone 2
/// adds the freehand stroke and the highlighter. Text and the destructive ops
/// join the enum in later milestones.
///
/// `Clone` but not `Copy`: `Freehand` carries a variable-length point list, so
/// the enum can no longer be a bitwise copy. Nothing here required `Copy`.
#[derive(Clone, Debug, PartialEq)]
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
    /// A drawn stroke. The points are the drag as sampled, minus the duplicates
    /// `decimate` removed; the curve through them is `render`'s business, not the
    /// document's.
    Freehand {
        points: Vec<Point>,
        color: Rgba,
        width: f32,
    },
    /// A translucent block over the region `a`–`b`. No `width` — it is a fill
    /// with no outline — and no `filled`, since it is nothing else.
    Highlight {
        a: Point,
        b: Point,
        color: Rgba,
    },
    /// Blur everything already drawn inside the region `a`–`b`. The first
    /// destructive op: it carries no colour or width because it adds no ink of
    /// its own, only names a region and a moment in the list.
    Blur {
        a: Point,
        b: Point,
    },
    /// A single line of text. `pos` is the **top-left of the line box**, not the
    /// baseline — a click should put text below-right of the cursor, the way a
    /// caret implies. The baseline is derived at render from the font's ascent.
    Text {
        pos: Point,
        text: String,
        color: Rgba,
        size: f32,
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
    Pencil,
    Highlight,
    Blur,
    Text,
}

impl Op {
    /// Build the op a completed drag produces from the drag's sampled path, in
    /// image pixels, with the toolbar's current colour, width, and fill.
    ///
    /// A drag is always a point sequence; the tools differ in how much of it they
    /// consume. `Pencil` takes the whole path, the rest take its first and last
    /// point and ignore the middle. `filled` is ignored by the tools that have no
    /// interior. `None` for an empty path, which has no geometry to describe.
    pub fn from_drag(
        tool: Tool,
        path: &[Point],
        color: Rgba,
        width: f32,
        filled: bool,
    ) -> Option<Op> {
        let (&a, &b) = (path.first()?, path.last()?);
        Some(match tool {
            Tool::Arrow => Op::Arrow { a, b, color, width },
            Tool::Line => Op::Line { a, b, color, width },
            Tool::Rect => Op::Rect { a, b, color, width, filled },
            Tool::Ellipse => Op::Ellipse { a, b, color, width, filled },
            Tool::Highlight => Op::Highlight { a, b, color },
            Tool::Blur => Op::Blur { a, b },
            // Text is the first tool a drag does not produce. It is committed by
            // typing, not by releasing the pointer, so a drag with it selects
            // nothing and must commit nothing.
            Tool::Text => return None,
            Tool::Pencil => Op::Freehand {
                points: decimate(path, MIN_SAMPLE_DIST),
                color,
                width,
            },
        })
    }
}

/// Drop samples closer than `min_dist` to the last one kept. A pointer sampled
/// at 120 Hz oversamples badly — consecutive frames can land on the same pixel —
/// and those duplicates are pure jitter for `smooth_path` to bend around. The
/// first and last points always survive, so the stroke keeps the exact endpoints
/// the drag had.
///
/// This is baked into the stored op, unlike smoothing: it removes sampling noise
/// rather than expressing a curve, so nothing later would want the duplicates
/// back.
pub fn decimate(path: &[Point], min_dist: f32) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::new();
    for &p in path {
        let keep = match out.last() {
            Some(&last) => dist(last, p) >= min_dist,
            None => true,
        };
        if keep {
            out.push(p);
        }
    }
    // The final sample is where the drag actually ended. If it fell inside
    // `min_dist` of the previous survivor it was just dropped, which would stop
    // the stroke short of the cursor — put it back.
    if let (Some(&end), Some(&kept)) = (path.last(), out.last())
        && kept != end
    {
        out.push(end);
    }
    out
}

fn dist(a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    (dx * dx + dy * dy).sqrt()
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
        // `ref points` because `Op` is not `Copy` — the scalars still bind by
        // value, only the list has to borrow.
        Op::Freehand { ref points, color, width } => {
            if let Some(path) = smooth_path(points) {
                stroke(pm, &path, width, color);
            }
        }
        Op::Highlight { a, b, color } => {
            if let Some(rect) = norm_rect(a, b) {
                fill(
                    pm,
                    &PathBuilder::from_rect(rect),
                    with_alpha(color, HIGHLIGHT_ALPHA),
                );
            }
        }
        Op::Blur { a, b } => draw_blur(pm, a, b),
        Op::Text { pos, ref text, color, size } => draw_text(pm, pos, text, color, size),
    }
}

/// Collects skrifa's glyph outlines into a tiny-skia path.
///
/// The two APIs agree method for method, so the adapter's whole job is the
/// **coordinate flip**: font outlines are Y-up from an origin on the baseline,
/// while tiny-skia is Y-down from the image's top-left. Every emitted point is
/// therefore `pen_x + x` across and `baseline - y` down. Getting this backwards
/// renders legible-looking text mirrored about the baseline, which is why it has
/// its own test.
struct GlyphPen {
    pb: PathBuilder,
    pen_x: f32,
    baseline: f32,
}

impl GlyphPen {
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        (self.pen_x + x, self.baseline - y)
    }
}

impl OutlinePen for GlyphPen {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.pb.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.at(x, y);
        self.pb.line_to(x, y);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (cx, cy) = self.at(cx, cy);
        let (x, y) = self.at(x, y);
        self.pb.quad_to(cx, cy, x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (cx0, cy0) = self.at(cx0, cy0);
        let (cx1, cy1) = self.at(cx1, cy1);
        let (x, y) = self.at(x, y);
        self.pb.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }

    fn close(&mut self) {
        self.pb.close();
    }
}

/// Draw one line of `text` with its line box starting at `pos`.
///
/// Glyphs are filled paths, not a rasterised mask — the same `fill` every other
/// op uses — so text composites identically to the rest and a later blur can
/// smear it like anything else.
///
/// `FontRef` is parsed per op rather than cached. It is a zero-copy parse of the
/// table directory and `render` runs on edits, not per frame; caching it behind
/// a `OnceLock` is the obvious optimisation and is deliberately not done before
/// there is a measured reason.
fn draw_text(pm: &mut Pixmap, pos: Point, text: &str, color: Rgba, size: f32) {
    if text.is_empty() {
        return;
    }
    let Ok(font) = FontRef::new(FONT) else {
        return;
    };

    let px = Size::new(size);
    let metrics = font.metrics(px, LocationRef::default());
    let glyph_metrics = font.glyph_metrics(px, LocationRef::default());
    let charmap = font.charmap();
    let outlines = font.outline_glyphs();

    // `pos` is the top of the line box; the baseline sits one ascent below it.
    let baseline = pos.y + metrics.ascent;
    let mut pen_x = pos.x;

    for ch in text.chars() {
        // A character the font has no glyph for is skipped, not fatal — it costs
        // one missing character rather than the whole annotation.
        let Some(gid) = charmap.map(ch) else {
            continue;
        };

        if let Some(glyph) = outlines.get(gid) {
            let mut pen = GlyphPen {
                pb: PathBuilder::new(),
                pen_x,
                baseline,
            };
            let settings = DrawSettings::unhinted(px, LocationRef::default());
            if glyph.draw(settings, &mut pen).is_ok()
                && let Some(path) = pen.pb.finish()
            {
                fill(pm, &path, color);
            }
        }

        // Advance even when the glyph had no outline — a space has an advance
        // and no contours, and dropping it would close up the gap.
        pen_x += glyph_metrics.advance_width(gid).unwrap_or(0.0);
    }
}

/// Blur whatever has already been drawn inside the region `a`–`b`.
///
/// This is the first op that *reads* the pixmap instead of only adding to it,
/// and it is why list order has been load-bearing since milestone 1: `render`
/// walks the visible prefix, so by the time this runs, `pm` already holds every
/// earlier op composited onto the base. A blur therefore covers the annotations
/// beneath it and leaves later ones sharp, with no machinery beyond reading the
/// buffer it is handed.
///
/// `fast_blur` documents that it expects premultiplied alpha, which is exactly
/// what a `Pixmap` stores — the two agree with no conversion. That is moot while
/// the opacity invariant holds every alpha at 255, but the buffers would still
/// line up if it ever stopped.
///
/// Every step is fallible and every failure is silent: a degenerate drag, or one
/// entirely off-canvas, has nothing to blur, and returning is the correct
/// outcome rather than an error to report.
fn draw_blur(pm: &mut Pixmap, a: Point, b: Point) {
    let Some(rect) = clamped_int_rect(a, b, pm.width(), pm.height()) else {
        return;
    };
    let Some(region) = pm.clone_rect(rect) else {
        return;
    };
    let (w, h) = (region.width(), region.height());

    // `Pixmap` and `RgbaImage` are both tightly packed RGBA8, so the buffer
    // moves between them without a pixel being touched.
    let Some(src) = RgbaImage::from_raw(w, h, region.data().to_vec()) else {
        return;
    };
    let blurred = image::imageops::fast_blur(&src, BLUR_SIGMA);
    let Some(out) =
        IntSize::from_wh(w, h).and_then(|size| Pixmap::from_vec(blurred.into_raw(), size))
    else {
        return;
    };

    pm.draw_pixmap(
        rect.x(),
        rect.y(),
        out.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// The integer pixel rectangle spanned by two drag corners, clipped to the
/// canvas. `None` for a degenerate rectangle or one lying entirely outside it —
/// `from_ltrb` rejects both, since either way there is no region to operate on.
fn clamped_int_rect(a: Point, b: Point, w: u32, h: u32) -> Option<IntRect> {
    // Outward rounding, so a drag never blurs less than it covered.
    let left = a.x.min(b.x).floor().max(0.0) as i32;
    let top = a.y.min(b.y).floor().max(0.0) as i32;
    let right = (a.x.max(b.x).ceil() as i32).min(w as i32);
    let bottom = (a.y.max(b.y).ceil() as i32).min(h as i32);
    IntRect::from_ltrb(left, top, right, bottom)
}

/// A palette colour at a given opacity. The op carries the colour that was
/// picked; the tool's alpha is applied here, so the document never stores a
/// derived value.
pub fn with_alpha(color: Rgba, alpha: u8) -> Rgba {
    [color[0], color[1], color[2], alpha]
}

/// One smoothed path through `path`. Each interior sample becomes a quadratic
/// control point and the curve passes through the midpoints between consecutive
/// samples — the standard midpoint-quadratic walk. It rounds the corners off a
/// sampled drag without needing tangent estimation, and the curve never swings
/// wide of the points that were actually drawn.
///
/// One path for the whole stroke, not one per segment: the curves have to span
/// segment boundaries to smooth anything, and a single path lets the stroker
/// resolve every join once.
fn smooth_path(path: &[Point]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    match path {
        [] => return None,
        // A press that never moved. A zero-length segment under a round cap is
        // the dot.
        [p] => {
            pb.move_to(p.x, p.y);
            pb.line_to(p.x, p.y);
        }
        [a, b] => {
            pb.move_to(a.x, a.y);
            pb.line_to(b.x, b.y);
        }
        [first, rest @ ..] => {
            pb.move_to(first.x, first.y);
            for pair in rest.windows(2) {
                let (ctrl, next) = (pair[0], pair[1]);
                pb.quad_to(
                    ctrl.x,
                    ctrl.y,
                    (ctrl.x + next.x) / 2.0,
                    (ctrl.y + next.y) / 2.0,
                );
            }
            // The walk above stops at the midpoint before the final sample; run
            // out to it so the stroke ends where the drag did.
            let end = rest[rest.len() - 1];
            pb.line_to(end.x, end.y);
        }
    }
    pb.finish()
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

    #[test]
    fn decimate_drops_dense_samples_but_keeps_both_endpoints() {
        let path = [
            Point::new(0.0, 0.0),
            Point::new(0.5, 0.0),  // within 2.0 of the first — dropped
            Point::new(1.0, 0.0),  // still within 2.0 of the first — dropped
            Point::new(10.0, 0.0), // far enough — kept
            Point::new(10.2, 0.0), // too close to keep, but it is where the drag
                                   // ended, so it is restored
        ];
        let out = decimate(&path, 2.0);
        assert_eq!(
            out,
            vec![
                Point::new(0.0, 0.0),
                Point::new(10.0, 0.0),
                Point::new(10.2, 0.0),
            ]
        );
    }

    #[test]
    fn decimate_of_an_empty_path_is_empty() {
        assert!(decimate(&[], 2.0).is_empty());
    }

    fn black(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, image::Rgba([0, 0, 0, 255]))
    }

    #[test]
    fn freehand_renders_ink_along_its_path() {
        let mut d = Document::new(black(40, 40));
        d.push(Op::Freehand {
            points: vec![
                Point::new(5.0, 20.0),
                Point::new(20.0, 20.0),
                Point::new(35.0, 20.0),
            ],
            color: [255, 0, 0, 255],
            width: 4.0,
        });
        let out = render(&d);
        let on = out.get_pixel(20, 20);
        assert!(on[0] > 200, "ink on the stroke, got {on:?}");
        let off = out.get_pixel(20, 2);
        assert!(off[0] < 64, "no ink far from the stroke, got {off:?}");
    }

    #[test]
    fn a_one_point_freehand_stroke_renders_a_dot() {
        let mut d = Document::new(black(20, 20));
        d.push(Op::Freehand {
            points: vec![Point::new(10.0, 10.0)],
            color: [255, 0, 0, 255],
            width: 6.0,
        });
        let out = render(&d);
        let center = out.get_pixel(10, 10);
        assert!(center[0] > 200, "a click with no drag still marks, got {center:?}");
    }

    #[test]
    fn a_two_point_freehand_stroke_renders_a_segment() {
        let mut d = Document::new(black(20, 20));
        d.push(Op::Freehand {
            points: vec![Point::new(4.0, 10.0), Point::new(16.0, 10.0)],
            color: [255, 0, 0, 255],
            width: 4.0,
        });
        let out = render(&d);
        let mid = out.get_pixel(10, 10);
        assert!(mid[0] > 200, "ink along the segment, got {mid:?}");
    }

    #[test]
    fn a_highlight_tints_the_base_without_replacing_it() {
        // White base, gold highlight: the tint is unambiguous in the blue channel.
        let base = RgbaImage::from_pixel(10, 10, image::Rgba([255, 255, 255, 255]));
        let mut d = Document::new(base);
        d.push(Op::Highlight {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 10.0),
            color: [0xf6, 0xc1, 0x77, 0xff],
        });
        let out = render(&d);
        let p = out.get_pixel(5, 5);
        // Blue falls toward gold's 0x77 without reaching it — the white base is
        // still showing through, which is what makes it a highlighter and not a
        // rectangle.
        assert!(
            p[2] < 255 && p[2] > 0x77,
            "gold tint over white, got {p:?}"
        );
    }

    /// Half black, half white, split down the middle — a hard vertical edge for
    /// a blur to soften.
    fn split(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, _| {
            if x < w / 2 {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([255, 255, 255, 255])
            }
        })
    }

    fn blur(a: Point, b: Point) -> Op {
        Op::Blur { a, b }
    }

    #[test]
    fn a_blur_destroys_detail_inside_its_region() {
        let mut d = Document::new(split(60, 60));
        d.push(blur(Point::new(0.0, 0.0), Point::new(60.0, 60.0)));
        let out = render(&d);
        // Either side of the seam was pure black and pure white; after a blur
        // both must have moved toward each other.
        let left = out.get_pixel(28, 30)[0];
        let right = out.get_pixel(31, 30)[0];
        assert!(left > 20, "black side lifted by the blur, got {left}");
        assert!(right < 235, "white side pulled down by the blur, got {right}");
    }

    #[test]
    fn a_blur_is_bounded_by_its_rectangle() {
        let base = split(60, 60);
        let clean = render(&Document::new(base.clone()));

        let mut d = Document::new(base);
        d.push(blur(Point::new(20.0, 20.0), Point::new(40.0, 40.0)));
        let out = render(&d);

        // A pixel well outside the blurred rect is untouched, byte for byte.
        assert_eq!(out.get_pixel(5, 5), clean.get_pixel(5, 5));
        assert_eq!(out.get_pixel(55, 55), clean.get_pixel(55, 55));
        // ...while the seam inside it moved.
        assert_ne!(out.get_pixel(29, 30), clean.get_pixel(29, 30));
    }

    #[test]
    fn a_blur_covers_earlier_ops_and_not_later_ones() {
        // The milestone's real claim: destructive ops act on the prefix beneath
        // them, so list position decides what they touch. Nothing tested this
        // until blur existed to test it with.
        let region = (Point::new(10.0, 10.0), Point::new(50.0, 50.0));
        let mark = |p: Point| Op::Rect {
            a: Point::new(p.x, p.y),
            b: Point::new(p.x + 12.0, p.y + 12.0),
            color: [255, 0, 0, 255],
            width: 1.0,
            filled: true,
        };

        // Same two ops, opposite order.
        let mut under = Document::new(split(60, 60));
        under.push(mark(Point::new(20.0, 20.0)));
        under.push(blur(region.0, region.1));

        let mut over = Document::new(split(60, 60));
        over.push(blur(region.0, region.1));
        over.push(mark(Point::new(20.0, 20.0)));

        let blurred_mark = under_pixel(&render(&under));
        let sharp_mark = under_pixel(&render(&over));

        // Drawn after the blur, the rect keeps its exact fill colour.
        assert_eq!(sharp_mark, [255, 0, 0], "op after the blur stays sharp");
        // Drawn before it, the same rect has been smeared into its surroundings.
        assert_ne!(blurred_mark, [255, 0, 0], "op before the blur is blurred");
        assert!(
            blurred_mark[0] < 255 || blurred_mark[1] > 0,
            "the blurred rect bled with the background, got {blurred_mark:?}"
        );
    }

    /// The RGB of the pixel at the centre of the test rect above.
    fn under_pixel(img: &RgbaImage) -> [u8; 3] {
        let p = img.get_pixel(26, 26);
        [p[0], p[1], p[2]]
    }

    #[test]
    fn a_degenerate_blur_renders_and_changes_nothing() {
        let base = split(20, 20);
        let clean = render(&Document::new(base.clone()));
        let mut d = Document::new(base);
        // Zero area, and a rectangle entirely off-canvas.
        d.push(blur(Point::new(5.0, 5.0), Point::new(5.0, 5.0)));
        d.push(blur(Point::new(90.0, 90.0), Point::new(120.0, 120.0)));
        let out = render(&d);
        assert_eq!(out.as_raw(), clean.as_raw(), "no-op blurs must not alter pixels");
    }

    #[test]
    fn a_blur_leaves_the_output_fully_opaque() {
        let mut d = Document::new(split(30, 30));
        d.push(blur(Point::new(5.0, 5.0), Point::new(25.0, 25.0)));
        let out = render(&d);
        assert!(out.pixels().all(|px| px[3] == 255), "blur must not open holes");
    }

    fn text_op(pos: Point, s: &str, size: f32) -> Op {
        Op::Text {
            pos,
            text: s.to_string(),
            color: [255, 0, 0, 255],
            size,
        }
    }

    /// Bounding box of red ink over a black base, as `(x0, y0, x1, y1)`.
    fn ink_bounds(img: &RgbaImage) -> Option<(u32, u32, u32, u32)> {
        let mut bounds: Option<(u32, u32, u32, u32)> = None;
        for (x, y, px) in img.enumerate_pixels() {
            if px[0] > 100 {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
        bounds
    }

    #[test]
    fn text_renders_below_its_anchor_not_above_it() {
        // The Y flip is the one thing in this milestone that can invert
        // silently. Font outlines are Y-up from a baseline origin; tiny-skia is
        // Y-down from the top-left. Flipped the wrong way the glyphs render
        // *above* the anchor and still look like perfectly good text, so only a
        // positional assertion catches it.
        let size = 40.0;
        let anchor = Point::new(10.0, 40.0);
        let mut d = Document::new(black(200, 140));
        d.push(text_op(anchor, "Hxy", size));

        let out = render(&d);
        let (x0, y0, _, y1) = ink_bounds(&out).expect("text drew ink");

        // One row of tolerance for anti-aliasing at the cap line.
        assert!(
            y0 + 1 >= anchor.y as u32,
            "no ink above the anchor; topmost inked row was {y0}, anchor at {}",
            anchor.y
        );
        assert!(
            x0 + 1 >= anchor.x as u32,
            "ink starts at the anchor; leftmost inked column was {x0}"
        );
        assert!(
            (y1 as f32) < anchor.y + size * 1.6,
            "ink stays within about one line box below the anchor; bottom row {y1}"
        );
    }

    #[test]
    fn text_advances_so_more_characters_reach_further() {
        let render_at = |s: &str| {
            let mut d = Document::new(black(400, 100));
            d.push(text_op(Point::new(10.0, 10.0), s, 40.0));
            render(&d)
        };

        let one = ink_bounds(&render_at("M")).expect("one glyph drew");
        let two = ink_bounds(&render_at("MM")).expect("two glyphs drew");
        assert!(
            two.2 > one.2,
            "two glyphs reach further right than one: {} vs {}",
            two.2,
            one.2
        );

        // A space has an advance and no contours. Advancing only on glyphs that
        // drew something would close the gap up, so the spaced string must be
        // the wider one.
        let tight = ink_bounds(&render_at("MM")).expect("drew");
        let spaced = ink_bounds(&render_at("M M")).expect("drew");
        assert!(
            spaced.2 > tight.2,
            "a space advances the pen: {} vs {}",
            spaced.2,
            tight.2
        );
    }

    #[test]
    fn empty_text_renders_nothing() {
        let base = black(50, 50);
        let clean = render(&Document::new(base.clone()));
        let mut d = Document::new(base);
        d.push(text_op(Point::new(5.0, 5.0), "", 20.0));
        assert_eq!(render(&d).as_raw(), clean.as_raw());
    }

    #[test]
    fn characters_without_a_glyph_are_skipped_not_fatal() {
        // A CJK ideograph this Latin font has no glyph for, between two it does.
        let mut d = Document::new(black(240, 100));
        d.push(text_op(Point::new(10.0, 10.0), "A漢B", 30.0));
        let out = render(&d);
        assert!(
            ink_bounds(&out).is_some(),
            "the Latin characters still drew around the missing glyph"
        );
    }

    #[test]
    fn text_leaves_the_output_fully_opaque() {
        let mut d = Document::new(black(200, 100));
        d.push(text_op(Point::new(10.0, 10.0), "opaque", 30.0));
        let out = render(&d);
        assert!(out.pixels().all(|px| px[3] == 255), "text must not open holes");
    }

    #[test]
    fn a_translucent_op_leaves_the_output_fully_opaque() {
        // The invariant `render` depends on: it copies tiny-skia's premultiplied
        // buffer straight into a straight-RGBA image, which is only valid because
        // every pixel is opaque. A translucent op is the first thing that could
        // have broken it — blending over an opaque base must still yield opaque.
        let mut d = Document::new(black(10, 10));
        d.push(Op::Highlight {
            a: Point::new(2.0, 2.0),
            b: Point::new(8.0, 8.0),
            color: [0xf6, 0xc1, 0x77, 0xff],
        });
        let out = render(&d);
        assert!(
            out.pixels().all(|px| px[3] == 255),
            "a translucent op must not open holes in the capture"
        );
    }
}
