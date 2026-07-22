// Scrolling capture: match consecutive frames by sum-of-absolute-differences
// and fold each one into a growing image as it arrives.
//
// The algorithm and every constant here come from the feasibility spike
// (`a6762c1`, written up in SPIKE-FINDINGS.md), which proved the approach and
// then took a naive 9454 ms stitch down to 1589 ms. They are carried over
// unchanged and are not to be re-derived.
//
// The one deliberate departure: the spike collected every frame and stitched at
// the end, which cost ~9 MB per frame — about 5.8 GB over its 639-frame run.
// Its own timing shows a fold costs ~2.5 ms against a 33 ms frame budget, so
// folding as we go is affordable and holds only the accumulator and one frame of
// luma. The price is that a capture cannot be re-stitched with different
// parameters afterwards.

use image::RgbaImage;

use crate::overlay::Rect;

/// Bottom-strip height used as the match template.
const STRIP_MAX: usize = 200;
/// Subsample columns for speed.
const COL_STEP: usize = 4;
/// Real scroll runs 33–66 px/frame at 30 fps, so searching further is waste.
const MAX_SEARCH_OFFSET: usize = 300;
/// Matching never reads above this many rows from the frame bottom, so only that
/// band is converted to luma. This change alone took luma cost 1227 ms → 461 ms
/// with byte-identical output.
const BAND_ROWS: usize = STRIP_MAX + MAX_SEARCH_OFFSET;

/// Mean per-pixel luma difference above which a pair counts as unmatched.
///
/// **Never exercised on real data.** Across the spike's 639-frame run, matches
/// scored 2.7–4.6 against this 15.0 — a 4–5× margin — and nothing was ever
/// rejected. The graceful-gap path below has therefore only ever run in a unit
/// test, which is the sole evidence it behaves at all.
const SAD_REJECT_THRESHOLD: f64 = 15.0;

/// Stop accumulating past this height. Bounds a capture that gets started and
/// forgotten, without limiting any realistic scroll — roughly twelve viewports.
const MAX_ACC_ROWS: u32 = 20_000;

/// Convert a selection in physical grab pixels to the logical output coordinates
/// `capture_output_region` expects.
///
/// `overlay::select_region` works in the grab's physical pixels, because that is
/// what cropping the grab requires. `capture_output_region` takes logical
/// coordinates. At scale 2 the two differ by a factor of two, and confusing them
/// captures a rectangle of roughly the right shape in the wrong place — which
/// presents as a capture bug rather than a units bug, so it is worth being
/// explicit about.
///
/// Sub-logical-pixel precision is lost here and cannot be otherwise: the region
/// is re-captured rather than cropped out of the grab, so a rectangle that began
/// on an odd physical pixel lands on the even one below it.
pub fn physical_to_logical(rect: &Rect, scale: i32) -> (i32, i32, i32, i32) {
    let s = scale.max(1);
    (
        rect.x as i32 / s,
        rect.y as i32 / s,
        (rect.width as i32 / s).max(1),
        (rect.height as i32 / s).max(1),
    )
}

/// The bottom band of a frame, as single-channel luma.
struct Luma {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

fn to_luma(img: &RgbaImage) -> Luma {
    let (w, full_h) = (img.width() as usize, img.height() as usize);
    // Only the bottom band participates in matching; skip converting the rest.
    let h = full_h.min(BAND_ROWS);
    let raw = img.as_raw();
    let start = (full_h - h) * w * 4;
    let mut px = Vec::with_capacity(w * h);
    let mut i = start;
    for _ in 0..(w * h) {
        let (r, g, b) = (raw[i] as u32, raw[i + 1] as u32, raw[i + 2] as u32);
        // Integer approximation of 0.299R + 0.587G + 0.114B.
        px.push(((77 * r + 150 * g + 29 * b) >> 8) as u8);
        i += 4;
    }
    Luma { w, h, px }
}

/// Result of matching one frame's content against the bottom strip of the last.
#[derive(Clone, Copy, Debug)]
struct Match {
    /// Pixels the content scrolled up between the two frames.
    offset: usize,
    /// Mean per-pixel luma difference at the best offset (0..255).
    mean_sad: f64,
}

/// Find how far content scrolled between `prev` and `next` by sliding `prev`'s
/// bottom strip upward over `next` and minimising SAD. Forward scroll only
/// (offset ≥ 0); backward scroll is out of scope by the roadmap.
fn find_offset(prev: &Luma, next: &Luma) -> Match {
    debug_assert_eq!((prev.w, prev.h), (next.w, next.h));
    let (w, h) = (prev.w, prev.h);
    // Fixed-height template from the frame bottom; halved for tiny frames so a
    // search range still remains.
    let strip_h = STRIP_MAX.min(h / 2).max(1);
    let max_offset = (h - strip_h).min(MAX_SEARCH_OFFSET);

    let mut best_sad = u64::MAX;
    let mut best_d = 0usize;

    // `prev`'s strip occupies rows [h - strip_h, h). At offset d it aligns with
    // `next` rows [h - strip_h - d, h - d).
    for d in 0..=max_offset {
        let mut sad = 0u64;
        'rows: for r in 0..strip_h {
            let prow = (h - strip_h + r) * w;
            let nrow = (h - strip_h - d + r) * w;
            let mut c = 0;
            while c < w {
                let diff = (prev.px[prow + c] as i32 - next.px[nrow + c] as i32).unsigned_abs();
                sad += diff as u64;
                // Early abort: this offset can no longer beat the incumbent.
                if sad >= best_sad {
                    break 'rows;
                }
                c += COL_STEP;
            }
        }
        if sad < best_sad {
            best_sad = sad;
            best_d = d;
        }
    }

    let n_cols = w.div_ceil(COL_STEP);
    let mean_sad = best_sad as f64 / (strip_h * n_cols) as f64;
    Match {
        offset: best_d,
        mean_sad,
    }
}

/// What a frame contributed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// New content appended, this many rows.
    Appended(u32),
    /// The view had not moved; nothing to add.
    Duplicate,
    /// No confident overlap. The frame is dropped and a gap accepted.
    Rejected,
    /// The height cap is reached; nothing further will be appended.
    Full,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Stats {
    pub appended: usize,
    pub duplicates: usize,
    pub rejected: usize,
    pub rows: u32,
}

/// Accumulates frames into one tall image as they arrive.
pub struct Stitcher {
    w: u32,
    h: u32,
    acc: Vec<u8>,
    rows: u32,
    prev: Luma,
    stats: Stats,
    full: bool,
}

impl Stitcher {
    pub fn new(first: &RgbaImage) -> Self {
        Stitcher {
            w: first.width(),
            h: first.height(),
            acc: first.as_raw().clone(),
            rows: first.height(),
            prev: to_luma(first),
            stats: Stats {
                rows: first.height(),
                ..Default::default()
            },
            full: false,
        }
    }

    pub fn push(&mut self, frame: &RgbaImage) -> Push {
        if self.full {
            return Push::Full;
        }
        // A frame of different dimensions cannot be matched against the anchor —
        // the viewport changed, which is out of scope by the roadmap.
        if frame.width() != self.w || frame.height() != self.h {
            self.stats.rejected += 1;
            return Push::Rejected;
        }

        let next = to_luma(frame);
        let m = find_offset(&self.prev, &next);

        if m.mean_sad > SAD_REJECT_THRESHOLD {
            // Keep the old anchor deliberately: a later frame may still match
            // the view we last trusted, so holding position recovers, where
            // advancing onto an unmatched frame would compound the error.
            self.stats.rejected += 1;
            return Push::Rejected;
        }

        // Both remaining cases are confidently the same view, so the anchor moves.
        self.prev = next;

        if m.offset == 0 {
            self.stats.duplicates += 1;
            return Push::Duplicate;
        }

        let offset = m.offset as u32;
        if self.rows + offset > MAX_ACC_ROWS {
            self.full = true;
            return Push::Full;
        }

        // Append only the freshly-revealed rows from the frame's bottom.
        let start = (self.h as usize - m.offset) * self.w as usize * 4;
        self.acc.extend_from_slice(&frame.as_raw()[start..]);
        self.rows += offset;
        self.stats.appended += 1;
        self.stats.rows = self.rows;
        Push::Appended(offset)
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    pub fn finish(self) -> RgbaImage {
        RgbaImage::from_raw(self.w, self.rows, self.acc)
            .expect("accumulated buffer matches its dimensions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tall synthetic document with per-row texture and no short repeat, so
    /// SAD has something unambiguous to lock onto. Deliberately unlike the
    /// periodic content the spike found ambiguous — that weakness is known and
    /// is not what these tests are measuring.
    fn document(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            let v = ((y.wrapping_mul(2_654_435_761) ^ x.wrapping_mul(40_503)) >> 16) as u8;
            image::Rgba([v, v.wrapping_mul(3), v.wrapping_add(70), 255])
        })
    }

    /// The window a viewport would show with the document scrolled to `top`.
    fn viewport(doc: &RgbaImage, top: u32, h: u32) -> RgbaImage {
        image::imageops::crop_imm(doc, 0, top, doc.width(), h).to_image()
    }

    #[test]
    fn find_offset_recovers_a_known_scroll() {
        let doc = document(64, 2000);
        let a = to_luma(&viewport(&doc, 0, 600));
        let b = to_luma(&viewport(&doc, 45, 600));
        let m = find_offset(&a, &b);
        assert_eq!(m.offset, 45, "recovered offset (sad {:.1})", m.mean_sad);
        assert!(m.mean_sad < SAD_REJECT_THRESHOLD);
    }

    #[test]
    fn a_static_view_yields_zero_offset() {
        let doc = document(64, 1200);
        let v = viewport(&doc, 0, 600);
        let m = find_offset(&to_luma(&v), &to_luma(&v));
        assert_eq!(m.offset, 0);
        assert_eq!(m.mean_sad, 0.0);
    }

    #[test]
    fn the_stitcher_appends_exactly_the_revealed_rows() {
        let doc = document(64, 2000);
        let vh = 600;
        let mut s = Stitcher::new(&viewport(&doc, 0, vh));
        assert_eq!(s.push(&viewport(&doc, 40, vh)), Push::Appended(40));
        assert_eq!(s.push(&viewport(&doc, 80, vh)), Push::Appended(40));
        assert_eq!(s.rows(), vh + 80);

        let out = s.finish();
        assert_eq!(out.width(), 64);
        assert_eq!(out.height(), vh + 80);
    }

    #[test]
    fn a_duplicate_frame_adds_no_rows() {
        let doc = document(64, 1200);
        let v = viewport(&doc, 0, 600);
        let mut s = Stitcher::new(&v);
        assert_eq!(s.push(&v), Push::Duplicate);
        assert_eq!(s.rows(), 600);
        assert_eq!(s.stats().duplicates, 1);
    }

    #[test]
    fn an_unmatched_frame_is_rejected_and_keeps_the_anchor() {
        // The only evidence the reject path works: the spike never triggered it.
        let doc = document(64, 1200);
        let mut s = Stitcher::new(&viewport(&doc, 0, 600));

        // Flat grey shares no structure with the textured document, so no offset
        // can score anywhere near the threshold.
        let alien = RgbaImage::from_pixel(64, 600, image::Rgba([128, 128, 128, 255]));
        assert_eq!(s.push(&alien), Push::Rejected);
        assert_eq!(s.rows(), 600, "a rejected frame contributes nothing");

        // And the anchor was held, so the next good frame still matches the view
        // from before the reject rather than the garbage in between.
        assert_eq!(s.push(&viewport(&doc, 40, 600)), Push::Appended(40));
        assert_eq!(s.stats().rejected, 1);
    }

    #[test]
    fn a_resized_frame_is_rejected_rather_than_matched() {
        let doc = document(64, 1200);
        let mut s = Stitcher::new(&viewport(&doc, 0, 600));
        let narrow = document(48, 600);
        assert_eq!(s.push(&narrow), Push::Rejected);
        assert_eq!(s.rows(), 600);
    }

    #[test]
    fn the_height_cap_stops_accumulation() {
        // Small frames, so the cap arrives after a few hundred cheap matches.
        let (w, vh, step) = (32u32, 64u32, 30u32);
        let doc = document(w, MAX_ACC_ROWS + 200);
        let mut s = Stitcher::new(&viewport(&doc, 0, vh));

        let mut top = 0;
        let mut hit_cap = false;
        while top + step + vh < doc.height() {
            top += step;
            if s.push(&viewport(&doc, top, vh)) == Push::Full {
                hit_cap = true;
                break;
            }
        }

        assert!(hit_cap, "the cap should be reached; stopped at {}", s.rows());
        assert!(s.rows() <= MAX_ACC_ROWS, "never exceeds the cap");
        // Once full it stays full, rather than resuming on the next frame.
        assert_eq!(s.push(&viewport(&doc, top + step, vh)), Push::Full);
    }

    #[test]
    fn physical_pixels_convert_to_logical_coordinates() {
        let rect = Rect {
            x: 100,
            y: 200,
            width: 640,
            height: 480,
        };
        // Scale 1: the two spaces coincide.
        assert_eq!(physical_to_logical(&rect, 1), (100, 200, 640, 480));
        // Scale 2: the author's display. Halved throughout.
        assert_eq!(physical_to_logical(&rect, 2), (50, 100, 320, 240));
        // A degenerate scale must not divide by zero or produce an empty region.
        assert_eq!(physical_to_logical(&rect, 0), (100, 200, 640, 480));
    }
}
