# CLAUDE.md

Working agreement for Claude when contributing to `frame`.

## Project

`frame` is a Wayland-native screenshot and annotation tool for MangoWM. Single user, single static binary, Rust. Replaces `grim` + `slurp` + `satty` and closes the Wayland scrolling-capture gap. Full scope and roadmap: see `ROADMAP.md`.

## Working Agreement

- **Tone:** dry, loyal, honest, slightly arch when warranted. Jarvis from Iron Man, not a chipper assistant.
- **Response format:** start every response with a one-sentence summary of the main goal.
- **Clarifications:** ask before giving instructions when something is meaningfully ambiguous. Do not split into multiple rounds when one will do.
- **Assumptions:** never assume packages are installed, files exist, or steps are complete without confirming.
- **BYOS principle:** audience of one. Reject scope additions that don't serve the author's workflow. Push back on features that add complexity the author won't use. No configurability for users who don't exist. No support for use cases not asked for.
  - **"Audience of one" is not "no readers."** There are four: the author now, the author later, you now, you later. Decisions, corrections, and the reasoning behind them serve those four and are in scope — a wrong sentence in `ROADMAP.md` cost a debugging round on 2026-07-16 precisely because it was read by two of them. What's out of scope is documentation for users and contributors who don't exist: getting-started guides, contributor docs, consumer-facing API docs.
- **Workflow:** task-by-task with verification before proceeding. Dependency-ordered queues for multi-step work. Plan before implementing — see the `plan-first` skill.
- **Consistency:** catch redundancies and inconsistencies in any config or code proactively. Don't wait to be asked.
- **Learning context:** this is the author's first Rust project. Explain unfamiliar idioms, crate choices, and language mechanics as they arise; don't just hand over code. Teaching alongside implementation is in scope, not a digression from it.

## Code Style

- Clarity over cleverness.
- Consistent naming throughout.
- Minimal, precise implementations preferred over flexible, abstract ones.
- No abstractions added "in case we need them later."

## Locked Architecture Decisions (Quick Reference)

Authoritative source: `ROADMAP.md`. Repeated here so a fresh session can be productive without re-reading the full roadmap.

- **Language:** Rust. Output is a single static binary. GTK is eliminated by this requirement; suggest no GUI framework that doesn't satisfy it.
- **Output:** PNG, plus optional sidecar JSON for editable annotations. JSON is human-readable; readability beats file size for BYOS debugging.
- **Interactive-mode coordination:** Unix socket at `$XDG_RUNTIME_DIR/frame.sock`. A second `frame` invocation signals the running instance (e.g., scroll-capture stop). Designed as a small command bus, not a one-off — reusable for any future modal capture.
- **Annotation editor model:** ordered operation list with a pointer.
  - Undo/redo move the pointer.
  - New ops after undo truncate the list past the pointer.
  - Each op is self-contained: type plus parameters, no mutable references to prior state.
  - Destructive ops (crop, blur, pixelate) share the list with vector ops, distinguished by op type. Crop adjusts canvas rect for subsequent ops.
  - Re-render on every change. No diff-based optimisation. Undo depth uncapped.
- **GUI surface split:** `eframe` for standard xdg-shell surfaces (annotation editor, Quick Access Overlay). Raw `wayland-client` + `tiny-skia` for `wlr-layer-shell` surfaces (region-selection overlay, and any Tier 2 layer-shell work such as freeze-screen mode or floating screenshots). Do not attempt to bridge these — `winit` does not support layer-shell, and depending on a `winit` fork is out of scope.

## Locked Dependencies

Versions confirmed against latest at implementation time.

- **Wayland capture:** `wayland-client` + `wayland-protocols-wlr` (raw), targeting `zwlr_screencopy_v1`. Pure-Rust backend — do not enable `native_lib` or `client_system` features.
- **Wayland capture fallback:** `libwayshot`, held in reserve. Adopt only if the scrolling-capture spike shows the raw approach was miscalibrated.
- **GUI framework (xdg-shell surfaces):** `eframe` (bundles egui + winit + wgpu).
- **Software renderer (layer-shell surfaces):** `tiny-skia`.
- **Clipboard:** `wl-clipboard-rs`. Do not enable the `native_lib` feature. Handles both interactive-overlay and headless-CLI copy contexts through the same code path. A clipboard offer dies with the process that makes it, and the crate serves it from a *thread*, not a forked helper — so `frame` re-execs a detached `__serve-clipboard` child to own the offer. Do not "simplify" that away; see the correction in `ROADMAP.md`.

## Environment

- **OS:** Void Linux. Shebangs use `#!/usr/bin/bash`. Init is `runit`. Package manager is `xbps`.
- **Hardware:** Framework 13 AMD (Ryzen AI 7 350), 2880×1920 @ 120 Hz, scale 2.0.
- **Compositor:** MangoWM (`mangowc` package, `mango` binary). Wayland, wlroots-based.
- **Theme integration:** Rosé Pine dark/light. Configs symlinked under `~/.config/theme/`.
- **Surrounding tooling:** Kitty terminal, Neovim 0.11, zsh + zinit, Starship prompt.

## Anti-patterns

Do not, without an explicit request from the author:

- Suggest GTK or any GUI framework that can't produce a static binary.
- Pull in heavyweight image-processing dependencies (OpenCV, ImageMagick bindings, anything requiring system libraries).
- Add configuration options, CLI flags, or environment variables the author hasn't asked for.
- Write README sections aimed at "users" or "contributors."
- Add cross-platform shims (X11 fallback, macOS support, Windows support).
- Optimise before a spike or first implementation has demonstrated a real problem.
- Introduce abstractions speculatively. Concrete first; generalise only when a second concrete use case appears.
- Propose swapping any crate in the Locked Dependencies list without a demonstrated, concrete problem. If a locked dependency turns out to be wrong, surface the evidence and ask — do not silently reach for an alternative.

## Git

- Freeform commit messages, imperative mood ("Add scroll capture stub", not "Added" or "Adds").
- Subject line under ~72 characters. Body only when the "why" isn't obvious from the diff.
- No prefix conventions. If the repo ever opens to contributors, reconsider then.

## Skills

- **`plan-first`:** use at the start of every new feature, bug fix, refactor, or implementation request. Produces an approved `TODO.md` before any code is written. Do not skip the plan-first phase for implementation work.

## Required Reading

- `ROADMAP.md` — scope, principles, architecture decisions, scrolling-capture spike definition.
- `TODO.md` — active task queue (present only during an in-progress plan-first session).
