# Release performance results

Date: 2026-11-10. Machine: Arch/Omarchy, Hyprland (Wayland). Monitors: DP-1 2560x1440 @ 144 Hz, HDMI-A-1 1920x1080 @ 100 Hz. The Shiv window was tiled at 941x1030 on monitor 0 (HDMI-A-1).

## Binary

`cargo build --release -p shiv-ui`: `target/release/shiv_ui` = 34,870,264 bytes (33.3 MiB); after fix round 1, 34,875,576 bytes.

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

## Fix round 1: Repair memory and Browse creep (2026-11-10)

Cause, measured with a counting global allocator (temporary test in shiv-repair, then the same allocator in the measuring patch): the embedded VData history (`crates/shiv-repair/assets/vdata-history.bin`, 10 MB, legacy format) decoded into **79 MB of live heap**, almost all of it a `BTreeMap<key, BTreeSet<value>>` with 150,846 one-element sets. It sat in a `static OnceLock` for the life of the process, and `with_observed` deep-copied it on every plan build (heap peak 162 MB for one analyze of the tiny fixture). Glibc then kept the worker thread's freed pages: the dynamic mmap/trim thresholds rise after large frees, and `malloc_trim` does not release a thread arena's top.

Fix (`fix(repair): ...`): string bindings are a sorted flat `Vec` (64 bytes a pair), tables and bindings are shared through `Arc` instead of cloned, and the decoded index is held through a `Weak` cache, so it is freed when a check returns (one decode per `analyze`). Engine heap: peak 162 MB -> 21 MB, retained 79 MB -> 0.2 MB. The UI calls `malloc_trim` after each Repair worker and sets `M_MMAP_THRESHOLD`/`M_TRIM_THRESHOLD` to 128 KiB at startup (glibc only).

Method as above (same measuring patch plus a `stat` command that printed live heap; reverted). Fixture and cache under the session scratchpad, never the real game. Before and after were built from the same tree with the same patch and run back to back (VmRSS in MB = kB / 1000).

| Step | Before | After |
| --- | --- | --- |
| Browse idle (30 s) | 80.0 | 73.4 |
| Scrolling 30 s / 60 s | 134.2 / 137.6 | 112.8 / 115.5 |
| 10 s after scroll stopped | 139.0 | 116.1 |
| Repair shown (analyze) | 313.6 | 120.5 |
| After Repair apply (+ re-check) | 313.0 | 120.5 |
| Left Repair (Browse) | 313.0 | 120.5 |
| Performance screen | 320.9 | 125.6 |
| Left Performance | 227.8 | 121.8 |
| Peak VmHWM | 393.9 | 147.5 |

Fresh process, no scrolling: before idle 80.3, Repair 254.9, after apply 255.1, peak 256.2; after idle 73.4, Repair 84.8, after apply 85.1, left Performance 86.3, peak 104.2.

With only the repair-engine fix and `malloc_trim` (no `mallopt`), the scroll run still reached 153.3 MB on Repair (peak 183.4) and the no-scroll run kept 13 MB after apply in a thread arena; the fixed thresholds removed both.

Browse scroll creep: live heap goes 24.7 -> 50.3 MB while scrolling and then stays flat; that is the thumbnail LRU filling its 24 MiB budget (bounded, as designed). The rest of the old growth (139 MB RSS against 50 MB live heap) was allocator retention in the four thumbnail worker arenas plus file-backed pages for the image decoders (RssFile +8.6 MB); the fixed thresholds cut it by about 23 MB. RSS still rises about 3 MB between 30 s and 60 s of scrolling (fragmentation); no unbounded structure was found.

## Verdict against the 150 MB target

- Browse idle: 73 MB, pass. Browse after scrolling the full list: 116 MB, pass (was 139 MB).
- Repair (tiny fixture): pass after fix round 1. Shown and applied stay at 120.5 MB after a full scroll (85 MB in a fresh process); peak 147.5 MB, just under the target, reached only when a Repair check runs on top of a full thumbnail cache. Before the fix: FAIL (313 MB, peak 394 MB).
- Thumbnails: placeholders show while scrolling at 25,000 px/s; they fill in when scrolling stops (LRU cache is bounded).

## Scrolling feel: OPEN (owner)

Not judgeable by the agent. Measured only: scripted wheel scrolling cost about 16 % of one core on the 941x1030 window. Frame time was not instrumented. Owner must judge feel on the 144 Hz 2560x1440 monitor and on the other monitor (Wayland; X11 if available).

## Checks

- `cargo fmt --all --check`: clean (after formatting `thumbs.rs` and the kv-parser fixes).
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (after the mechanical kv-parser fixes).
- `timeout 900 cargo test --workspace`: all pass (kv-parser 74+8+2+7, shiv-api 5+13, shiv-core 18+17+14+15+22+11+7, shiv-repair 228 (30 ignored), shiv-ui 25 (1 ignored), source2-model 86 (4 ignored), vpk-parser 2+1, vpkmanager 65).

## Unverified

- Scroll feel on both monitors; X11.
- RSS with a real, full Deadlock install in Repair (the per-check transient grows with the game VPKs and the mods checked; the 79 MB history no longer stays resident).
- Real downloads/installs during the run (install path not exercised here).
