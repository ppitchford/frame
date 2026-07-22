// A passive outline marking the region a scroll capture is reading.
//
// Without it, a running capture looks exactly like nothing having happened —
// which is what made the start/stop toggle feel broken during verification.
//
// **Four separate bars, not one bordered rectangle.** `zwlr_screencopy` copies
// the composited output, so anything drawn over the region lands in every
// captured frame. A single surface with a transparent middle *should* composite
// away to nothing, but "should" is carrying a premultiplied-alpha assumption
// whose failure mode is a silent tint on every capture. Four opaque bars sitting
// strictly outside the region cannot contaminate it at all.
//
// Separate from `overlay.rs` deliberately: that module is an interactive event
// loop with its own exit paths, while this is static — create it, draw once,
// leave it, tear it down.

use std::fs::File;
use std::os::fd::AsFd;

use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

/// Outline thickness, in logical pixels.
const THICKNESS: i32 = 3;

/// Rosé Pine love — reads as "live" without being alarming.
const COLOR: (u8, u8, u8) = (0xeb, 0x6f, 0x92);

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    outputs: Vec<(u32, wl_output::WlOutput, Option<String>)>,
}

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
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_shm" => state.shm = Some(registry.bind(name, version.min(1), qh, ())),
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()))
                }
                "wl_output" => {
                    let proxy = registry.bind(name, version.min(4), qh, name);
                    state.outputs.push((name, proxy, None));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for App {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        registry_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && let Some(entry) = state.outputs.iter_mut().find(|o| o.0 == *registry_name)
        {
            entry.2 = Some(name);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        _: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Acknowledge and otherwise ignore. The surface never resizes and never
        // needs redrawing, so there is no state to keep.
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            layer_surface.ack_configure(serial);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_buffer::WlBuffer);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore wl_region::WlRegion);
delegate_noop!(App: ignore ZwlrLayerShellV1);

/// One bar of the outline. Held only to keep it alive and to tear it down.
struct Bar {
    surface: wl_surface::WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    buffer: wl_buffer::WlBuffer,
    pool: wl_shm_pool::WlShmPool,
    _mmap: MmapMut,
}

/// A live region outline. Dropping it removes the outline from the screen.
pub struct Indicator {
    conn: Connection,
    bars: Vec<Bar>,
}

impl Indicator {
    /// Outline the logical rectangle `(x, y, w, h)` on `target`.
    ///
    /// Returns `Err` if the compositor is missing anything needed. Callers treat
    /// that as cosmetic — a capture without an outline is still a capture.
    pub fn show(
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        scale: i32,
        target: Option<&str>,
    ) -> Result<Indicator, String> {
        let conn =
            Connection::connect_to_env().map_err(|e| format!("Wayland connect failed: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());

        let mut app = App::default();
        queue.roundtrip(&mut app).map_err(|e| e.to_string())?;
        queue.roundtrip(&mut app).map_err(|e| e.to_string())?;

        let compositor = app.compositor.clone().ok_or("no wl_compositor")?;
        let shm = app.shm.clone().ok_or("no wl_shm")?;
        let layer_shell = app
            .layer_shell
            .clone()
            .ok_or("compositor does not advertise zwlr_layer_shell_v1")?;
        let output = target
            .and_then(|t| app.outputs.iter().find(|o| o.2.as_deref() == Some(t)))
            .map(|o| o.1.clone());

        let t = THICKNESS;
        // Each bar sits strictly outside the region: the top and bottom run the
        // full width plus both corners, the sides fill the gap between them.
        let rects = [
            (x - t, y - t, w + 2 * t, t), // top
            (x - t, y + h, w + 2 * t, t), // bottom
            (x - t, y, t, h),             // left
            (x + w, y, t, h),             // right
        ];

        let mut bars = Vec::new();
        for (bx, by, bw, bh) in rects {
            // A region against a screen edge pushes a bar off it; clamping the
            // origin keeps the rest visible rather than dropping the bar.
            let (bx, by) = (bx.max(0), by.max(0));
            if bw <= 0 || bh <= 0 {
                continue;
            }
            bars.push(make_bar(
                &compositor,
                &shm,
                &layer_shell,
                output.as_ref(),
                &qh,
                bx,
                by,
                bw,
                bh,
                scale,
            )?);
        }

        // One roundtrip so every surface is configured before we attach.
        queue.roundtrip(&mut app).map_err(|e| e.to_string())?;

        for bar in &bars {
            bar.surface.attach(Some(&bar.buffer), 0, 0);
            bar.surface.damage(0, 0, i32::MAX, i32::MAX);
            bar.surface.commit();
        }
        conn.flush().ok();

        Ok(Indicator { conn, bars })
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        for bar in &self.bars {
            bar.layer_surface.destroy();
            bar.surface.destroy();
            bar.buffer.destroy();
            bar.pool.destroy();
        }
        // Push the teardown out before the process moves on, or the outline can
        // outlive the capture on screen.
        self.conn.flush().ok();
    }
}

#[allow(clippy::too_many_arguments)]
fn make_bar(
    compositor: &wl_compositor::WlCompositor,
    shm: &wl_shm::WlShm,
    layer_shell: &ZwlrLayerShellV1,
    output: Option<&wl_output::WlOutput>,
    qh: &QueueHandle<App>,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    scale: i32,
) -> Result<Bar, String> {
    let s = scale.max(1);
    let (pw, ph) = (w * s, h * s);
    let stride = pw * 4;
    let size = (stride * ph) as usize;

    let fd = memfd_create("frame-indicator", MemfdFlags::empty()).map_err(|e| e.to_string())?;
    ftruncate(&fd, size as u64).map_err(|e| e.to_string())?;
    let file = File::from(fd);
    let mut mmap = unsafe { MmapMut::map_mut(&file).map_err(|e| e.to_string())? };

    // ARGB8888 is byte order B,G,R,A little-endian. Opaque, so premultiplied and
    // straight coincide and the fill is a plain repeat.
    let (r, g, b) = COLOR;
    for px in mmap.chunks_exact_mut(4) {
        px.copy_from_slice(&[b, g, r, 255]);
    }

    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(0, pw, ph, stride, wl_shm::Format::Argb8888, qh, ());

    let surface = compositor.create_surface(qh, ());
    let layer_surface =
        layer_shell.get_layer_surface(&surface, output, Layer::Overlay, "frame-scroll".into(), qh, ());

    // Anchored to the top-left corner and pushed into place by margins, which is
    // how layer-shell expresses an absolute position.
    layer_surface.set_anchor(Anchor::Top | Anchor::Left);
    layer_surface.set_margin(y, 0, 0, x);
    layer_surface.set_size(w as u32, h as u32);
    layer_surface.set_exclusive_zone(-1);
    // Never take the keyboard: the stop key has to keep reaching the compositor.
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

    // An empty input region, so clicks and scrolls pass through to the window
    // being captured underneath.
    let region = compositor.create_region(qh, ());
    surface.set_input_region(Some(&region));
    region.destroy();

    surface.set_buffer_scale(s);
    surface.commit();

    Ok(Bar {
        surface,
        layer_surface,
        buffer,
        pool,
        _mmap: mmap,
    })
}
