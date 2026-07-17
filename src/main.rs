// frame — Wayland-native screenshot and annotation tool for MangoWM.

mod capture;
mod output;
mod overlay;
mod qao;

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("region") => region_capture(),
        Some(output::SERVE_ARG) => output::serve_clipboard(),
        _ => {
            eprintln!("usage: frame region");
            std::process::exit(2);
        }
    }
}

/// Interactive region capture: grab the output, select a region over the frozen
/// grab, crop it, and hand it to the Quick Access Overlay. Saving and copying are
/// the overlay's buttons — this command writes nothing on its own.
fn region_capture() {
    let (grab, scale) = capture::capture_full_output().unwrap_or_else(|e| {
        eprintln!("capture failed: {e}");
        std::process::exit(1);
    });

    let Some(rect) = overlay::select_region(&grab, scale) else {
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
