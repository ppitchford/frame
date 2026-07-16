// frame — Wayland-native screenshot and annotation tool for MangoWM.

mod capture;
mod output;
mod overlay;

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
/// grab, crop it, and save + copy.
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

    match output::save_and_copy(&cropped) {
        Ok(path) => println!(
            "saved {} ({}x{}) and copied to clipboard",
            path.display(),
            cropped.width(),
            cropped.height()
        ),
        Err(e) => {
            eprintln!("output failed: {e}");
            std::process::exit(1);
        }
    }
}
