// frame — scrolling-capture feasibility spike.
//
// Throwaway code. The deliverable is a go/no-go verdict on the
// wlr-screencopy + SAD-stitch loop, not production scaffolding.

use std::fs::File;
use std::io::BufRead;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use image::RgbaImage;
use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

const OUT_DIR: &str = "/tmp/frame-spike";
const TARGET_FPS: u64 = 30;

// SAD matcher tuning.
const STRIP_MAX: usize = 200; // bottom-strip height used as the match template
const COL_STEP: usize = 4; // subsample columns for speed
/// Cap the vertical search. Real per-frame scroll is tens of px at 30 fps;
/// searching the whole frame height is ~20× wasted work. A frame that scrolled
/// further than this is rejected as a gap — acceptable per the roadmap.
const MAX_SEARCH_OFFSET: usize = 300;
/// Matching only ever reads the bottom `strip + search` rows of a frame, so we
/// only convert that band to luma. Pure speedup — the matched pixels are
/// identical to converting the whole frame.
const BAND_ROWS: usize = STRIP_MAX + MAX_SEARCH_OFFSET;
/// Reject a match whose mean per-pixel luma difference exceeds this. Above it,
/// the frames don't overlap cleanly (scrolled too far, or content changed) —
/// better to drop the frame and accept a gap than stitch a bad seam. Tuned
/// against real captures in the spike.
const SAD_REJECT_THRESHOLD: f64 = 15.0;

/// A logical-coordinate rectangle to capture. Matches slurp's output units.
#[derive(Clone, Copy, Debug)]
struct Region {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// The buffer parameters the compositor asks us to allocate, learned from the
/// frame's `buffer` event.
#[derive(Clone, Copy)]
struct BufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

/// Globals plus per-capture scratch state. The event queue dispatches every
/// proxy's events into this one struct, so the capture handshake flags live
/// here too.
#[derive(Default)]
struct App {
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    screencopy: Option<ZwlrScreencopyManagerV1>,

    // Reset before each capture.
    pending_buffer: Option<BufferSpec>,
    buffer_done: bool,
    frame_ready: bool,
    frame_failed: bool,
    y_invert: bool,
}

impl App {
    fn reset_capture(&mut self) {
        self.pending_buffer = None;
        self.buffer_done = false;
        self.frame_ready = false;
        self.frame_failed = false;
        self.y_invert = false;
    }
}

// The registry is the one global we handle by hand: each `Global` event is a
// chance to bind an interface we care about.
impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    if state.output.is_none() {
                        state.output = Some(registry.bind(name, version.min(4), qh, ()));
                    }
                }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy = Some(registry.bind(name, version.min(3), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// The screencopy frame drives the handshake; this is the one non-trivial
// dispatch. Sequence: `buffer` (maybe several) → `buffer_done` → we copy →
// `ready` on success or `failed` on error.
impl Dispatch<ZwlrScreencopyFrameV1, ()> for App {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                // Only shm formats interest us; dmabuf offers arrive as a
                // separate event we ignore. Keep the first shm offer.
                if let WEnum::Value(format) = format {
                    if state.pending_buffer.is_none() {
                        state.pending_buffer = Some(BufferSpec {
                            format,
                            width,
                            height,
                            stride,
                        });
                    }
                }
            }
            Event::Flags { flags } => {
                if let WEnum::Value(flags) = flags {
                    state.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            Event::BufferDone => state.buffer_done = true,
            Event::Ready { .. } => state.frame_ready = true,
            Event::Failed => state.frame_failed = true,
            _ => {}
        }
    }
}

delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_output::WlOutput);
delegate_noop!(App: ignore ZwlrScreencopyManagerV1);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);

/// Capture one frame of `region` into an RGBA image. Blocks until the
/// compositor reports the frame ready (or failed).
fn capture_frame(
    app: &mut App,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<App>,
    qh: &QueueHandle<App>,
    region: Region,
) -> Result<RgbaImage, String> {
    app.reset_capture();

    let manager = app.screencopy.clone().unwrap();
    let output = app.output.clone().unwrap();
    let frame = manager.capture_output_region(
        0, // don't overlay the cursor
        &output,
        region.x,
        region.y,
        region.width,
        region.height,
        qh,
        (),
    );

    // Phase 1: learn the buffer spec.
    while !app.buffer_done && !app.frame_failed {
        queue
            .blocking_dispatch(app)
            .map_err(|e| format!("dispatch (buffer_done) failed: {e}"))?;
    }
    if app.frame_failed {
        return Err("compositor sent `failed` before buffer_done".into());
    }
    let spec = app
        .pending_buffer
        .ok_or("buffer_done without any shm buffer offer")?;

    // Phase 2: allocate matching shm, hand it over, ask for the copy.
    let size = (spec.stride * spec.height) as usize;
    let fd = memfd_create("frame-spike", MemfdFlags::empty())
        .map_err(|e| format!("memfd_create failed: {e}"))?;
    ftruncate(&fd, size as u64).map_err(|e| format!("ftruncate failed: {e}"))?;
    let file = File::from(fd);
    let mut mmap =
        unsafe { MmapMut::map_mut(&file).map_err(|e| format!("mmap failed: {e}"))? };

    let shm = app.shm.clone().unwrap();
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        spec.width as i32,
        spec.height as i32,
        spec.stride as i32,
        spec.format,
        qh,
        (),
    );

    frame.copy(&buffer);

    // Phase 3: wait for the pixels.
    while !app.frame_ready && !app.frame_failed {
        queue
            .blocking_dispatch(app)
            .map_err(|e| format!("dispatch (ready) failed: {e}"))?;
    }
    if app.frame_failed {
        return Err("compositor sent `failed` during copy".into());
    }

    let img = buffer_to_rgba(&mut mmap, spec, app.y_invert);

    // Clean up per-capture Wayland objects; the mmap/file drop with scope.
    buffer.destroy();
    pool.destroy();
    frame.destroy();
    conn.flush().ok();

    Ok(img)
}

/// Convert the shm buffer to an `RgbaImage`. wlr-screencopy hands us
/// {X,A}RGB8888 which, little-endian in memory, is byte order B,G,R,A.
fn buffer_to_rgba(mmap: &mut MmapMut, spec: BufferSpec, y_invert: bool) -> RgbaImage {
    let (w, h, stride) = (spec.width, spec.height, spec.stride as usize);
    let opaque = matches!(spec.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888);
    // Some compositors report *bgr* variants; handle the common two orderings.
    let bgr = matches!(spec.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);

    let mut img = RgbaImage::new(w, h);
    for row in 0..h {
        let src_row = if y_invert { h - 1 - row } else { row };
        let base = src_row as usize * stride;
        for col in 0..w {
            let p = base + col as usize * 4;
            let (b0, b1, b2, b3) = (mmap[p], mmap[p + 1], mmap[p + 2], mmap[p + 3]);
            let (r, g, b) = if bgr {
                (b2, b1, b0)
            } else {
                (b0, b1, b2)
            };
            let a = if opaque { 255 } else { b3 };
            img.put_pixel(col, row, image::Rgba([r, g, b, a]));
        }
    }
    img
}

/// A single-channel luma view of a frame, for matching. Rec.601 weights.
struct Luma {
    w: usize,
    h: usize,
    px: Vec<u8>,
}

fn to_luma(img: &RgbaImage) -> Luma {
    let (w, full_h) = (img.width() as usize, img.height() as usize);
    // Only the bottom band participates in matching; skip the rest.
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

/// Result of matching `next`'s content against the bottom strip of `prev`.
#[derive(Clone, Copy, Debug)]
struct Match {
    /// Pixels the content scrolled up between the two frames.
    offset: usize,
    /// Mean per-pixel luma difference at the best offset (0..255).
    mean_sad: f64,
}

/// Find how far the content scrolled between `prev` and `next` by sliding
/// `prev`'s bottom strip upward over `next` and minimising SAD. Forward scroll
/// only (offset ≥ 0); backward scroll is out of scope.
fn find_offset(prev: &Luma, next: &Luma) -> Match {
    debug_assert_eq!((prev.w, prev.h), (next.w, next.h));
    let (w, h) = (prev.w, prev.h);
    // Fixed-height template from the frame bottom; halve for tiny frames so a
    // search range remains.
    let strip_h = STRIP_MAX.min(h / 2).max(1);
    let max_offset = (h - strip_h).min(MAX_SEARCH_OFFSET);

    let mut best_sad = u64::MAX;
    let mut best_d = 0usize;

    // `prev` strip occupies rows [h - strip_h, h). At offset d, it aligns with
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
                // Early abort: this offset can't beat the incumbent.
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

/// Ask slurp for a region. It prints `x,y wxh` in logical coordinates, which
/// is exactly what `capture_output_region` wants. Throwaway stub for the real
/// selection overlay.
fn pick_region() -> Region {
    let out = std::process::Command::new("slurp")
        .output()
        .expect("failed to run slurp — is it installed?");
    if !out.status.success() {
        eprintln!("slurp cancelled; nothing selected.");
        std::process::exit(1);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    // Format: "100,200 600x400"
    let (pos, size) = text
        .split_once(' ')
        .unwrap_or_else(|| panic!("unexpected slurp output: {text:?}"));
    let (x, y) = pos
        .split_once(',')
        .unwrap_or_else(|| panic!("unexpected slurp position: {pos:?}"));
    let (w, h) = size
        .split_once('x')
        .unwrap_or_else(|| panic!("unexpected slurp size: {size:?}"));
    Region {
        x: x.parse().expect("bad slurp x"),
        y: y.parse().expect("bad slurp y"),
        width: w.parse().expect("bad slurp width"),
        height: h.parse().expect("bad slurp height"),
    }
}

/// Block until the user presses Enter on stdin.
fn wait_for_enter() {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .expect("failed to read stdin");
}

/// Capture frames at ~TARGET_FPS from `region` until the user presses Enter a
/// second time. Frames are kept in memory; disk writes happen after, so PNG
/// encoding doesn't throttle the capture rate.
fn run_capture_loop(
    app: &mut App,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<App>,
    qh: &QueueHandle<App>,
    region: Region,
) -> Vec<RgbaImage> {
    // A background thread flips `stop` when Enter is pressed again.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        std::thread::spawn(move || {
            wait_for_enter();
            stop.store(true, Ordering::Relaxed);
        });
    }

    let frame_budget = Duration::from_micros(1_000_000 / TARGET_FPS);
    let mut frames = Vec::new();
    let start = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        let tick = Instant::now();
        match capture_frame(app, conn, queue, qh, region) {
            Ok(img) => frames.push(img),
            Err(e) => {
                eprintln!("frame dropped: {e}");
            }
        }
        // Pace to the frame budget; skip the sleep if capture already overran.
        if let Some(rest) = frame_budget.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let fps = frames.len() as f64 / elapsed.max(f64::MIN_POSITIVE);
    println!(
        "Captured {} frames in {elapsed:.1}s (~{fps:.1} fps).",
        frames.len()
    );
    frames
}

#[derive(Default, Debug)]
struct StitchStats {
    accepted: usize,
    rejected: usize,
    duplicates: usize,
    appended_rows: usize,
}

/// Stitch a frame sequence into one tall image. Each frame is matched against
/// the last accepted frame; the non-overlapping bottom portion is appended.
fn stitch(frames: &[RgbaImage]) -> (RgbaImage, StitchStats) {
    let first = &frames[0];
    let (w, h) = (first.width() as usize, first.height() as usize);

    // Accumulate raw RGBA rows; build the image once at the end.
    let mut acc: Vec<u8> = first.as_raw().clone();
    let mut acc_rows = h;
    let mut prev_luma = to_luma(first);
    let mut stats = StitchStats::default();

    for (i, next) in frames.iter().enumerate().skip(1) {
        let next_luma = to_luma(next);
        let m = find_offset(&prev_luma, &next_luma);

        if m.mean_sad > SAD_REJECT_THRESHOLD {
            // No clean overlap — drop the frame, keep the anchor, accept a gap.
            stats.rejected += 1;
            println!("  frame {i:>3}: REJECT (offset {}, sad {:.1})", m.offset, m.mean_sad);
            continue;
        }
        if m.offset == 0 {
            // No new content since the last accepted frame.
            stats.duplicates += 1;
            prev_luma = next_luma;
            continue;
        }

        // Append the freshly-revealed bottom `offset` rows.
        let start = (h - m.offset) * w * 4;
        acc.extend_from_slice(&next.as_raw()[start..]);
        acc_rows += m.offset;
        stats.accepted += 1;
        stats.appended_rows += m.offset;
        println!("  frame {i:>3}: +{:>4} rows (sad {:.1})", m.offset, m.mean_sad);
        prev_luma = next_luma;
    }

    let img = RgbaImage::from_raw(w as u32, acc_rows as u32, acc)
        .expect("accumulated buffer size mismatch");
    (img, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a luma frame where every row is a flat value given by `f(row)`.
    fn frame(w: usize, h: usize, f: impl Fn(usize) -> u8) -> Luma {
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            let v = f(y);
            for _ in 0..w {
                px.push(v);
            }
        }
        Luma { w, h, px }
    }

    #[test]
    fn recovers_known_offset() {
        let (w, h, d) = (64usize, 100usize, 7usize);
        // Unique value per row so the bottom strip matches at exactly one offset.
        let prev = frame(w, h, |y| y as u8);
        // `next` = `prev` scrolled up by d; bottom d rows are fresh content.
        let next = frame(w, h, |y| {
            if y + d < h {
                (y + d) as u8
            } else {
                (200 + (y + d - h)) as u8
            }
        });
        let m = find_offset(&prev, &next);
        assert_eq!(m.offset, d, "expected offset {d}, got {}", m.offset);
        assert!(m.mean_sad < 0.5, "clean shift should have ~0 SAD, got {}", m.mean_sad);
    }

    #[test]
    fn static_frame_is_zero_offset() {
        let (w, h) = (64usize, 100usize);
        let img = frame(w, h, |y| (y * 3) as u8);
        let clone = frame(w, h, |y| (y * 3) as u8);
        let m = find_offset(&img, &clone);
        assert_eq!(m.offset, 0, "identical frames must not fabricate movement");
        assert!(m.mean_sad < 0.5);
    }
}

/// Throwaway: reload a saved `frame-*.png` sequence and re-run stitch, so the
/// matcher/stitch can be tuned and timed without a fresh Wayland capture.
fn restitch() {
    let mut paths: Vec<_> = std::fs::read_dir(OUT_DIR)
        .expect("no output dir to restitch from")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("frame-") && n.ends_with(".png"))
        })
        .collect();
    paths.sort();
    println!("Loading {} frames (untimed)…", paths.len());
    let frames: Vec<RgbaImage> = paths
        .iter()
        .map(|p| image::open(p).expect("failed to open frame").to_rgba8())
        .collect();
    if frames.is_empty() {
        eprintln!("No frames found in {OUT_DIR}.");
        std::process::exit(1);
    }

    println!("Stitching {} frames…", frames.len());
    let t = Instant::now();
    let (stitched, stats) = stitch(&frames);
    let path = format!("{OUT_DIR}/stitched.png");
    stitched.save(&path).expect("failed to write stitched PNG");
    let ms = t.elapsed().as_millis();
    println!(
        "Stitched {}x{} px in {ms} ms → {path}",
        stitched.width(),
        stitched.height()
    );
    println!(
        "  accepted {}, rejected {}, duplicates {}, appended {} rows",
        stats.accepted, stats.rejected, stats.duplicates, stats.appended_rows
    );
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("restitch") {
        restitch();
        return;
    }

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland display");
    let display = conn.display();

    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    display.get_registry(&qh, ());

    let mut app = App::default();
    queue
        .roundtrip(&mut app)
        .expect("registry roundtrip failed");

    if app.screencopy.is_none() {
        eprintln!(
            "FATAL: compositor does not advertise zwlr_screencopy_manager_v1.\n\
             The scrolling-capture approach depends on it. Aborting spike."
        );
        std::process::exit(1);
    }
    assert!(app.shm.is_some(), "compositor did not advertise wl_shm");
    assert!(app.output.is_some(), "compositor advertised no wl_output");

    // Fresh output dir each run so stale frames never mix into inspection.
    let _ = std::fs::remove_dir_all(OUT_DIR);
    std::fs::create_dir_all(OUT_DIR).expect("failed to create output dir");

    println!("Select the scroll viewport with slurp…");
    let region = pick_region();
    println!(
        "Region {}x{} at ({},{}). Press Enter to START capturing, then scroll.",
        region.width, region.height, region.x, region.y
    );
    wait_for_enter();
    println!("Capturing — scroll now. Press Enter again to STOP.");

    let frames = run_capture_loop(&mut app, &conn, &mut queue, &qh, region);
    if frames.is_empty() {
        eprintln!("No frames captured; nothing to stitch.");
        std::process::exit(1);
    }

    // Stitch — this is the stop-to-image path the 2s criterion measures, so
    // time it in isolation (before the inspection-only frame dump below).
    println!("Stitching {} frames…", frames.len());
    let t = Instant::now();
    let (stitched, stats) = stitch(&frames);
    let stitched_path = format!("{OUT_DIR}/stitched.png");
    stitched.save(&stitched_path).expect("failed to write stitched PNG");
    let stitch_ms = t.elapsed().as_millis();

    println!(
        "Stitched {}x{} px in {stitch_ms} ms → {stitched_path}",
        stitched.width(),
        stitched.height()
    );
    println!(
        "  accepted {}, rejected {}, duplicates {}, appended {} rows",
        stats.accepted, stats.rejected, stats.duplicates, stats.appended_rows
    );

    // Inspection-only: dump every raw frame. Untimed; not part of the pipeline.
    println!("Saving {} frames to {OUT_DIR}/ …", frames.len());
    for (i, img) in frames.iter().enumerate() {
        let path = format!("{OUT_DIR}/frame-{i:03}.png");
        img.save(&path).expect("failed to write frame PNG");
    }
    println!("Done.");
}
