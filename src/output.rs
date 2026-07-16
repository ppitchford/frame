// Output sinks for a finished capture: write a timestamped PNG and copy the
// same bytes to the Wayland clipboard.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbaImage};
use wl_clipboard_rs::copy::{MimeType, Options, Source};

/// Hidden subcommand: the re-exec'd child that owns the clipboard offer.
pub const SERVE_ARG: &str = "__serve-clipboard";

/// Handshake byte the helper writes back once it has claimed the selection.
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

/// Encode `img` to PNG, save it under the pictures directory, and copy it to
/// the clipboard. Returns the path written.
pub fn save_and_copy(img: &RgbaImage) -> Result<PathBuf, String> {
    let png = encode_png(img)?;

    let path = output_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create dir failed: {e}"))?;
    }
    std::fs::write(&path, &png).map_err(|e| format!("write PNG failed: {e}"))?;

    copy_to_clipboard(png)?;
    Ok(path)
}

fn encode_png(img: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(img.as_raw(), img.width(), img.height(), ExtendedColorType::Rgba8)
        .map_err(|e| format!("PNG encode failed: {e}"))?;
    Ok(png)
}

/// `$XDG_PICTURES_DIR/frame-<timestamp>.png`, falling back to `~/Pictures`.
fn output_path() -> PathBuf {
    let dir = std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME unset");
            PathBuf::from(home).join("Pictures")
        });
    let ts = jiff::Zoned::now().strftime("%Y-%m-%d-%H%M%S").to_string();
    dir.join(format!("frame-{ts}.png"))
}

/// Hand the PNG to a detached helper that outlives this process.
///
/// A Wayland clipboard offer is served live by a connected client, so it dies
/// with the process that makes it — `wl-clipboard-rs` serves from a thread, it
/// does not fork. `frame` exits right after a capture, so we re-exec ourselves
/// and let the child own the offer. Re-exec rather than `fork()` because the
/// Quick Access Overlay will copy from an `eframe` process, and forking a
/// multithreaded process can leave the child holding a locked allocator.
fn copy_to_clipboard(png: Vec<u8>) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe failed: {e}"))?;
    let mut child = Command::new(exe)
        .arg(SERVE_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn clipboard helper failed: {e}"))?;

    let mut stdin = child.stdin.take().expect("stdin piped");
    stdin
        .write_all(&png)
        .map_err(|e| format!("send PNG to clipboard helper failed: {e}"))?;
    drop(stdin); // EOF: the helper reads to end before claiming the selection.

    // Block until the helper reports it owns the offer, so a clipboard failure
    // is still reported by the process the user is watching. `child` is then
    // dropped without waiting — dropping a `Child` does not kill it, so the
    // helper keeps serving after we exit and init reaps it.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut status = [0u8; 1];
    match stdout.read_exact(&mut status) {
        Ok(()) if status[0] == STATUS_OK => Ok(()),
        Ok(()) => Err("clipboard helper could not claim the selection".to_string()),
        Err(e) => Err(format!("clipboard helper exited before serving: {e}")),
    }
}

/// The `__serve-clipboard` child: read a PNG from stdin, claim the selection,
/// and serve paste requests until another client takes the clipboard.
pub fn serve_clipboard() -> ! {
    let mut png = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut png) {
        eprintln!("clipboard helper: reading stdin failed: {e}");
        std::process::exit(1);
    }

    let mut opts = Options::new();
    opts.foreground(true); // serve on this thread; the process exists to do it
    let prepared = opts.prepare_copy(
        Source::Bytes(png.into()),
        MimeType::Specific("image/png".to_string()),
    );

    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(e) => {
            report(STATUS_ERR);
            eprintln!("clipboard helper: claiming the selection failed: {e}");
            std::process::exit(1);
        }
    };

    report(STATUS_OK);
    if let Err(e) = prepared.serve() {
        eprintln!("clipboard helper: serving failed: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn report(status: u8) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(&[status]);
    let _ = stdout.flush();
}
