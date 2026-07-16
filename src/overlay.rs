// Region-selection overlay: a wlr-layer-shell surface covering the output,
// showing a frozen grab of the screen, rendered with tiny-skia. Selection
// input, dimming, crosshair and magnifier are layered on in later tasks.

use std::fs::File;
use std::os::fd::AsFd;

use image::RgbaImage;
use memmap2::MmapMut;
use rustix::fs::{MemfdFlags, ftruncate, memfd_create};
use tiny_skia::{Paint, Pixmap, Transform};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{
    Layer, ZwlrLayerShellV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1,
};

/// evdev keycode for Escape (raw code carried by wl_keyboard.key).
const KEY_ESC: u32 = 1;
/// evdev/linux button code for the left mouse button.
const BTN_LEFT: u32 = 0x110;

// Magnifier loupe geometry (physical pixels).
const MAG_SRC: i32 = 17; // source sample width/height in pixels (odd → true center)
const MAG_ZOOM: i32 = 8; // magnification factor
const MAG_SIDE: i32 = MAG_SRC * MAG_ZOOM; // on-screen loupe size
const MAG_OFFSET: i32 = 32; // gap between cursor and loupe

// Crosshair marker geometry (physical pixels).
const CROSS_ARM: f32 = 16.0; // arm length from the centre
const CROSS_THICK: f32 = 2.0;

/// Smallest selection worth capturing, per side, in physical pixels. A bare
/// click yields 0×0, which would otherwise reach the PNG encoder; a twitch
/// during a click yields a few pixels of noise. Well below any deliberate
/// selection — a single character is ~20 physical pixels wide at scale 2.
const MIN_SELECTION: u32 = 8;

/// A selected rectangle in physical pixels, ready to crop from the grab.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Whether this is a real selection rather than a stray click.
    fn is_usable(&self) -> bool {
        self.width >= MIN_SELECTION && self.height >= MIN_SELECTION
    }
}

struct Overlay {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    scale: i32,
    phys_w: u32,
    phys_h: u32,
    base: Pixmap,   // the frozen grab, immutable source
    dimmed: Pixmap, // base with the dim applied once, reused every frame
    canvas: Pixmap, // scratch: dimmed + selection + crosshair, rebuilt per frame

    // Selection state, in logical (surface-local) coordinates.
    pointer_pos: (f64, f64),
    anchor: Option<(f64, f64)>, // drag start while the left button is held
    selection: Option<Rect>,    // physical-pixel result, set on release

    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    mmap: Option<MmapMut>,
    configured: bool,
    dirty: bool,         // pointer state changed; a redraw is wanted
    frame_pending: bool, // a frame is committed and awaiting the compositor's callback
    running: bool,
}

impl Overlay {
    /// Compose the frame (grab → dim → un-dim selection → crosshair) and commit
    /// it. tiny-skia stores premultiplied RGBA; wl_shm Argb8888 is little-endian
    /// BGRA — so swap R and B on the way out.
    fn render(&mut self, qh: &QueueHandle<Self>) {
        if self.mmap.is_none() || self.surface.is_none() || self.buffer.is_none() {
            return;
        }
        let s = self.scale as f32;
        let ident = Transform::identity();

        // Fresh canvas from the pre-dimmed backdrop (no per-frame alpha blend).
        self.canvas.data_mut().copy_from_slice(self.dimmed.data());

        // Restore original brightness inside the selection and outline it.
        if let Some(sel) = self.drag_rect() {
            restore_rect(&mut self.canvas, &self.base, sel);
            draw_border(&mut self.canvas, sel);
        }

        // A `+` marker centred on the cursor (physical pixels).
        let (cx, cy) = (self.pointer_pos.0 as f32 * s, self.pointer_pos.1 as f32 * s);
        let mut cross = Paint::default();
        cross.set_color_rgba8(255, 255, 255, 180);
        let arms = [
            tiny_skia::Rect::from_xywh(
                cx - CROSS_ARM,
                cy - CROSS_THICK / 2.0,
                CROSS_ARM * 2.0,
                CROSS_THICK,
            ),
            tiny_skia::Rect::from_xywh(
                cx - CROSS_THICK / 2.0,
                cy - CROSS_ARM,
                CROSS_THICK,
                CROSS_ARM * 2.0,
            ),
        ];
        for r in arms.into_iter().flatten() {
            self.canvas.fill_rect(r, &cross, ident, None);
        }

        // Magnifier loupe around the cursor.
        draw_magnifier(
            &mut self.canvas,
            &self.base,
            cx as i32,
            cy as i32,
            self.phys_w as i32,
            self.phys_h as i32,
        );

        // Blit to shm.
        let mmap = self.mmap.as_mut().unwrap();
        for (dst, px) in mmap.chunks_exact_mut(4).zip(self.canvas.data().chunks_exact(4)) {
            dst[0] = px[2]; // B
            dst[1] = px[1]; // G
            dst[2] = px[0]; // R
            dst[3] = px[3]; // A
        }
        let surface = self.surface.as_ref().unwrap();
        surface.attach(self.buffer.as_ref(), 0, 0);
        surface.set_buffer_scale(self.scale);
        surface.damage_buffer(0, 0, self.phys_w as i32, self.phys_h as i32);
        // Ask to be told when this frame is presented, so we pace to the
        // display instead of to the (much faster) pointer event stream.
        surface.frame(qh, ());
        surface.commit();
        self.frame_pending = true;
    }

    /// The current drag rectangle in physical pixels, mapping logical
    /// coordinates through the output scale and clamping to the grab bounds.
    fn drag_rect(&self) -> Option<Rect> {
        let (ax, ay) = self.anchor?;
        let (cx, cy) = self.pointer_pos;
        let s = self.scale as f64;
        let x0 = (ax.min(cx) * s).round().clamp(0.0, self.phys_w as f64);
        let y0 = (ay.min(cy) * s).round().clamp(0.0, self.phys_h as f64);
        let x1 = (ax.max(cx) * s).round().clamp(0.0, self.phys_w as f64);
        let y1 = (ay.max(cy) * s).round().clamp(0.0, self.phys_h as f64);
        Some(Rect {
            x: x0 as u32,
            y: y0 as u32,
            width: (x1 - x0) as u32,
            height: (y1 - y0) as u32,
        })
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for Overlay {
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
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => {
                    state.shm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    let _: wl_seat::WlSeat = registry.bind(name, version.min(5), qh, ());
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// The seat tells us which input devices exist; grab the keyboard when offered.
impl Dispatch<wl_seat::WlSeat, ()> for Overlay {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

// Pointer drives selection: enter/motion track the cursor (logical coords),
// left press anchors the drag, left release commits the rectangle and exits.
impl Dispatch<wl_pointer::WlPointer, ()> for Overlay {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_pos = (surface_x, surface_y);
                state.dirty = true;
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(btn_state),
                ..
            } if button == BTN_LEFT => match btn_state {
                wl_pointer::ButtonState::Pressed => {
                    state.anchor = Some(state.pointer_pos);
                    state.dirty = true;
                }
                wl_pointer::ButtonState::Released => {
                    // Degenerate drags cancel: `None` here means no file and no
                    // clipboard, same as Escape.
                    state.selection = state.drag_rect().filter(Rect::is_usable);
                    state.anchor = None;
                    state.running = false;
                }
                _ => {}
            },
            _ => {}
        }
    }
}

// Only Escape matters for now: cancel the overlay.
impl Dispatch<wl_keyboard::WlKeyboard, ()> for Overlay {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
        {
            if key == KEY_ESC {
                state.running = false;
            }
        }
    }
}

// The frame callback fires when the compositor has presented our last commit —
// our cue that it's time to draw the next frame.
impl Dispatch<wl_callback::WlCallback, ()> for Overlay {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_pending = false;
        }
    }
}

// The layer surface tells us our size and when to draw; it can also be closed
// by the compositor.
impl Dispatch<ZwlrLayerSurfaceV1, ()> for Overlay {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                layer_surface.ack_configure(serial);
                state.configured = true;
                state.dirty = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.running = false,
            _ => {}
        }
    }
}

delegate_noop!(Overlay: ignore wl_compositor::WlCompositor);
delegate_noop!(Overlay: ignore wl_shm::WlShm);
delegate_noop!(Overlay: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Overlay: ignore wl_buffer::WlBuffer);
delegate_noop!(Overlay: ignore wl_surface::WlSurface);
delegate_noop!(Overlay: ignore ZwlrLayerShellV1);

/// Build the base canvas from the grab. Opaque pixels, so premultiplied and
/// straight RGBA coincide — a direct copy.
fn base_pixmap(grab: &RgbaImage) -> Pixmap {
    let mut pm = Pixmap::new(grab.width(), grab.height()).expect("pixmap alloc");
    pm.data_mut().copy_from_slice(grab.as_raw());
    pm
}

/// The grab with a uniform dim applied once, up front.
fn dimmed_pixmap(base: &Pixmap) -> Pixmap {
    let mut pm = base.clone();
    let mut dim = Paint::default();
    dim.set_color_rgba8(0, 0, 0, 120);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, pm.width() as f32, pm.height() as f32) {
        pm.fill_rect(r, &dim, Transform::identity(), None);
    }
    pm
}

/// Copy the grab's original (un-dimmed) pixels into `canvas` within `sel`.
fn restore_rect(canvas: &mut Pixmap, base: &Pixmap, sel: Rect) {
    let w = base.width() as usize;
    let cdata = canvas.data_mut();
    let bdata = base.data();
    for row in sel.y..sel.y + sel.height {
        let start = (row as usize * w + sel.x as usize) * 4;
        let len = sel.width as usize * 4;
        cdata[start..start + len].copy_from_slice(&bdata[start..start + len]);
    }
}

/// Stroke a rectangle outline of thickness `t` in `color` (four thin rects).
fn stroke_rect(canvas: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, t: f32, color: [u8; 4]) {
    let mut p = Paint::default();
    p.set_color_rgba8(color[0], color[1], color[2], color[3]);
    let sides = [
        tiny_skia::Rect::from_xywh(x, y, w, t),         // top
        tiny_skia::Rect::from_xywh(x, y + h - t, w, t), // bottom
        tiny_skia::Rect::from_xywh(x, y, t, h),         // left
        tiny_skia::Rect::from_xywh(x + w - t, y, t, h), // right
    ];
    for r in sides.into_iter().flatten() {
        canvas.fill_rect(r, &p, Transform::identity(), None);
    }
}

const ROSE: [u8; 4] = [235, 188, 186, 255];
const GOLD: [u8; 4] = [246, 193, 119, 255];

/// Outline `sel` with a 2px rose border.
fn draw_border(canvas: &mut Pixmap, sel: Rect) {
    stroke_rect(
        canvas,
        sel.x as f32,
        sel.y as f32,
        sel.width as f32,
        sel.height as f32,
        2.0,
        ROSE,
    );
}

/// Draw the magnifier loupe: a zoomed nearest-neighbour view of the grab around
/// the cursor, placed beside it (flipped away from screen edges), with a rose
/// frame and a gold box marking the exact target pixel.
fn draw_magnifier(canvas: &mut Pixmap, base: &Pixmap, cx: i32, cy: i32, pw: i32, ph: i32) {
    // Position the loupe near the cursor, flipping to the other side when it
    // would run off the right/bottom edge, then clamp fully on-screen.
    let mut ox = cx + MAG_OFFSET;
    let mut oy = cy + MAG_OFFSET;
    if ox + MAG_SIDE > pw {
        ox = cx - MAG_OFFSET - MAG_SIDE;
    }
    if oy + MAG_SIDE > ph {
        oy = cy - MAG_OFFSET - MAG_SIDE;
    }
    ox = ox.clamp(0, pw - MAG_SIDE);
    oy = oy.clamp(0, ph - MAG_SIDE);

    let src_x0 = cx - MAG_SRC / 2;
    let src_y0 = cy - MAG_SRC / 2;
    let (bw, bh) = (base.width() as i32, base.height() as i32);
    let cw = canvas.width() as i32;

    // Nearest-neighbour zoom, sampling the un-dimmed grab (clamped at edges).
    {
        let bdata = base.data();
        let cdata = canvas.data_mut();
        for ly in 0..MAG_SIDE {
            let sy = (src_y0 + ly / MAG_ZOOM).clamp(0, bh - 1);
            for lx in 0..MAG_SIDE {
                let sx = (src_x0 + lx / MAG_ZOOM).clamp(0, bw - 1);
                let si = ((sy * bw + sx) * 4) as usize;
                let di = (((oy + ly) * cw + (ox + lx)) * 4) as usize;
                cdata[di..di + 4].copy_from_slice(&bdata[si..si + 4]);
            }
        }
    }

    // Gold box around the exact centre pixel, then the rose frame.
    let cell = (MAG_SIDE - MAG_ZOOM) / 2;
    stroke_rect(
        canvas,
        (ox + cell) as f32,
        (oy + cell) as f32,
        MAG_ZOOM as f32,
        MAG_ZOOM as f32,
        2.0,
        GOLD,
    );
    stroke_rect(
        canvas,
        ox as f32,
        oy as f32,
        MAG_SIDE as f32,
        MAG_SIDE as f32,
        2.0,
        ROSE,
    );
}

/// Show the selection overlay over `grab`. Returns the selected rectangle, or
/// `None` if cancelled. (Task 2: displays the backdrop and exits on Escape;
/// selection input is wired up in later tasks.)
pub fn select_region(grab: &RgbaImage, scale: i32) -> Option<Rect> {
    let conn = Connection::connect_to_env().expect("Wayland connect failed");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let (phys_w, phys_h) = (grab.width(), grab.height());
    let base = base_pixmap(grab);
    let dimmed = dimmed_pixmap(&base);
    let mut ov = Overlay {
        compositor: None,
        shm: None,
        layer_shell: None,
        keyboard: None,
        pointer: None,
        scale,
        phys_w,
        phys_h,
        base,
        dimmed,
        canvas: Pixmap::new(phys_w, phys_h).expect("pixmap alloc"),
        pointer_pos: (0.0, 0.0),
        anchor: None,
        selection: None,
        surface: None,
        buffer: None,
        mmap: None,
        configured: false,
        dirty: false,
        frame_pending: false,
        running: true,
    };
    queue.roundtrip(&mut ov).expect("registry roundtrip");

    let compositor = ov.compositor.clone().expect("no wl_compositor");
    let shm = ov.shm.clone().expect("no wl_shm");
    let layer_shell = ov.layer_shell.clone().expect("no zwlr_layer_shell_v1");

    // Allocate the shm buffer at physical resolution.
    let size = (phys_w * phys_h * 4) as usize;
    let fd = memfd_create("frame-overlay", MemfdFlags::empty()).expect("memfd");
    ftruncate(&fd, size as u64).expect("ftruncate");
    let file = File::from(fd);
    let mmap = unsafe { MmapMut::map_mut(&file).expect("mmap") };
    let pool = shm.create_pool(file.as_fd(), size as i32, &qh, ());
    let buffer = pool.create_buffer(
        0,
        phys_w as i32,
        phys_h as i32,
        (phys_w * 4) as i32,
        wl_shm::Format::Argb8888,
        &qh,
        (),
    );

    // Create the layer surface filling the output, grabbing the keyboard so
    // Escape reaches us.
    let surface = compositor.create_surface(&qh, ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None, // let the compositor pick the output
        Layer::Overlay,
        "frame-region".to_string(),
        &qh,
        (),
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_exclusive_zone(-1);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer_surface.set_size(0, 0);
    surface.commit();

    ov.surface = Some(surface);
    ov.buffer = Some(buffer);
    ov.mmap = Some(mmap);

    // Pace rendering to the compositor's frame callbacks: handlers only update
    // state and mark `dirty`; we redraw the latest state at most once per
    // presented frame, so pointer events can never back up a render queue.
    while ov.running {
        queue.blocking_dispatch(&mut ov).expect("dispatch");
        if ov.dirty && !ov.frame_pending {
            ov.render(&qh);
            ov.dirty = false;
        }
    }
    ov.selection
}
