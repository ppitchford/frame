// Compositor IPC: ask MangoWM which output is focused, so capture and the
// region overlay can target it instead of guessing. This is the only place
// `frame` talks to the compositor out-of-band — the pixel grab itself stays
// pure Wayland. Every failure degrades to `None`, and callers fall back to the
// first output, so mmsg is an enhancement rather than a hard dependency.

use std::process::Command;

/// The connector name (e.g. `"eDP-1"`) of the output MangoWM currently has
/// focused, or `None` if it cannot be determined — mmsg absent, no focused
/// client, or unparseable output.
///
/// We read `focusing-client.monitor` rather than the `active` flag on
/// `get all-monitors`: `active` appears to mean "powered on", which is true for
/// every display once docked and so cannot single one out. The focused client's
/// monitor is unambiguous.
pub fn active_output_name() -> Option<String> {
    let json = query(&["get", "focusing-client"])?;
    let name = json.get("monitor")?.as_str()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_owned())
    }
}

/// Run one `mmsg` query and parse the single JSON line it prints. `None` on any
/// failure — mmsg missing, non-zero exit, or unparseable output — so callers
/// degrade rather than abort.
///
/// `Value` is serde_json's untyped tree: enough for pulling a few fields out of
/// mmsg's replies without mirroring its schema in structs that would then have
/// to track it.
pub fn query(args: &[&str]) -> Option<serde_json::Value> {
    // `.ok()?` turns the io::Result into an Option, returning `None` here if
    // mmsg is missing or fails to spawn.
    let output = Command::new("mmsg").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}
