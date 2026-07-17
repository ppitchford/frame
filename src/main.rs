// frame — Wayland-native screenshot and annotation tool for MangoWM.

mod capture;
mod monitor;
mod output;
mod overlay;
mod qao;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("region") => region_capture(),
        Some("full") => full_capture(),
        Some(output::SERVE_ARG) => output::serve_clipboard(),
        _ => {
            eprintln!("usage: frame <region|full>");
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
