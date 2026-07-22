// frame — Wayland-native screenshot and annotation tool for MangoWM.

mod capture;
mod editor;
mod indicator;
mod monitor;
mod output;
mod overlay;
mod qao;
mod scroll;
mod sock;
mod window;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("region") => region_capture(),
        Some("full") => full_capture(),
        Some("window") => window_capture(),
        Some("scroll") => scroll_capture(),
        Some(output::SERVE_ARG) => output::serve_clipboard(),
        _ => {
            eprintln!("usage: frame <region|full|window|scroll>");
            std::process::exit(2);
        }
    }
}

/// Interactive region capture: grab the active output, select a region over the
/// frozen grab, crop it, and hand it to the Quick Access Overlay. Saving and
/// copying are the overlay's buttons — this command writes nothing on its own.
fn region_capture() {
    // Resolve the focused output once and use it for both the grab and the
    // overlay, so the frozen backdrop and the selection surface share an output.
    let target = monitor::active_output_name();
    let (grab, scale) = capture::capture_output(target.as_deref()).unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });

    let Some(rect) = overlay::select_region(&grab, scale, target.as_deref()) else {
        println!("cancelled");
        return;
    };

    let cropped =
        image::imageops::crop_imm(&grab, rect.x, rect.y, rect.width, rect.height).to_image();

    if let Err(e) = qao::show(cropped, scale) {
        eprintln!("overlay failed: {e}");
        std::process::exit(1);
    }
}

/// Fullscreen capture: grab the whole active output and hand it straight to the
/// Quick Access Overlay — no selection step. Like `region`, the overlay's
/// buttons own saving and copying; this command writes nothing on its own.
fn full_capture() {
    let target = monitor::active_output_name();
    let (grab, scale) = capture::capture_output(target.as_deref()).unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });

    if let Err(e) = qao::show(grab, scale) {
        eprintln!("overlay failed: {e}");
        std::process::exit(1);
    }
}

/// Scrolling capture. Select a region, scroll the real window underneath it, and
/// press the same key again to stop; the frames are stitched into one tall image
/// and handed to the Quick Access Overlay.
///
/// The command is a **toggle**, which is what lets one keybinding do both jobs:
/// if a `stop` reaches a listening session, this invocation's whole purpose was
/// to deliver it. Failing to connect is not an error — it is how we learn there
/// is no session, and therefore that we should become one.
fn scroll_capture() {
    if sock::send(sock::STOP).is_ok() {
        return;
    }

    let target = monitor::active_output_name();
    let (grab, scale) = capture::capture_output(target.as_deref()).unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });

    let Some(rect) = overlay::select_region(&grab, scale, target.as_deref()) else {
        println!("cancelled");
        return;
    };
    // The overlay owns the screen while selecting; drop the frozen backdrop
    // before capturing, because the point of this mode is that the user scrolls
    // the real window underneath.
    drop(grab);

    let (x, y, w, h) = scroll::physical_to_logical(&rect, scale);

    // Mark the region being read. Cosmetic, so a failure here is reported and
    // the capture continues — but without it a running capture is
    // indistinguishable from nothing having happened.
    let marker = match indicator::Indicator::show(x, y, w, h, scale, target.as_deref()) {
        Ok(marker) => Some(marker),
        Err(e) => {
            eprintln!("region indicator unavailable: {e}");
            None
        }
    };

    let mut session = capture::Session::open(target.as_deref()).unwrap_or_else(|e| {
        eprintln!("capture session failed: {e}");
        std::process::exit(1);
    });
    // Bound to the socket *before* the first frame, so the stop key works even
    // if the user changes their mind immediately.
    let bus = sock::Server::bind().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    let first = session.capture_region(x, y, w, h).unwrap_or_else(|e| {
        eprintln!("first frame failed: {e}");
        std::process::exit(1);
    });
    let mut stitcher = scroll::Stitcher::new(&first);

    println!("scroll now; press the same key again to stop");
    let budget = std::time::Duration::from_micros(1_000_000 / 30);

    loop {
        let tick = std::time::Instant::now();

        if bus.take_command().as_deref() == Some(sock::STOP) {
            break;
        }

        match session.capture_region(x, y, w, h) {
            // A dropped frame costs overlap, not the capture: the next frame is
            // matched against the same anchor and usually still lands.
            Err(e) => eprintln!("frame dropped: {e}"),
            Ok(frame) => {
                if stitcher.push(&frame) == scroll::Push::Full {
                    println!("height cap reached at {} px; stopping", stitcher.rows());
                    break;
                }
            }
        }

        // Pace to the frame budget, skipping the sleep if capture overran it.
        if let Some(rest) = budget.checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }

    // Take the outline down before the overlay appears, so it does not hang over
    // the screen while the capture is being reviewed.
    drop(marker);

    // Release the socket before the overlay opens. `qao::show` blocks for the
    // overlay's whole lifetime, and a bus still bound during it swallows the
    // next scroll keypress: the new process connects, `send` succeeds against a
    // server that will never read it, and it exits having started nothing.
    drop(bus);

    let stats = stitcher.stats();
    let image = stitcher.finish();
    println!(
        "stitched {}×{} — {} frames appended, {} duplicate, {} rejected",
        image.width(),
        image.height(),
        stats.appended,
        stats.duplicates,
        stats.rejected
    );

    if let Err(e) = qao::show(image, scale) {
        eprintln!("overlay failed: {e}");
        std::process::exit(1);
    }
}

/// Window capture: grab the active output, pick one window over the frozen grab,
/// crop to its rectangle and hand that to the Quick Access Overlay.
///
/// The crop comes out of a full-output grab because `zwlr_screencopy_v1` copies
/// outputs, not surfaces — so anything overlapping the chosen window is in the
/// shot. See `window.rs`.
fn window_capture() {
    let target = monitor::active_output_name();
    let (grab, scale) = capture::capture_output(target.as_deref()).unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });

    // Without the compositor's window list there are no rectangles to offer, so
    // say so rather than raising a picker that cannot be satisfied.
    let Some(name) = target.as_deref() else {
        eprintln!("no focused output; cannot enumerate windows");
        std::process::exit(1);
    };
    let windows = window::visible_windows(name, scale, grab.width(), grab.height());
    if windows.is_empty() {
        eprintln!("no capturable windows on {name}");
        std::process::exit(1);
    }

    let Some(rect) = overlay::select_window(&grab, scale, Some(name), windows) else {
        println!("cancelled");
        return;
    };

    let cropped =
        image::imageops::crop_imm(&grab, rect.x, rect.y, rect.width, rect.height).to_image();

    if let Err(e) = qao::show(cropped, scale) {
        eprintln!("overlay failed: {e}");
        std::process::exit(1);
    }
}
