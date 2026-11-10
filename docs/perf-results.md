# Release performance results

Date: 2026-11-10. Machine: Arch/Omarchy, Hyprland (Wayland). Monitors: DP-1 2560x1440 @ 144 Hz, HDMI-A-1 1920x1080 @ 100 Hz. The Shiv window was tiled at 941x1030 on monitor 0 (HDMI-A-1).

## Binary

`cargo build --release -p shiv-ui`: `target/release/shiv_ui` = 34,870,264 bytes (33.3 MiB).

## Method

- Release builds ignore `SHIV_GAME_ROOT` and `SHIV_SCREEN` (both are `#[cfg(debug_assertions)]`), so a release run would use the real Steam install. For measurement an **uncommitted, since reverted** patch honoured `SHIV_GAME_ROOT` in release and added a 250 ms timer that read commands from `/tmp/shiv-ctl` (`screen N`, `scroll D`, `stop`, `apply`, `undo`), and an 8 ms timer that injected `PointerScrolled` events (delta_y -200 logical px per event, about 25,000 px/s). The real game folder was never touched: `SHIV_GAME_ROOT=/tmp/shiv-fix/Deadlock` (fixture from `SHIV_FIXTURE_DIR=/tmp/shiv-fix cargo test -p shiv-ui write_repair_fixture -- --ignored`), `XDG_CACHE_HOME=/tmp/shiv-cache`.
- Browse used the live catalog (10,949 mods) over the network.
- RSS = `VmRSS` in `/proc/<pid>/status`; CPU = utime+stime ticks from `/proc/<pid>/stat` (100 ticks/s).
- The 250 ms poll timer adds a little idle CPU not present in the committed build.

## Results (single run, in order)

| Step | VmRSS | Notes |
| --- | --- | --- |
| 10 s after start, Browse, thumbnails showing | 84.0 MB | |
| 60 s idle on Browse | 84.0 MB | idle CPU: 8 ticks in 60 s (0.13 %); earlier run 2 ticks in 30 s |
| Scrolling the whole list (60 s, 25,000 px/s) | 138.7 MB | CPU 959 ticks in 60 s (about 16 % of one core) |
| 10 s after scroll stopped | 138.7 MB | not returned to the OS |
| Repair screen shown (analyze, 1 tiny fixture mod) | 313.2 MB | +175 MB |
| After Repair apply | 312.5 MB (peak VmHWM 393.1 MB) | |
| Performance screen | 320.4 MB | idle CPU 2 ticks in 20 s |
| Left Performance | 227.3 MB | Performance's `malloc_trim` released about 93 MB |

Second process, no scrolling: idle 84.0 MB, Repair screen 258.5 MB, left Repair 258.5 MB, Performance 259.6 MB, left Performance 172.2 MB (peak 258.8 MB).

## Verdict against the 150 MB target

- Browse idle: 84 MB, pass. Browse after scrolling the full list: 138.7 MB, pass but only 11 MB of headroom; RSS keeps creeping while scrolling (131 MB at 25 s, 138 MB at 60 s).
- **Any run that includes Repair analyze: FAIL.** The analyze step alone adds about 175 MB (258 MB in a fresh process) even against a tiny fixture, peaks at 393 MB after apply, and is not handed back to the OS until Performance's `malloc_trim` runs. A real install (large game VPKs) is likely worse. Not fixed here (out of scope for verification). Suggested follow-up: find the allocation (the 11 MB embedded assets, or per-analysis buffers) and call `malloc_trim` after analyze/apply as Performance does.
- Thumbnails: placeholders show while scrolling at 25,000 px/s; they fill in when scrolling stops (LRU cache is bounded).

## Scrolling feel: OPEN (owner)

Not judgeable by the agent. Measured only: scripted wheel scrolling cost about 16 % of one core on the 941x1030 window. Frame time was not instrumented. Owner must judge feel on the 144 Hz 2560x1440 monitor and on the other monitor (Wayland; X11 if available).

## Checks

- `cargo fmt --all --check`: clean (after formatting `thumbs.rs` and the kv-parser fixes).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (after the mechanical kv-parser fixes).
- `timeout 900 cargo test --workspace`: all pass (kv-parser 74+8+2+7, shiv-api 5+13, shiv-core 18+17+14+15+22+11+7, shiv-repair 228 (30 ignored), shiv-ui 25 (1 ignored), source2-model 86 (4 ignored), vpk-parser 2+1, vpkmanager 65).

## Unverified

- Scroll feel on both monitors; X11.
- RSS with a real, full Deadlock install in Repair.
- Real downloads/installs during the run (install path not exercised here).
