# Scrolling-Capture Feasibility Spike — Findings

**Verdict: GO.** The `wlr-screencopy` capture loop plus SAD frame-stitching produces readable, seam-free output on text-heavy scrolling content and meets every success criterion. SAD is more than adequate; the `rustfft` phase-correlation fallback is **not** warranted.

Spike code was throwaway: written into `src/main.rs`, preserved in `a6762c1`, and retired from the tree by `bf0c4e4`. Do not look for it in `src/main.rs`. Artifacts were written to `/tmp/frame-spike/`.

## Test conditions

- **Compositor:** MangoWM. Advertises `zwlr_screencopy_manager_v1` (v3) — the feasibility gate passed immediately.
- **Content:** a Claude Code TUI showing a code diff (line-numbered, syntax-highlighted) — dense text, the target case.
- **Scroll:** ~21 s of continuous downward scrolling, ~5 viewport-heights. A *stress* run, larger than the 2–3 viewports the criteria call "typical."
- **Region:** 676×836 logical, selected via `slurp`; captured buffer 1352×1672 physical (scale 2.0).

## Success criteria — results

| Criterion | Result |
|---|---|
| Readable text over 2–3 viewport scrolls | **Met.** Text razor-sharp and fully legible throughout. |
| No visible seams in typical cases | **Met.** Verified at full resolution across many append boundaries: gutter line numbers run consecutively with no skipped or repeated rows, no tearing. |
| Stop-to-image under 2 s | **Met — 1589 ms** for the 639-frame stress run (see Performance). Typical smaller scrolls are faster. |
| Failure degrades gracefully, not crashes | **Met.** Crash-free over 639 frames; 517 scroll-pause duplicate frames handled with zero artifacts. (The reject→gap path was not triggered — see Caveats.) |

## What worked

- **Capture rate tracks repaint activity, not a fixed clock.** A static screen yields ~10 fps; *active scrolling* yields the full ~30 fps cap. We get high frame rate exactly when we need it. The 10 fps seen in an early static-screen smoke test was an artifact of nothing repainting.
- **SAD locks confidently on text.** Detected offsets were steady (+33 px/frame at 30 fps), mean per-pixel luma SAD stayed **2.7–4.6** against a reject threshold of **15.0** — a 4–5× confidence margin. **Zero rejects** across the run.
- **Duplicate handling is clean.** Pauses between scroll gestures produce offset-0 frames that are skipped without artifacts.

## Performance

Initial naive stitch: **9454 ms** — the offset search scanned up to `height − strip` (~1338 px) per frame pair when real scroll was only 33–66 px/frame. Two output-preserving fixes:

1. **Bounded search** (`MAX_SEARCH_OFFSET = 300`) + coarser sampling (`COL_STEP = 4`, `STRIP_MAX = 200`).
2. **Band-limited luma:** matching only reads the bottom `strip + search = 500` rows, so only that band is converted to luma (was: whole 1672-row frame). Luma cost dropped 1227 ms → 461 ms; stitched output byte-identical.

Result: **9454 ms → 1589 ms**, verified via a throwaway `frame restitch` subcommand that re-stitches the saved frames without a fresh capture. Further headroom exists (reuse the shm buffer across frames; the spike allocates a fresh memfd per frame) but is unnecessary for v1.

## Finding: SAD is ambiguous on periodic / low-texture content

The capture included a large block of near-identical horizontal rules. There, SAD has **multiple near-equal minima** (a repeating pattern matches well at several offsets), so the accumulated row count shifted with matcher parameters — and, critically, a wrong match there produces a **low** SAD, so it is **not** rejected. Text regions were unaffected in every configuration.

This is degenerate content, not the text the tool targets, and it is adjacent to the roadmap's out-of-scope list. Note for later, not a v1 blocker: if it bites, gate confidence on the **ratio of best to second-best minimum** (peak sharpness), not absolute SAD alone. Phase correlation would not reliably fix periodicity either, so this is not a reason to adopt `rustfft`.

## Caveats / unvalidated

- **The reject threshold (15.0) was never exercised** — no pair came close. It is a plausible but unconfirmed boundary; the graceful-gap path has not been seen on real data.
- Out of scope and untested, per the roadmap: backward scroll, animated/infinite-scroll content, any viewport change that isn't a scroll.

## Recommendations for v1

- **Keep the locked stack:** raw `wayland-client` + `zwlr_screencopy_v1` + SAD. No `libwayshot`, no `rustfft`.
- Replace the two spike stubs with the real designs already in the roadmap: the `wlr-layer-shell` selection overlay (for `slurp`) and the `frame.sock` Unix-socket start/stop signal (for Enter/Enter).
- Carry forward the tuned matcher params (`STRIP_MAX 200`, `COL_STEP 4`, `MAX_SEARCH_OFFSET 300`, `BAND_ROWS 500`) and the band-limited-luma approach as the starting point.
- Revisit periodic-content confidence (best/second-best ratio) only if it shows up in practice.
