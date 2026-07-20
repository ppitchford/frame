// Window enumeration via MangoWM IPC. `zwlr_screencopy_v1` grabs outputs, not
// surfaces, so window capture is a full-output grab cropped to a rectangle the
// compositor tells us about. This module produces those rectangles.
//
// Two consequences follow from cropping a screen grab, and neither is a bug to
// be fixed later:
//
//   - **Occlusion is captured.** Anything overlapping the target window is in
//     the grab and therefore in the crop. Under the scroller layout that is
//     mostly floating windows. Capturing the window's own surface would need a
//     protocol wlroots does not offer.
//   - **The crop is the geometry rect exactly.** The MangoWM border sits outside
//     it and is treated as compositor chrome, not content.

use crate::monitor;
use crate::overlay::Rect;

/// The windows on `monitor` that are visible and large enough to capture, as
/// rectangles into a grab of that output measuring `grab_w` × `grab_h` physical
/// pixels at `scale`.
///
/// Rectangles are all the picker needs: there is no hover label to draw (see
/// `overlay.rs`), so title and app id would be carried nowhere.
///
/// Returns an empty vector on any mmsg failure, which the caller reports rather
/// than showing a picker that cannot be satisfied. Unlike output selection,
/// there is no useful fallback here: without the compositor's window list there
/// are no rectangles to offer.
///
/// Sorted topmost-first, so the overlay's "first rect containing the cursor"
/// hit-test resolves overlaps the way the screen looks. See `stacking_rank`.
pub fn visible_windows(monitor: &str, scale: i32, grab_w: u32, grab_h: u32) -> Vec<Rect> {
    let Some(view) = viewport(monitor) else {
        return Vec::new();
    };
    let Some(clients) = monitor::query(&["get", "all-clients"]) else {
        return Vec::new();
    };
    let Some(clients) = clients.get("clients").and_then(|c| c.as_array()) else {
        return Vec::new();
    };

    let mut windows: Vec<(u8, u64, Rect)> = clients
        .iter()
        .filter(|c| c.get("monitor").and_then(|m| m.as_str()) == Some(monitor))
        .filter(|c| c.get("is_minimized").and_then(|m| m.as_bool()) != Some(true))
        .filter(|c| on_active_tag(c, &view.active_tags))
        .filter_map(|c| {
            let rect = client_rect(c, &view, scale, grab_w, grab_h)?;
            let area = rect.width as u64 * rect.height as u64;
            Some((stacking_rank(c), area, rect))
        })
        .filter(|(_, _, rect)| rect.is_usable())
        .collect();

    windows.sort_by_key(|&(rank, area, _)| (rank, area));
    windows.into_iter().map(|(_, _, rect)| rect).collect()
}

/// How near the viewer a client sits, coarsely; lower is nearer. Ties break on
/// area, smallest first.
///
/// mmsg reports no z-order and its client ordering is not documented as stacking
/// order, so this reads the one layering fact it does report: MangoWM draws
/// floating windows above tiled ones.
///
/// Area alone was tried first and is wrong. Under the scroller layout tiled
/// windows are columns and a floating window usually spans several of them, so
/// it is the *largest* rect in an overlap, not the smallest — ranking on area
/// alone made floating windows unpickable wherever they covered tiled ones.
/// Area survives only as the tiebreak within a rank, where the innermost-wins
/// intuition does hold.
fn stacking_rank(client: &serde_json::Value) -> u8 {
    match client.get("is_floating").and_then(|f| f.as_bool()) {
        Some(true) => 0,
        _ => 1,
    }
}

/// The part of the global logical coordinate space that `monitor` displays,
/// plus the tags it is currently showing.
struct Viewport {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    active_tags: Vec<i64>,
}

/// Read the monitor's origin, size and active tags.
///
/// Note this reads `active_tags`, not the sibling `active_client` field — the
/// latter was observed disagreeing with `get focusing-client` about which
/// window is focused, so nothing here depends on it.
fn viewport(monitor: &str) -> Option<Viewport> {
    let json = monitor::query(&["get", "monitor", monitor])?;
    Some(Viewport {
        x: json.get("x")?.as_i64()?,
        y: json.get("y")?.as_i64()?,
        width: json.get("width")?.as_i64()?,
        height: json.get("height")?.as_i64()?,
        active_tags: json
            .get("active_tags")?
            .as_array()?
            .iter()
            .filter_map(|t| t.as_i64())
            .collect(),
    })
}

/// Whether any of the client's tags is currently displayed on the monitor.
fn on_active_tag(client: &serde_json::Value, active: &[i64]) -> bool {
    client
        .get("tags")
        .and_then(|t| t.as_array())
        .is_some_and(|tags| {
            tags.iter()
                .filter_map(|t| t.as_i64())
                .any(|t| active.contains(&t))
        })
}

/// Map one client's rectangle into physical grab pixels, or `None` if it does
/// not overlap the viewport at all.
///
/// Client coordinates are **global logical** — one space spanning every output,
/// and under the scroller layout extending well past the visible viewport in
/// both directions (a window scrolled off to the left was observed at
/// `x: -1837`). So the rectangle is intersected with the viewport before it is
/// made output-local, and a window half off the edge yields the visible half.
/// That is deliberate: the picker highlights the same clamped rectangle it
/// captures, so what is highlighted is what is taken.
///
/// `scale` comes from the grab rather than from mmsg's `scale` field: the crop
/// is into those pixels, so the grab is the authority on how many there are.
fn client_rect(
    client: &serde_json::Value,
    view: &Viewport,
    scale: i32,
    grab_w: u32,
    grab_h: u32,
) -> Option<Rect> {
    let x = client.get("x")?.as_i64()?;
    let y = client.get("y")?.as_i64()?;
    let w = client.get("width")?.as_i64()?;
    let h = client.get("height")?.as_i64()?;

    // Intersect with the viewport in the global logical space.
    let x0 = x.max(view.x);
    let y0 = y.max(view.y);
    let x1 = (x + w).min(view.x + view.width);
    let y1 = (y + h).min(view.y + view.height);
    if x1 <= x0 || y1 <= y0 {
        return None; // scrolled fully off this output
    }

    // Make output-local, then scale into the grab's physical pixels, clamping to
    // the grab itself in case the compositor's idea of the output size and the
    // grab's disagree at the edges.
    let s = scale.max(1) as i64;
    let px0 = ((x0 - view.x) * s).clamp(0, grab_w as i64);
    let py0 = ((y0 - view.y) * s).clamp(0, grab_h as i64);
    let px1 = ((x1 - view.x) * s).clamp(0, grab_w as i64);
    let py1 = ((y1 - view.y) * s).clamp(0, grab_h as i64);

    Some(Rect {
        x: px0 as u32,
        y: py0 as u32,
        width: (px1 - px0) as u32,
        height: (py1 - py0) as u32,
    })
}
