// Screen capture via wlr-screencopy. Carried over from the scrolling-capture
// spike (see git history + SPIKE-FINDINGS.md), now a real module: a
// self-contained full-output grab used as the frozen backdrop for selection.

use std::fs::File;
use std::os::fd::AsFd;

use image::RgbaImage;
use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{
    self, ZwlrScreencopyFrameV1,
};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;

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
/// proxy's events into this one struct.
#[derive(Default)]
struct CaptureApp {
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    screencopy: Option<ZwlrScreencopyManagerV1>,
    scale: i32,

    pending_buffer: Option<BufferSpec>,
    buffer_done: bool,
    frame_ready: bool,
    frame_failed: bool,
    y_invert: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for CaptureApp {
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
                        // v2+ for the `scale` event.
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

// We care about one wl_output event: the integer scale factor, needed later to
// map logical pointer coordinates onto this physical-pixel grab.
impl Dispatch<wl_output::WlOutput, ()> for CaptureApp {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            state.scale = factor;
        }
    }
}

// The screencopy frame drives the handshake: `buffer` (maybe several) →
// `buffer_done` → we copy → `ready` on success or `failed` on error.
impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureApp {
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

delegate_noop!(CaptureApp: ignore wl_shm::WlShm);
delegate_noop!(CaptureApp: ignore ZwlrScreencopyManagerV1);
delegate_noop!(CaptureApp: ignore wl_shm_pool::WlShmPool);
delegate_noop!(CaptureApp: ignore wl_buffer::WlBuffer);

/// Grab the entire output into an RGBA image. Returns the grab and the output's
/// integer scale factor. Self-contained: opens and closes its own Wayland
/// connection.
pub fn capture_full_output() -> Result<(RgbaImage, i32), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("Wayland connect failed: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut app = CaptureApp {
        scale: 1,
        ..Default::default()
    };
    // Two roundtrips: the first surfaces the globals, the second lets wl_output
    // deliver its `scale` event after we've bound it.
    queue.roundtrip(&mut app).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut app).map_err(|e| e.to_string())?;

    let manager = app
        .screencopy
        .clone()
        .ok_or("compositor does not advertise zwlr_screencopy_manager_v1")?;
    let output = app.output.clone().ok_or("no wl_output")?;

    let frame = manager.capture_output(0, &output, &qh, ());

    while !app.buffer_done && !app.frame_failed {
        queue.blocking_dispatch(&mut app).map_err(|e| e.to_string())?;
    }
    if app.frame_failed {
        return Err("compositor sent `failed` before buffer_done".into());
    }
    let spec = app.pending_buffer.ok_or("no shm buffer offer")?;

    let size = (spec.stride * spec.height) as usize;
    let fd = memfd_create("frame-grab", MemfdFlags::empty()).map_err(|e| e.to_string())?;
    ftruncate(&fd, size as u64).map_err(|e| e.to_string())?;
    let file = File::from(fd);
    let mut mmap = unsafe { MmapMut::map_mut(&file).map_err(|e| e.to_string())? };

    let shm = app.shm.clone().ok_or("no wl_shm")?;
    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        spec.width as i32,
        spec.height as i32,
        spec.stride as i32,
        spec.format,
        &qh,
        (),
    );

    frame.copy(&buffer);
    while !app.frame_ready && !app.frame_failed {
        queue.blocking_dispatch(&mut app).map_err(|e| e.to_string())?;
    }
    if app.frame_failed {
        return Err("compositor sent `failed` during copy".into());
    }

    let img = buffer_to_rgba(&mut mmap, spec, app.y_invert);
    buffer.destroy();
    pool.destroy();
    frame.destroy();
    conn.flush().ok();

    Ok((img, app.scale))
}

/// Convert the shm buffer to an `RgbaImage`. wlr-screencopy hands us
/// {X,A}RGB8888 which, little-endian in memory, is byte order B,G,R,A.
fn buffer_to_rgba(mmap: &mut MmapMut, spec: BufferSpec, y_invert: bool) -> RgbaImage {
    let (w, h, stride) = (spec.width, spec.height, spec.stride as usize);
    let opaque = matches!(spec.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Xbgr8888);
    let bgr = matches!(spec.format, wl_shm::Format::Xrgb8888 | wl_shm::Format::Argb8888);

    let mut img = RgbaImage::new(w, h);
    for row in 0..h {
        let src_row = if y_invert { h - 1 - row } else { row };
        let base = src_row as usize * stride;
        for col in 0..w {
            let p = base + col as usize * 4;
            let (b0, b1, b2, b3) = (mmap[p], mmap[p + 1], mmap[p + 2], mmap[p + 3]);
            let (r, g, b) = if bgr { (b2, b1, b0) } else { (b0, b1, b2) };
            let a = if opaque { 255 } else { b3 };
            img.put_pixel(col, row, image::Rgba([r, g, b, a]));
        }
    }
    img
}
