# Toolkit decision (Task 0 spike)

**Decision: Slint 1.18.1 with the software renderer (`winit-software`).**
Select it with `SLINT_BACKEND=winit-software` or by building Slint with only
`renderer-software` (plus `backend-winit`, `backend-winit-wayland`) so no GPU stack is linked.
femtovg (130 MB) and skia (145 MB) also pass but cost 4-5x the RSS; software is the lowest-RSS passing renderer.
egui was not tried because a Slint renderer passed.

## Setup

- Machine: Arch Linux, Hyprland (Wayland), Rust 1.98.1, Slint 1.18.1 (features: `backend-winit`,
  `backend-winit-wayland`, `renderer-femtovg`, `renderer-software`, `renderer-skia`; all three compiled).
- Window was tiled by Hyprland to 941x1030 on monitor 0 (100 Hz) in every run.
- 1,000 rows, `ListView` (virtualised). Each row: 240x135 RGBA image (distinct per row, generated per pixel) and two text lines.
- Memory: a custom `slint::Model` creates the row image on demand in `row_data()`, and keeps only the last 48 images
  in a small LRU. 1,000 resident images would be ~130 MB of pixels, so they are not kept.
  Rows scrolled out of view are dropped, and scrolling back regenerates them, so decode/upload cost is part of the measurement.
- Scripted scroll: 1.5 s warm-up, then a 3,000 px/s auto-scroll for 10 s (about 200 rows, all newly instantiated),
  driven per frame from `animation-tick()`. Frame time is the delta between consecutive animation ticks.
- RSS: `VmRSS` from `/proc/self/status`, once after the 1.5 s warm-up (list loaded) and once after the scroll.

## Commands

```
cd spike && cargo build --release
./run.sh winit-femtovg  femtovg-N    # SLINT_BACKEND=winit-femtovg  ./target/release/spike
./run.sh winit-software software-N   # SLINT_BACKEND=winit-software ./target/release/spike
./run.sh winit-skia     skia-N       # SLINT_BACKEND=winit-skia     ./target/release/spike
PERF=refresh_full_speed,console ./run.sh winit-software software-fps   # Slint's own fps counter
```

Logs are in `spike/runs/`.

## Results (monitor refresh 100 Hz, so one frame = 10.0 ms)

| Renderer | RSS after load | RSS after scroll | Median | p95 | p99 | Max | Frames < 16.7 ms | Result |
|---|---|---|---|---|---|---|---|---|
| femtovg (run 1) | 118.1 MB | 130.2 MB | 10.00 ms | 10.25 ms | 10.65 ms | 20.37 ms | 99.8% | pass |
| femtovg (run 2) | 117.9 MB | 129.9 MB | 9.99 ms | 10.31 ms | 10.66 ms | 11.99 ms | 100% | pass |
| skia (run 1) | 143.0 MB | 143.6 MB | 9.97 ms | 10.20 ms | 10.34 ms | 11.88 ms | 100% | pass (narrow RSS margin) |
| skia (run 2) | 144.1 MB | 144.6 MB | 9.57 ms | 10.13 ms | 10.32 ms | 12.03 ms | 100% | pass (narrow RSS margin) |
| software (runs 1-3) | 30.0-30.3 MB | 30.3-30.6 MB | n/a | n/a | n/a | n/a | n/a | pass, see below |

Software frame times: the per-tick numbers for software (1.00 ms median, 10,000 samples) are not frames.
The software renderer keeps the event loop ticking every 1 ms, so they are discarded.
Slint's own counter (`SLINT_DEBUG_PERFORMANCE=refresh_full_speed,console`) reported
**100 fps (10 of 11 one-second samples) and 101 fps (1 sample)** during the scroll, equal to the monitor refresh, so no
frame was dropped at the rate the display can show. Per-frame percentiles are not available for software.

CPU time (user+sys) for the whole ~11.5 s run: femtovg 0.4 s, skia 0.9 s, software 11.4 s. The software figure is a
full core, but it was also a full core during the idle warm-up, so most of it is the harness's 1 ms tick spin, not rendering.
It still means software rendering headroom is unproven.

## Caveats

- A first set of runs, before duplicate tick notifications were filtered out, showed femtovg at 190 MB and skia at 170 MB
  with ~7 ms frames. Those runs landed on the 144 Hz 2560x1440 monitor, and GPU RSS grows with window size.
  I could not reliably re-pin runs to that monitor (`hyprctl movewindow` did not take effect). GPU RSS on a large window is
  therefore above the limit; software RSS did not move with window placement (30-36 MB).
- Software on a 144 Hz or 1440p+ window is untested. The owner should confirm the feel on both monitors.
- I did not compile the programmatic selection API (`slint::BackendSelector`); the env var path was the one verified.
- Fallback if software feels worse than expected on the owner's machine: femtovg (130 MB, passes) with `SLINT_BACKEND=winit-femtovg`.
