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
- Fullscreen capture (active output or all outputs)
- Scrolling capture — **spike passed (GO)**, see below
- Self-timer (configurable delay)

### Output
- Save to PNG (configurable output path)
- Copy to clipboard
- Sidecar JSON written alongside the PNG when annotations exist

### Post-capture flow
- Quick Access Overlay: floating preview with save / copy / annotate / discard actions
- Previous-capture quick recall (hotkey or command)
- Last-N capture thumbnails in the overlay

### Annotation editor
- **Drawing tools:** arrow, line, rectangle (with fill toggle), ellipse (with fill toggle), freehand pencil with auto-smoothing, highlighter, text (predefined style set)
- **Destructive ops:** crop, blur, pixelate

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

**Clipboard:**

> **Correction — 2026-07-16.** This section previously asserted that `wl-clipboard-rs` "forks a background helper to serve paste requests after the process exits when required." It does not, and never did. The claim was recorded at planning time without running the crate, and was then read as settled fact during task 6: `src/output.rs` was written to it, and shipped a `frame region` that printed `copied to clipboard` while leaving the clipboard empty. It cost a debugging round, and the false confirmations are worth remembering — `copy()` returned `Ok`, the PNG was on disk, and neither was evidence of anything. The only real test was whether the offer outlived the process. Corrected behaviour below.

- `wl-clipboard-rs`. Pure-Rust backend (do not enable the `native_lib` feature). Handles both interactive-overlay and headless-CLI copy contexts through the same code path.
- **A Wayland clipboard offer is served live by the client that makes it**, so it dies with that process. `wl-clipboard-rs` serves it from a thread inside the calling process (see `Options::foreground` in the crate's `copy.rs`) — there is no forked helper. An `Ok` from `copy()` means the offer was registered, not that it will outlive you.
- **`frame` re-execs itself** as a detached `__serve-clipboard` child, which reads the PNG from stdin, claims the offer, and serves until another client takes the clipboard. The child reports success over a pipe before it starts serving, so a genuine failure still reaches the parent's exit code.
- **Re-exec rather than `fork()`:** the Quick Access Overlay copies from an `eframe` (winit/wgpu) process, and `fork()` carries only the calling thread into the child — a mutex held by any other thread at that instant, the allocator's included, stays locked forever in a child that then allocates in its serve loop. `exec` resets the address space, so one code path stays safe from both the single-threaded CLI and the multithreaded overlay.
- **Verified 2026-07-16:** `frame region` → drag → release leaves a PNG on disk and a byte-identical `image/png` on the clipboard, served by a child reparented to `PPID 1` that outlives the capture.
