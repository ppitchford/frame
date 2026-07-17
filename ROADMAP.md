# frame — Roadmap

A Wayland-native screenshot and annotation tool for MangoWM. Built for a single user. Replaces `grim` + `slurp` + `satty` with a unified, scriptable Rust binary that also closes the scrolling-capture gap on Wayland.

## Project Principles

- **BYOS (Build Your Own Stuff).** Audience of one. No configurability surface beyond what the author uses. No support for use cases the author hasn't asked for.
  - **"Audience of one" is not "no readers."** There are four: the author now, the author later, the Claude session working on this now, and the one working on it later. Documentation that serves those four — decisions, corrections, and the reasoning behind them — is in scope and earns its keep. What's out of scope is documentation for users and contributors who don't exist: getting-started guides, contributor docs, API surface for consumers, support material.
- **Single static binary.** One executable. No runtime dependencies on system libraries beyond what Void provides by default. GTK is eliminated by this requirement.
- **Clarity over cleverness.** Minimal, precise implementations preferred over flexible, abstract ones.
- **Wayland-first.** No X11 compatibility shim, no legacy fallbacks.

## Architecture Decisions (Locked)

- **Implementation language:** Rust.
- **Output format:** PNG, with optional sidecar JSON for editable annotations.
- **Interactive-mode coordination:** Unix socket at `$XDG_RUNTIME_DIR/frame.sock`. A second invocation of `frame` while a session is active signals the running instance (e.g., scroll-capture stop). The pattern is reusable for any future modal capture; designed as a small command bus rather than a one-off.
- **Annotation editor model:** ordered operation list with a pointer.
  - Undo: move pointer back, re-render from base image through ops up to pointer.
  - Redo: move pointer forward.
  - New op after undo: truncate past pointer, append (standard editor behavior).
  - Save: serialize the op list.
  - Reopen: load list, pointer at end — cross-session undo for free.
  - Each operation must be self-contained: type plus parameters sufficient to re-execute against the base image. No mutable references to prior state.
  - Destructive ops (crop, blur, pixelate) live in the same list as vector ops with different op types. Crop adjusts the canvas rect for subsequent ops; the renderer handles that consistently.
  - Re-render on every change. No diff-based optimization. Undo depth uncapped.
- **Sidecar JSON format:** human-readable. For BYOS debugging, readability beats file size.

## Tier 1 — v1 Ship Target

### Capture
- Region capture, with crosshair and magnifier during selection — **shipped 2026-07-16** (`bf0c4e4`, `216684c`). `frame region`: frozen-grab backdrop, dim, `+` cursor marker, drag rectangle, magnifier loupe; crops to PNG + clipboard. Esc and sub-8px selections cancel with no output.
- Window capture (single-window selection)
- Fullscreen capture (active output or all outputs) — **next up.** Groundwork verified 2026-07-16, so a fresh session need not re-derive it:
  - **Most of it already exists.** `capture::capture_full_output()` grabs the whole output and returns it with the output's integer scale; it is what paints the region overlay's frozen backdrop today. The remaining work is a `frame full` subcommand handing that grab to the Quick Access Overlay, which already takes `(RgbaImage, scale)`. Expect it to be small — resist the urge to rebuild the capture path.
  - **"All outputs" is out of scope until a second output exists.** This machine has exactly one (`eDP-1`, 1440×960 logical, scale 2), so "active output" and "all outputs" are the same image. Multi-output compositing would be speculation against hardware that isn't here — BYOS. Revisit if the Framework is ever docked.
  - **`capture.rs` binds the first `wl_output` advertised and ignores the rest** (`if state.output.is_none()`, line 64). Correct on a single display, silently wrong the day there are two — it would grab whichever output the compositor happens to advertise first, not the active one. Latent assumption, not a bug today; it is the thing that breaks first if "active output" ever has to mean something.
- Scrolling capture — **spike passed (GO)**, see below
- Self-timer (configurable delay)

### Output
- Save to PNG (configurable output path)
- Copy to clipboard
- Sidecar JSON written alongside the PNG when annotations exist

### Post-capture flow
- Quick Access Overlay: floating preview with save / copy / annotate / discard actions — **shipped 2026-07-16** (`09d9db3`). `frame region` is now interactive rather than a headless one-shot: grab → select → crop → floating `eframe` preview. The command writes nothing on its own; the buttons own it. Save and Copy are **non-terminal** and stay enabled after they succeed — the tick reports what happened, but re-copying is the only recovery if another client takes the clipboard, and a second save writes a new timestamped file. Annotate is disabled with a tooltip until there is an editor to open. Save reuses the timestamped `$XDG_PICTURES_DIR` path; a file picker would mean `rfd`, which pulls a portal or GTK.
  - **Floating depends on a compositor rule that is not in this repo.** `windowrule=isfloating:1,appid:^frame$` in `~/.config/mango/config.conf`. A fresh clone tiles instead of floating, and the scroller then *silently overrides* the overlay's requested size — `with_inner_size` is advisory while tiled. `appid` is a regex and the anchors are load-bearing: unanchored, it would match any future app id merely containing "frame". Confirmed against mangowc 0.14.4 — the **config key** is `isfloating`, while the **`mmsg` JSON field** is `is_floating`; both exist in the binary, so guessing between them yields a rule that parses as nothing.
- Previous-capture quick recall (hotkey or command) — still open; needs a capture history that does not exist
- Last-N capture thumbnails in the overlay — still open; same missing history

### Annotation editor
- **Drawing tools:** arrow, line, rectangle (with fill toggle), ellipse (with fill toggle), freehand pencil with auto-smoothing, highlighter, text (predefined style set)
- **Destructive ops:** crop, blur, pixelate

### Cutover — deliberately last

**Deferred 2026-07-16**, to be done once the rest of Tier 1 is complete. The old flow stays intact until `frame` actually covers it; nothing here is a preference call to be re-litigated each session.

- Repoint both `~/.config/mango/config.conf` bindings (lines 205–206) from `~/.local/bin/screenshot` to `frame`. Like the windowrule, they live **outside this repo**:
  - `SUPER,Print` → `screenshot region` → `grim -g "$(slurp)" - | satty --filename -`
  - `SUPER+SHIFT,Print` → `screenshot full` → `grim - | satty --filename -`
- **Blocked on fullscreen capture, not just on preference.** The script covers `region` *and* `full`; `frame` has no `full` yet. Repointing only `SUPER,Print` would split the workflow across two tools and leave `satty` in the loop for half of it.
- The old flow goes capture → *straight* into the editor, with no preview step. The Quick Access Overlay inserts one. Whether that earns its place is a question for the cutover, once there is something behind the Annotate button.
- Retire `~/.local/bin/screenshot` only after both bindings are moved and the flow has survived real use.

## Pre-v1 Spike: Scrolling Capture Feasibility

> **Status: PASSED (GO) — 2026-07-15.** The capture-and-stitch loop produces readable, seam-free output on text-heavy content; all four success criteria met. SAD proved adequate — the `rustfft` fallback is **not** needed. Stitch clears the 2 s target (1589 ms on a ~5-viewport stress run). One noted weakness: periodic/low-texture content (e.g. blocks of identical rules) is ambiguous for SAD and can mis-stitch without a reject — degenerate content, deferred. Full write-up in `SPIKE-FINDINGS.md`. The throwaway spike code lives in `a6762c1` and was retired from the tree by `bf0c4e4` — it is *not* in `src/main.rs`.

Wayland has no compositor primitive for capturing offscreen surface content. The approach is select-area-then-user-scrolls, with frame stitching via the `wlr-screencopy` loop.

**Goal:** prove the capture-and-stitch loop produces a usable, seam-free image on representative content before committing v1 to a schedule that depends on it.

**Approach:**
- User selects region, presses bound hotkey to start, scrolls naturally, presses hotkey again to stop (via Unix socket signal to the running instance).
- Frames captured at ~30 fps via `wlr-screencopy`.
- Frame-to-frame overlap detected by sum-of-absolute-differences (SAD) match: take a horizontal strip from the bottom ~20% of frame N, slide it vertically over frame N+1, accept the offset minimising SAD.
- Reject pairs where best-match SAD exceeds a confidence threshold — drop the frame, accept a potential gap.
- Stitch: append the non-overlapping bottom portion of frame N+1 to the accumulated image.

**Fallback if SAD proves unreliable:** phase correlation via `rustfft`. More robust to noise; FFT-based image registration is well-understood and pure-Rust.

**Success criteria:**
- Readable stitched output for text-heavy content (docs, code, web pages) over 2–3 viewport scrolls.
- No visible seams in typical cases.
- "Done" hotkey to image-ready in under 2 seconds for the typical case.
- Failure modes degrade output gracefully (seams, gaps), not crashes.

**Out of scope even if the spike succeeds:** animated content, infinite-scroll feeds where content loads mid-capture, backward scroll, any content where the viewport changes between frames for reasons other than scroll position.

## Tier 2 — Follow-on (Candidates, Not Commitments)

Re-evaluated once Tier 1 is stable.

- Spotlight tool
- Counter / step-mark tool for numbered tutorial callouts
- Freeze-screen mode (snapshot the screen, then select against the frozen image)
- Combine multiple captures into one canvas
- Floating screenshots (pin a capture above all windows, optional click-through)
- OCR via Tesseract (on-device, text copied to clipboard)
- History browser UI — only if the in-overlay last-N thumbnails prove insufficient in practice
- All-in-one mode (single keybinding exposing all capture modes) — contingent on a wider-release decision
- Native `.frame` format with editable annotation layers — contingent on a wider-release decision
- Migrate screen-capture from `zwlr_screencopy_v1` to `ext-image-copy-capture-v1` — pending broad compositor support for the newer protocol

## Tier 3 — Parked

Will not be revisited without an explicit reason.

- Screen recording (MP4, WebM, GIF) — separate project, codec and encoding work
- Click and keystroke capture overlays during recording
- Camera/webcam overlay during recording
- Padded backgrounds for social-media-ready images
- Cloud upload and shareable links

## Locked Dependencies

Settled during the initial planning session. Versions to confirm against latest at implementation time.

**Behavioural claims in this section were written from planning-time assumption, not from running the crates.** The crate choices are locked; what the crates *do* is not established until an implementation has exercised it. Verify against the crate source and against observable end state — not against a returned `Ok`. The clipboard correction below is what one unverified sentence cost when a later session read it as fact.

**Wayland capture:**
- `wayland-client` + `wayland-protocols-wlr` (raw), targeting `zwlr_screencopy_v1`. Pure-Rust backend, no `libwayland-client.so` runtime dependency.
- `libwayshot` — **fallback retired 2026-07-16.** It was contingent on the spike revealing the raw approach was miscalibrated. The spike passed (GO), and region capture then shipped on the raw stack, so the condition can no longer fire. Recorded as a closed decision, not a live option.

**GUI framework:**
- `eframe` (egui + winit + wgpu) for xdg-shell surfaces: annotation editor, Quick Access Overlay.
- Raw `wayland-client` + `tiny-skia` for `wlr-layer-shell` surfaces: region-selection overlay, and any Tier 2 layer-shell work (freeze-screen mode, floating screenshots).
- **The two stacks run sequentially in one process — verified 2026-07-16 (`09d9db3`).** winit's event loop starts cleanly *after* the raw layer-shell overlay has torn down; `frame region` does exactly this on every capture. This was the main risk in the QAO plan, since nothing established that the second could follow the first on one connection. Running the overlay as a re-exec'd process was the fallback: it is not needed, and is a closed option rather than a live one. This is sequencing, not bridging — `winit` still has no layer-shell support, so the surface split above stands.
- **Feature selection is deliberate.** `eframe`'s defaults pull `x11` — against the Wayland-first principle — plus `web_screen_reader` (dead weight in a native binary) and `accesskit` (not asked for). Only `default_fonts`, `wayland`, and `wgpu` are enabled. It builds clean without the rest. `persistence` is **not** a default, so `with_app_id` (which would otherwise select the persistence-file location) causes no state files.
- `eframe` does bundle wgpu, and wgpu wants Vulkan, which sits in tension with "no runtime dependencies on system libraries beyond what Void provides by default". Mesa/Vulkan is already required by MangoWM via wlroots, so the overlay adds no dependency the compositor does not already impose — but the binary is not static in the strict sense. Recorded as an accepted cost, not a discovery to be made again at link time.

**Clipboard:**

> **Correction — 2026-07-16.** This section previously asserted that `wl-clipboard-rs` "forks a background helper to serve paste requests after the process exits when required." It does not, and never did. The claim was recorded at planning time without running the crate, and was then read as settled fact during task 6: `src/output.rs` was written to it, and shipped a `frame region` that printed `copied to clipboard` while leaving the clipboard empty. It cost a debugging round, and the false confirmations are worth remembering — `copy()` returned `Ok`, the PNG was on disk, and neither was evidence of anything. The only real test was whether the offer outlived the process. Corrected behaviour below.

- `wl-clipboard-rs`. Pure-Rust backend (do not enable the `native_lib` feature). Handles both interactive-overlay and headless-CLI copy contexts through the same code path.
- **A Wayland clipboard offer is served live by the client that makes it**, so it dies with that process. `wl-clipboard-rs` serves it from a thread inside the calling process (see `Options::foreground` in the crate's `copy.rs`) — there is no forked helper. An `Ok` from `copy()` means the offer was registered, not that it will outlive you.
- **`frame` re-execs itself** as a detached `__serve-clipboard` child, which reads the PNG from stdin, claims the offer, and serves until another client takes the clipboard. The child reports success over a pipe before it starts serving, so a genuine failure still reaches the parent's exit code.
- **Re-exec rather than `fork()`:** the Quick Access Overlay copies from an `eframe` (winit/wgpu) process, and `fork()` carries only the calling thread into the child — a mutex held by any other thread at that instant, the allocator's included, stays locked forever in a child that then allocates in its serve loop. `exec` resets the address space, so one code path stays safe from both the single-threaded CLI and the multithreaded overlay.
- **Verified 2026-07-16:** `frame region` → drag → release leaves a PNG on disk and a byte-identical `image/png` on the clipboard, served by a child reparented to `PPID 1` that outlives the capture.
- **The re-exec reasoning has now been exercised — 2026-07-16 (`09d9db3`).** The argument above was written from reasoning alone, before any overlay existed to test it. `copy()` called from inside the multithreaded `eframe`/wgpu process leaves an `image/png` byte-identical to the file on disk, read back *after* the process exited, served by a child at `PPID 1`. The fork-versus-exec argument holds; it is no longer a prediction.

> **Do not verify the clipboard by asking for a paste-back.** Copying the terminal output in order to report it makes the *terminal* take the selection, which evicts our offer and makes a working clipboard look broken. This produced a convincing false alarm on 2026-07-16: `wl-paste --list-types` showed no `image/png` and no helper was alive, and both were correct behaviour — `serve()` is documented to exit when another client takes the clipboard. The tell was that the clipboard held the exact output text, owned by `application/glfw+clipboard-<pid>` (GLFW is kitty's toolkit). **Method that works:** run the command so its output reaches the reader without a copy, then read `wl-paste` from a separate shell while the offer is live, and compare bytes against the file. Related: `pgrep -f` matches the shell running the check — use `pgrep -x frame`, or a phantom process will confirm whatever you already suspect.
