//! Shiv desktop UI: a thin Slint shell over shiv-core and shiv-api.

slint::include_modules!();

use shiv_core::game::{self, GameError, GamePaths};

mod browse;
mod crosshair;
mod describe;
mod detail_tabs;
mod installed;
mod launch;
mod performance;
mod profiles;
mod repair;
mod thumbs;

/// The API serves its routes under `/api` (`/api/v2/mods`, ...).
const DEFAULT_API_URL: &str = "https://api.deadlockmods.app/api";

/// Where Deadlock lives. In debug builds only, `SHIV_GAME_ROOT` (the `Deadlock`
/// folder) overrides the Steam lookup, for testing against a throwaway install;
/// it redirects installs too. Release builds ignore it.
pub(crate) fn locate_game() -> Result<GamePaths, GameError> {
    #[cfg(debug_assertions)]
    if let Some(root) = std::env::var_os("SHIV_GAME_ROOT").filter(|v| !v.is_empty()) {
        return Ok(GamePaths::from_root(root.into()));
    }
    game::locate()
}

/// glibc otherwise raises its mmap and trim thresholds as large blocks are
/// freed (up to 32 and 64 MiB), after which worker threads' freed memory (a
/// repair check, decoded images) stays resident and `malloc_trim` cannot
/// release a thread arena's top. Fixed thresholds hand blocks of 128 KiB or
/// more back on free and trim each heap once 128 KiB at its top are free.
fn tune_allocator() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only changes allocator tuning; it is called before any
    // other thread exists.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 128 * 1024);
    }
}

/// Return freed heap pages to the OS (glibc only keeps them otherwise).
pub(crate) fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim only releases free memory held by the allocator;
    // it takes no pointers and is safe to call from any thread.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Run `work` on a named worker thread and pass its result to `done`, also on
/// that thread (it posts back to the event loop). A panic in `work` reaches
/// `done` as `Err` with a readable message, so every caller clears its busy
/// flag or in-flight marker and shows an error instead of hanging (release
/// builds unwind). Returns false when the thread could not be started; `done`
/// is then not called.
pub(crate) fn spawn_worker<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(Result<T, String>) + Send + 'static,
) -> bool {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .map_err(|payload| panic_text(payload.as_ref()));
            done(result);
        })
        .is_ok()
}

/// The message for a worker that panicked.
pub(crate) fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown cause".to_owned());
    format!(
        "Shiv hit an internal error and stopped this task ({detail}). Try again; if it keeps happening, please report it."
    )
}

/// Show the Repair screen (from a stale-package notice elsewhere).
pub(crate) fn open_repair(app: &App) {
    app.set_screen(4);
    // `changed screen` also asks for a check; a second request while the
    // first runs is dropped, so this costs nothing when both fire.
    app.global::<RepairState>().invoke_shown();
}

fn main() -> Result<(), slint::PlatformError> {
    tune_allocator();
    let app = App::new()?;
    // Override for testing (e.g. an unreachable URL to see the Retry state).
    let api_url = std::env::var("SHIV_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let cache = browse::attach(&app, &api_url);
    installed::attach(&app, cache);
    launch::attach(&app);
    repair::attach(&app);
    crosshair::attach(&app);
    performance::attach(&app, &api_url);
    profiles::attach(&app);
    // Debug builds only: open on another screen (e.g. for screenshots); 0 is Browse.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("SHIV_SCREEN")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        app.set_screen(n.clamp(0, 5));
        // Setting the screen from Rust does not run `changed screen`.
        match app.get_screen() {
            2 => app.global::<CrosshairState>().invoke_shown(),
            3 => app.global::<PerfState>().invoke_shown(),
            4 => app.global::<RepairState>().invoke_shown(),
            5 => app.global::<ProfilesState>().invoke_shown(),
            _ => {}
        }
    }
    app.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use slint::{ComponentHandle, Model, Timer, TimerMode};
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    /// Run the event loop until `cond` holds or `timeout` passes. A quit
    /// request left over from an earlier run can end a run early, so the loop
    /// is re-entered until the deadline.
    fn run_until(app: &App, timeout: Duration, cond: impl Fn(&App) -> bool + 'static) -> bool {
        let cond: Rc<dyn Fn(&App) -> bool> = Rc::new(cond);
        let start = Instant::now();
        loop {
            let met = Rc::new(Cell::new(false));
            let quit = Rc::new(Cell::new(false));
            let (flag, sent) = (met.clone(), quit.clone());
            let weak = app.as_weak();
            let check = cond.clone();
            let timer = Timer::default();
            timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
                if sent.get() {
                    return;
                }
                let done = weak.upgrade().is_some_and(|a| check(&a));
                if done || start.elapsed() > timeout {
                    flag.set(done);
                    sent.set(true);
                    let _ = slint::quit_event_loop();
                }
            });
            slint::run_event_loop().expect("event loop");
            if met.get() {
                return true;
            }
            if quit.get() || start.elapsed() > timeout {
                return false;
            }
        }
    }

    #[test]
    fn a_panicking_worker_still_reports_back() {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = tx.clone();
        assert!(spawn_worker(
            "shiv-test-ok",
            || 7,
            move |r| sent.send(r).expect("send")
        ));
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(Ok(7)));
        assert!(spawn_worker(
            "shiv-test-panic",
            || -> i32 { panic!("boom {}", 1) },
            move |r| tx.send(r).expect("send")
        ));
        let text = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reported")
            .expect_err("panicked");
        assert!(
            text.contains("internal error") && text.contains("boom 1"),
            "{text}"
        );
    }

    fn click(app: &App, label: &str) {
        let el = ElementHandle::find_by_accessible_label(app, label)
            .next()
            .unwrap_or_else(|| panic!("no element labelled {label:?}"));
        el.invoke_accessible_default_action();
    }

    /// Review focus: API unreachable on startup shows an error with Retry,
    /// and the other screens are still reachable. Then the install state
    /// machine against a stub API. This is the only test that initialises the
    /// Slint platform (it can be set once per process), hence one function.
    #[test]
    fn api_unreachable_shows_retry_and_navigation_still_works() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        // A port that was just free: connection refused.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("bind");
        let app = App::new().expect("app");
        browse::attach(&app, &format!("http://{addr}"));
        app.show().expect("show");

        let failed = |a: &App| a.global::<BrowseState>().get_load_state() == LoadState::Failed;
        assert!(run_until(&app, Duration::from_secs(20), failed));
        let state = app.global::<BrowseState>();
        assert!(state.get_error_text().contains("could not be reached"));

        click(&app, "Retry");
        assert_eq!(state.get_load_state(), LoadState::Loading);
        assert!(run_until(&app, Duration::from_secs(20), failed));

        for (i, screen) in [
            "Installed",
            "Crosshair",
            "Performance",
            "Repair",
            "Profiles",
            "Browse",
        ]
        .iter()
        .enumerate()
        {
            click(&app, screen);
            assert_eq!(app.get_screen(), ((i + 1) % 6) as i32, "{screen}");
        }
        let api = stub_api();
        install_cancel_and_failure(&api);
        nsfw_flow(&api);
        installed_nsfw_flow(&api);
        download_picker_flow(&api);
        detail_flow();
        #[cfg(unix)]
        profiles_flow();
        installed_screen();
        launch_flow();
        repair_flow();
        crosshair_flow();
        performance_flow();
    }

    /// A tiny install for the repair engine (as in shiv-repair's own tests):
    /// the game ships two heroes; the installed mod carries an outdated copy
    /// of the table without the newer one, which a repair restores.
    pub(crate) fn repair_fixture(root: &std::path::Path) -> shiv_core::game::GamePaths {
        use shiv_core::sidecar::{self, Sidecar};
        use vpkmanager::source2::kv3::{self, Value};

        fn compiled(rows: Vec<(String, Value)>) -> Vec<u8> {
            let payload = kv3::encode(&Value::Object(rows), &kv3::Format([0; 16]));
            let total = 32 + payload.len();
            let mut b = vec![0u8; total];
            b[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
            b[4..6].copy_from_slice(&12u16.to_le_bytes());
            b[8..12].copy_from_slice(&8u32.to_le_bytes());
            b[12..16].copy_from_slice(&1u32.to_le_bytes());
            b[16..20].copy_from_slice(b"DATA");
            b[20..24].copy_from_slice(&12u32.to_le_bytes());
            b[24..28].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
            b[32..].copy_from_slice(&payload);
            b
        }
        fn hero(id: i64) -> Value {
            Value::Object(vec![("m_HeroID".to_string(), Value::Int(id))])
        }
        fn pack(src: &std::path::Path, rows: Vec<(String, Value)>, out: &std::path::Path) {
            let file = src.join("scripts/heroes.vdata_c");
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, compiled(rows)).unwrap();
            vpkmanager::pack_directory(src, out).unwrap();
        }

        let paths = shiv_core::game::GamePaths::from_root(root.join("Deadlock"));
        let citadel = paths.root.join("game").join("citadel");
        std::fs::create_dir_all(&paths.addons).unwrap();
        std::fs::write(citadel.join("steam.inf"), "ClientVersion=100\n").unwrap();
        std::fs::write(
            &paths.gameinfo,
            "\"GameInfo\"\n{\n\tFileSystem\n\t{\n\t\tSearchPaths\n\t\t{\n\t\t\tGame citadel/addons\n\t\t\tGame citadel\n\t\t\tGame core\n\t\t}\n\t}\n}\n",
        )
        .unwrap();
        pack(
            &root.join("game-src"),
            vec![("hero_a".into(), hero(1)), ("hero_new".into(), hero(2))],
            &citadel.join("pak01_dir.vpk"),
        );
        pack(
            &root.join("mod-src"),
            vec![("hero_a".into(), hero(1))],
            &paths.addons.join("pak01_dir.vpk"),
        );
        sidecar::write(
            &paths.addons,
            &Sidecar {
                mod_id: "data".into(),
                name: "Data mod".into(),
                version: None,
                thumbnail_url: None,
                vpks: vec!["pak01_dir.vpk".into()],
                nsfw: None,
            },
        )
        .unwrap();
        paths
    }

    /// Writes the repair fixture to `$SHIV_FIXTURE_DIR` for manual checks and
    /// screenshots (`SHIV_GAME_ROOT=$SHIV_FIXTURE_DIR/Deadlock`).
    #[test]
    #[ignore = "writes a fixture for manual runs"]
    fn write_repair_fixture() {
        let dir = std::env::var_os("SHIV_FIXTURE_DIR").expect("SHIV_FIXTURE_DIR");
        let paths = repair_fixture(std::path::Path::new(&dir));
        println!("SHIV_GAME_ROOT={}", paths.root.display());
    }

    /// Repair against the temp fixture (never the real install): the check
    /// runs off the UI thread when the screen is shown; Apply and Undo are
    /// refused while a check runs; a running game refuses Apply with a
    /// message; Apply asks first, backs up, writes the package and re-checks;
    /// Undo puts everything back; a changed mod marks the package stale.
    fn repair_flow() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempfile::tempdir().expect("tempdir");
        let paths = repair_fixture(temp.path());
        let overlay = paths.root.join("game/citadel/shiv_repair/pak01_dir.vpk");
        let mod_vpk = paths.addons.join("pak01_dir.vpk");
        let mod_bytes = std::fs::read(&mod_vpk).expect("mod");
        let gameinfo = std::fs::read(&paths.gameinfo).expect("gameinfo");

        let running = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicBool::new(false));
        let (flag, gate_in, located) = (running.clone(), gate.clone(), paths.clone());
        let app = App::new().expect("app");
        repair::attach_with(
            &app,
            repair::Deps {
                locate: Arc::new(move || {
                    while gate_in.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(located.clone())
                }),
                running: Arc::new(move || flag.load(Ordering::Relaxed)),
            },
        );
        app.show().expect("show");
        let state = app.global::<RepairState>();
        let idle = |a: &App| {
            let s = a.global::<RepairState>();
            !s.get_scanning() && !s.get_busy() && s.get_load_state() == LoadState::Ready
        };
        let status = |a: &App| {
            a.global::<RepairState>()
                .get_rows()
                .row_data(0)
                .map(|r| r.status.to_string())
        };

        // Nothing runs until the screen is shown; then the check is off-thread.
        assert_eq!(state.get_load_state(), LoadState::Loading);
        assert!(!state.get_scanning());
        app.set_screen(4);
        assert!(run_until(&app, Duration::from_secs(60), idle));
        let r = state.get_rows().row_data(0).expect("row");
        assert_eq!(r.name.as_str(), "Data mod");
        assert_eq!(r.tone, RepairTone::Fixable);
        assert_eq!(r.status.as_str(), "Fixable");
        assert!(
            r.changes
                .row_data(0)
                .is_some_and(|c| c.contains("scripts/heroes.vdata_c")),
            "{:?}",
            r.changes.row_data(0)
        );
        assert_eq!(state.get_fixable_count(), 1);
        assert!(state.get_can_apply() && !state.get_can_undo() && !state.get_stale());
        assert_eq!(state.get_apply_label().as_str(), "Repair 1 mod");
        assert_eq!(state.get_summary().as_str(), "1 mod, 1 fixable");

        // Busy guard: while a check is in flight, Apply, Undo and another
        // check are dropped (no second worker, nothing written).
        gate.store(true, Ordering::Relaxed);
        state.invoke_rescan();
        assert!(state.get_scanning());
        state.invoke_apply();
        state.invoke_undo();
        assert!(!state.get_busy());
        run_until(&app, Duration::from_millis(300), |_| false);
        assert!(!overlay.exists());
        assert_eq!(
            status(&app).as_deref(),
            Some("Fixable"),
            "list kept while re-checking"
        );
        gate.store(false, Ordering::Relaxed);
        assert!(run_until(&app, Duration::from_secs(60), idle));

        // Game running: the check reports it, and Apply (even if invoked
        // directly) is refused by the engine with a plain message.
        running.store(true, Ordering::Relaxed);
        state.invoke_rescan();
        assert!(run_until(&app, Duration::from_secs(60), idle));
        assert!(state.get_game_running());
        state.invoke_apply();
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && a.global::<RepairState>().get_result_title() != ""));
        assert!(state.get_result_error());
        assert_eq!(state.get_result_title().as_str(), "Repair failed");
        assert!(
            state
                .get_result_lines()
                .row_data(0)
                .is_some_and(|l| l.contains("Deadlock is running")),
        );
        assert!(!overlay.exists());
        assert_eq!(std::fs::read(&paths.gameinfo).expect("gameinfo"), gameinfo);
        running.store(false, Ordering::Relaxed);
        click(&app, "Re-scan");
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && !a.global::<RepairState>().get_game_running()));
        click(&app, "Dismiss");
        assert_eq!(state.get_result_title().as_str(), "");

        // Apply asks first; Cancel writes nothing.
        click(&app, "Repair 1 mod");
        assert_eq!(state.get_confirm(), 1);
        click(&app, "Cancel");
        assert_eq!(state.get_confirm(), 0);
        assert!(!overlay.exists());

        // Apply: backup, package, gameinfo mount; the mod VPK is untouched.
        click(&app, "Repair 1 mod");
        click(&app, "Apply repair");
        assert_eq!(state.get_confirm(), 0);
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && a.global::<RepairState>().get_result_title()
                == "Repaired 1 mod"));
        assert!(!state.get_result_error());
        let lines: Vec<String> = (0..state.get_result_lines().row_count())
            .filter_map(|i| state.get_result_lines().row_data(i).map(|l| l.to_string()))
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("Backup of the replaced files: ")),
            "{lines:?}"
        );
        assert!(overlay.exists());
        assert_eq!(std::fs::read(&mod_vpk).expect("mod"), mod_bytes);
        assert_ne!(std::fs::read(&paths.gameinfo).expect("gameinfo"), gameinfo);
        assert_eq!(status(&app).as_deref(), Some("Repaired"));
        assert_eq!(state.get_fixable_count(), 0);
        assert!(state.get_can_undo());

        // Undo asks first, then puts the package and gameinfo back.
        click(&app, "Undo last repair");
        assert_eq!(state.get_confirm(), 2);
        click(&app, "Undo repair");
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && a.global::<RepairState>().get_result_title()
                == "Last repair undone"));
        assert!(!overlay.exists());
        assert_eq!(std::fs::read(&paths.gameinfo).expect("gameinfo"), gameinfo);
        assert_eq!(std::fs::read(&mod_vpk).expect("mod"), mod_bytes);
        assert_eq!(status(&app).as_deref(), Some("Fixable"));
        assert!(!state.get_can_undo());

        // A mod changed after a repair: the package is stale ("needs re-apply").
        click(&app, "Repair 1 mod");
        click(&app, "Apply repair");
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && a.global::<RepairState>().get_result_title()
                == "Repaired 1 mod"));
        let later = std::time::SystemTime::now() + Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&mod_vpk)
            .and_then(|f| f.set_modified(later))
            .expect("touch");
        click(&app, "Re-scan");
        let stale = run_until(&app, Duration::from_secs(60), move |a| {
            idle(a) && a.global::<RepairState>().get_stale()
        });
        assert!(
            stale,
            "{:?} {:?} {} {:?}",
            status(&app),
            shiv_repair::overlay_state(&paths),
            overlay.exists(),
            std::fs::metadata(&mod_vpk).and_then(|m| m.modified())
        );
        assert_eq!(state.get_apply_label().as_str(), "Re-apply repairs");
        assert!(state.get_can_apply());
        assert!(state.get_has_package());

        // The stale package is flagged outside Repair too: on Installed and
        // in the Launch review, each with a way to the Repair screen.
        let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (counter, located) = (launches.clone(), paths.clone());
        launch::attach_with(
            &app,
            launch::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                check: Arc::new(shiv_core::health::check),
                launch: Arc::new(move |_, _| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                running: Arc::new(|| false),
                vanilla: Arc::new(|_, _| Ok(())),
                can_detect: true,
                poll: Duration::from_millis(10),
                timeout: Duration::from_secs(5),
            },
        );
        let located = paths.clone();
        installed::attach_with(
            &app,
            Arc::new(thumbs::Cache::with_fetcher(1 << 20, |_| None)),
            installed::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                running: Arc::new(|| false),
            },
        );
        app.set_screen(1);
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            s.get_load_state() == LoadState::Ready && !s.get_busy() && s.get_repair_stale()
        }));
        click(&app, "Open Repair");
        assert_eq!(app.get_screen(), 4);
        assert!(run_until(&app, Duration::from_secs(60), idle));
        let launch = app.global::<LaunchState>();
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && s.get_repair_stale()
        }));
        assert_eq!(launch.get_problems_short().as_str(), "1 problem");
        // Launch stops at the review instead of launching with it.
        launch.invoke_launch();
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<LaunchState>()
            .get_dialog_open()));
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        launch.invoke_open_repair();
        assert!(!launch.get_dialog_open());
        assert_eq!(app.get_screen(), 4);
        assert!(run_until(&app, Duration::from_secs(60), idle));

        // Remove repair package: asks, unmounts and deletes, independent of
        // Undo; gameinfo.gi is as before any repair.
        click(&app, "Remove repair package");
        assert_eq!(state.get_confirm(), 3);
        click(&app, "Remove package");
        assert!(run_until(&app, Duration::from_secs(60), move |a| idle(a)
            && a.global::<RepairState>().get_result_title()
                == "Removed the repair package"));
        assert!(!state.get_result_error());
        assert!(!overlay.exists());
        assert!(!paths.root.join("game/citadel/shiv_repair").exists());
        assert_eq!(std::fs::read(&paths.gameinfo).expect("gameinfo"), gameinfo);
        assert_eq!(std::fs::read(&mod_vpk).expect("mod"), mod_bytes);
        assert!(!state.get_has_package() && !state.get_stale());
        assert!(state.get_can_undo());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && !s.get_repair_stale()
        }));
        app.global::<InstalledState>().invoke_refresh();
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && !s.get_repair_stale()
        }));
    }

    /// Crosshair against a temp install and a temp Steam userdata folder
    /// (never the real ones): showing the screen loads the hero settings
    /// off-thread; a slider edit updates the values the preview draws and the
    /// commands; a running game refuses Apply; Apply writes the hero settings,
    /// keeps the user's other settings and drops the old autoexec block;
    /// edits are dropped while an apply writes; unapplied edits survive a
    /// re-show; a missing Steam shows a clear error.
    /// A small perf catalog: one preset per tier plus a retired "lean" one
    /// (skipped), one denied convar.
    pub(crate) const PERF_CATALOG: &str = r#"{"schemaVersion":1,"version":"2026.11.07-1","generatedAt":"x",
      "categories":[{"id":"lighting","label":"Lighting","weight":2}],
      "convars":[
        {"name":"fps_max","kind":"int","min":0,"max":1000,"default":"400","help":"Frame rate cap","category":"other"},
        {"name":"r_ssao","kind":"bool","help":"Ambient occlusion","category":"lighting"},
        {"name":"sv_cheats","kind":"bool","category":"other"},
        {"name":"r_mode","kind":"enum","enumValues":[{"value":"0","label":"Low"},{"value":"1","label":"High"}],"category":"other"}
      ],
      "presets":[
        {"id":"b","name":"Bal","author":"me","tier":"balanced","blurb":"Even.","source":{"kind":"bundled"},
         "entries":[{"path":["ConVars","r_ssao"],"value":"false"},{"path":["ConVars","fps_max"],"value":"240"},{"path":["ConVars","sv_cheats"],"value":"1"}]},
        {"id":"p","name":"Pretty","author":"me","tier":"pretty","blurb":"Nice.","source":{"kind":"bundled"},"entries":[{"path":["ConVars","r_ssao"],"value":"true"},{"path":["ConVars","r_mode"],"value":"1"}]},
        {"id":"s","name":"Sweat","author":"me","tier":"sweaty","blurb":"Fast.","source":{"kind":"bundled"},"entries":[{"path":["ConVars","r_ssao"],"value":"false"}]},
        {"id":"t","name":"Potato","author":"me","tier":"potato","blurb":"Fastest.","source":{"kind":"bundled"},"entries":[{"path":["ConVars","r_ssao"],"value":"0"}]},
        {"id":"l","name":"Lean","author":"me","tier":"lean","blurb":"Old.","source":{"kind":"bundled"},"entries":[{"path":["ConVars","r_ssao"],"value":"0"}]}
      ],
      "rules":{"excludedSections":["FileSystem"],"guardedSections":[],"denied":[{"path":["ConVars","sv_cheats"],"reason":"Turns on cheats."}]}}"#;

    /// Performance against a temp gameinfo.gi and a stub catalog: presets
    /// stage changes, the editor follows kind, range and the deny rules,
    /// Apply is refused while the game runs and then writes with a backup.
    fn performance_flow() {
        use shiv_core::perf::CatalogCache;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let temp = tempfile::tempdir().expect("tempdir");
        let paths = shiv_core::game::GamePaths::from_root(temp.path().join("Deadlock"));
        std::fs::create_dir_all(&paths.addons).expect("addons");
        let original = "GameInfo\n{\n\tFileSystem\n\t{\n\t\tSearchPaths\n\t\t{\n\t\t\tGame citadel\n\t\t}\n\t}\n\tConVars\n\t{\n\t\t\"r_ssao\"\t\t\"true\" // keep me\n\t\tfps_max 400\n\t}\n}\n";
        std::fs::write(&paths.gameinfo, original).expect("gameinfo");

        let running = Arc::new(AtomicBool::new(false));
        let fetches = Arc::new(AtomicUsize::new(0));
        let (flag, count, located) = (running.clone(), fetches.clone(), paths.clone());
        let app = App::new().expect("app");
        performance::attach_with(
            &app,
            performance::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                running: Arc::new(move || flag.load(Ordering::Relaxed)),
                fetch: Arc::new(move |etag| {
                    count.fetch_add(1, Ordering::Relaxed);
                    // The saved copy is current once it has the ETag.
                    if etag == Some("\"e1\"") {
                        return Ok(None);
                    }
                    Ok(Some(("\"e1\"".into(), PERF_CATALOG.as_bytes().to_vec())))
                }),
                cache: Some(CatalogCache::new(temp.path().join("cache"))),
            },
        );
        app.show().expect("show");
        let state = app.global::<PerfState>();
        let ready = |a: &App| {
            let s = a.global::<PerfState>();
            s.get_load_state() == LoadState::Ready && s.get_status().contains("settings set")
        };
        app.set_screen(3);
        assert!(run_until(&app, Duration::from_secs(20), ready));
        assert_eq!(
            state.get_status().as_str(),
            "Catalog 2026.11.07-1 · 2 settings set in gameinfo.gi"
        );
        let tiers: Vec<_> = state.get_tiers().iter().collect();
        let labels: Vec<_> = tiers.iter().map(|t| t.label.to_string()).collect();
        assert_eq!(labels, ["Pretty", "Balanced", "Sweaty", "Potato"]);
        assert!(tiers.iter().all(|t| t.available));
        assert!(tiers[1].name.contains("Bal"));
        // Settings the file sets come first.
        let rows: Vec<_> = state.get_rows().iter().collect();
        assert_eq!(rows[0].name.as_str(), "fps_max");
        assert_eq!(rows[0].current.as_str(), "400");
        assert_eq!(rows[1].name.as_str(), "r_ssao");
        assert_eq!(state.get_rows().row_count(), 4);

        // A preset stages its allowed ConVars edits; nothing is written yet.
        click(&app, "Balanced preset");
        assert_eq!(state.get_active_tier(), 1);
        let changes: Vec<_> = state.get_changes().iter().collect();
        assert_eq!(changes.len(), 2, "sv_cheats is denied");
        assert_eq!(changes[0].name.as_str(), "fps_max");
        assert_eq!(changes[0].before.as_str(), "400");
        assert_eq!(changes[0].after.as_str(), "240");
        assert_eq!(state.get_apply_label().as_str(), "Apply 2 changes");
        assert_eq!(
            std::fs::read_to_string(&paths.gameinfo).expect("read"),
            original
        );

        // The editor follows the convar's kind and range.
        state.invoke_search("fps".into());
        assert_eq!(state.get_rows().row_count(), 1);
        click(&app, "fps_max");
        let e = state.get_editor();
        assert!(e.open);
        assert_eq!(e.control, PerfControl::Slider);
        assert_eq!(e.value.as_str(), "240");
        assert_eq!(e.maximum, 1000.0);
        assert!(e.limits.contains("Range 0 to 1000"), "{}", e.limits);
        assert_eq!(e.help.as_str(), "Frame rate cap");
        state.invoke_set_text("5000".into());
        assert_eq!(state.get_editor().value.as_str(), "1000", "clamped");
        state.invoke_set_text("fast".into());
        assert!(state.get_editor().note.contains("whole number"));
        assert_eq!(
            state.get_rows().row_data(0).expect("row").pending.as_str(),
            "1000"
        );
        // Back to the file's value: no longer a change.
        state.invoke_set_number(400.0);
        assert_eq!(state.get_changes().row_count(), 1);
        state.invoke_set_number(300.0);

        // A denied convar can't be edited.
        state.invoke_search("cheats".into());
        click(&app, "sv_cheats");
        assert_eq!(state.get_editor().control, PerfControl::Locked);
        assert!(state.get_editor().note.contains("cheats"));
        state.invoke_toggle();
        assert_eq!(state.get_changes().row_count(), 2);

        // Enum: a choice; picking the file's value changes nothing.
        state.invoke_search("r_mode".into());
        click(&app, "r_mode");
        assert_eq!(state.get_editor().control, PerfControl::Choice);
        state.invoke_pick_option(1);
        assert_eq!(state.get_changes().row_count(), 3);
        click(&app, "Undo r_mode");
        assert_eq!(state.get_changes().row_count(), 2);

        // Comment out: r_ssao goes off.
        state.invoke_search("".into());
        click(&app, "r_ssao");
        assert_eq!(state.get_editor().control, PerfControl::Toggle);
        click(&app, "Comment out");
        let changes: Vec<_> = state.get_changes().iter().collect();
        assert_eq!(changes[1].after.as_str(), "commented out");

        // Game running: Apply is refused and nothing is written.
        let idle = |a: &App| !a.global::<PerfState>().get_busy();
        running.store(true, Ordering::Relaxed);
        state.invoke_apply();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert!(state.get_result_error());
        assert!(state.get_result_detail().contains("Deadlock is running"));
        assert!(state.get_game_running());
        assert_eq!(
            std::fs::read_to_string(&paths.gameinfo).expect("read"),
            original
        );
        assert_eq!(state.get_changes().row_count(), 2, "edits survive");
        running.store(false, Ordering::Relaxed);

        // Apply writes only the listed keys and keeps a backup.
        state.invoke_apply();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert!(!state.get_result_error(), "{}", state.get_result_detail());
        assert_eq!(state.get_result_title().as_str(), "Settings applied");
        let written = std::fs::read_to_string(&paths.gameinfo).expect("read");
        assert_eq!(
            written,
            original
                .replace("fps_max 400", "fps_max 300")
                .replace("\t\t\"r_ssao\"", "\t\t// shiv off: \"r_ssao\"")
        );
        let backup = paths.gameinfo.with_file_name("gameinfo.gi.bak");
        assert_eq!(std::fs::read_to_string(backup).expect("backup"), original);
        let first = shiv_core::perf::original_backup_path(&paths);
        assert_eq!(
            std::fs::read_to_string(&first).expect("shiv-orig"),
            original
        );
        assert!(
            state
                .get_result_detail()
                .contains(&first.display().to_string()),
            "{}",
            state.get_result_detail()
        );
        assert_eq!(state.get_changes().row_count(), 0);
        assert_eq!(state.get_active_tier(), -1);
        assert_eq!(state.get_apply_label().as_str(), "Apply");
        assert!(
            state.get_status().contains("1 settings set"),
            "{}",
            state.get_status()
        );

        // Showing again re-reads the file (the catalog is not fetched again).
        std::fs::write(&paths.gameinfo, original).expect("game update");
        state.invoke_shown();
        let reread = |a: &App| {
            a.global::<PerfState>()
                .get_status()
                .contains("2 settings set")
        };
        assert!(run_until(&app, Duration::from_secs(20), reread));
        assert_eq!(fetches.load(Ordering::Relaxed), 1);

        // Leaving the screen drops the catalog and the list but keeps the
        // pending edits; coming back reloads from the cache (a 304).
        click(&app, "Balanced preset");
        assert_eq!(state.get_changes().row_count(), 2);
        app.set_screen(0);
        let dropped = |a: &App| a.global::<PerfState>().get_rows().row_count() == 0;
        assert!(run_until(&app, Duration::from_secs(20), dropped));
        assert_eq!(state.get_tiers().row_count(), 0);
        assert!(!state.get_editor().open);
        assert_eq!(state.get_changes().row_count(), 2, "pending edits are kept");
        app.set_screen(3);
        let back = |a: &App| {
            let s = a.global::<PerfState>();
            s.get_load_state() == LoadState::Ready && s.get_rows().row_count() == 4
        };
        assert!(run_until(&app, Duration::from_secs(20), back));
        assert_eq!(fetches.load(Ordering::Relaxed), 2);
        assert_eq!(state.get_changes().row_count(), 2);
        assert_eq!(state.get_active_tier(), 1);
        assert_eq!(state.get_tiers().row_count(), 4);
    }

    fn crosshair_flow() {
        use shiv_core::crosshair::{self as core, CrosshairConfig};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let temp = tempfile::tempdir().expect("tempdir");
        let paths = shiv_core::game::GamePaths::from_root(temp.path().join("Deadlock"));
        std::fs::create_dir_all(paths.autoexec.parent().expect("cfg dir")).expect("mkdir");
        let user_dir = temp.path().join("Steam/userdata/42/1422450");
        let lst = core::user_settings_path(&user_dir);
        std::fs::create_dir_all(lst.parent().expect("remote cfg")).expect("mkdir");
        std::fs::write(
            &lst,
            core::EMPTY_SETTINGS.replace(
                "base = {}",
                "base = { mouse_sens = \"1.5\" crosshair_pip_gap = \"5\" }",
            ),
        )
        .expect("hero settings");
        // An autoexec crosshair from the first release would override the
        // hero settings at every launch; Apply removes it.
        let user = "bind mouse4 \"+jump\"\nfps_max 240\n";
        std::fs::write(
            &paths.autoexec,
            format!(
                "{user}// === Shiv - Crosshair Settings (DO NOT EDIT) ===\ncitadel_crosshair_pip_gap 9\n// === End Shiv Crosshair Settings ===\n"
            ),
        )
        .expect("autoexec");

        let running = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(AtomicBool::new(false));
        let no_steam = Arc::new(AtomicBool::new(false));
        let (flag, gate_in, located) = (running.clone(), gate.clone(), paths.clone());
        let (no_steam_in, user_in) = (no_steam.clone(), user_dir.clone());
        let app = App::new().expect("app");
        crosshair::attach_with(
            &app,
            crosshair::Deps {
                locate: Arc::new(move || {
                    while gate_in.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(located.clone())
                }),
                running: Arc::new(move || flag.load(Ordering::Relaxed)),
                user_dir: Arc::new(move || {
                    if no_steam_in.load(Ordering::Relaxed) {
                        Err(core::CrosshairError::SteamNotFound)
                    } else {
                        Ok(user_in.clone())
                    }
                }),
            },
        );
        app.show().expect("show");
        let state = app.global::<CrosshairState>();
        let idle = |a: &App| {
            let s = a.global::<CrosshairState>();
            !s.get_loading() && !s.get_busy()
        };

        // Defaults until shown; showing reads the hero settings on a worker.
        assert_eq!(state.get_values().pip_gap, 0.0);
        // (`changed screen` runs on the event loop, not inside set_screen.)
        app.set_screen(2);
        let loaded = |a: &App| {
            let s = a.global::<CrosshairState>();
            !s.get_loading() && s.get_values().pip_gap == 5.0
        };
        assert!(run_until(&app, Duration::from_secs(20), loaded));
        let v = state.get_values();
        assert_eq!(v.pip_gap, 5.0);
        assert_eq!(v.gap_px, 21.0);
        assert_eq!(
            state.get_status().as_str(),
            "Same as the crosshair in your Steam settings"
        );

        // A slider step (through its accessibility action, like a keyboard
        // or screen reader would) updates the preview's values at once.
        let slider = |e: &ElementHandle| {
            e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Slider)
        };
        let gap = ElementHandle::find_by_accessible_label(&app, "Gap")
            .find(slider)
            .expect("gap slider");
        assert_eq!(gap.accessible_value().as_deref(), Some("5"));
        gap.invoke_accessible_increment_action();
        let v = state.get_values();
        assert_eq!(v.pip_gap, 6.0);
        assert_eq!(v.gap_px, 23.0, "preview gap: 11 + 2 per step above 0");
        assert_eq!(gap.accessible_value().as_deref(), Some("6"));
        assert!(
            state
                .get_commands()
                .contains("citadel_crosshair_pip_gap 6;")
        );
        assert_eq!(state.get_status().as_str(), "Changes not applied yet");
        eprintln!(
            "crosshair preview after Gap +1: pip_gap={} gap_px={} commands={}",
            v.pip_gap,
            v.gap_px,
            state.get_commands()
        );
        let opacity = ElementHandle::find_by_accessible_label(&app, "Opacity")
            .find(slider)
            .expect("opacity slider");
        opacity.invoke_accessible_decrement_action();
        assert_eq!(state.get_values().pip_opacity, 0.9);
        assert_eq!(opacity.accessible_value().as_deref(), Some("90%"));
        // The switch, not the "Outline" text beside it.
        ElementHandle::find_by_accessible_label(&app, "Outline")
            .find(|e| e.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Switch))
            .expect("outline switch")
            .invoke_accessible_default_action();
        assert!(state.get_values().pip_border);
        click(&app, "Preset colour 2");
        assert_eq!(state.get_values().hex.as_str(), "#FF0000");
        let edited = CrosshairConfig {
            gap: 6.0,
            pip_opacity: 0.9,
            pip_border: true,
            color: core::Rgb { r: 255, g: 0, b: 0 },
            ..CrosshairConfig::default()
        };

        // Re-showing keeps unapplied edits.
        state.invoke_shown();
        assert!(state.get_loading());
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(state.get_values().pip_gap, 6.0);

        // Game running: Apply is refused and nothing is written.
        let before = std::fs::read_to_string(&lst).expect("read");
        running.store(true, Ordering::Relaxed);
        state.invoke_apply();
        assert!(state.get_busy());
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert!(state.get_result_error());
        assert!(state.get_result_detail().contains("Deadlock is running"));
        assert!(state.get_game_running());
        assert_eq!(std::fs::read_to_string(&lst).expect("read"), before);
        running.store(false, Ordering::Relaxed);

        // Busy guard: while an apply is in flight, edits and a second apply
        // are dropped.
        // The stale "running" flag does not disable the button: the worker
        // re-checks (the game has closed meanwhile).
        gate.store(true, Ordering::Relaxed);
        assert!(state.get_game_running());
        click(&app, "Apply crosshair");
        assert!(state.get_busy());
        state.invoke_edit(CrosshairField::PipGap, 40.0);
        state.invoke_apply();
        assert_eq!(state.get_values().pip_gap, 6.0);
        gate.store(false, Ordering::Relaxed);
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert!(!state.get_result_error(), "{}", state.get_result_detail());
        assert_eq!(state.get_result_title().as_str(), "Crosshair applied");
        assert!(!state.get_game_running());
        assert_eq!(
            state.get_status().as_str(),
            "Same as the crosshair in your Steam settings"
        );
        let text = std::fs::read_to_string(&lst).expect("read");
        assert!(text.contains("mouse_sens = \"1.5\""), "{text}");
        assert!(text.contains("crosshair_pip_gap = \"6\""), "{text}");
        assert_eq!(
            std::fs::read_to_string(&paths.autoexec).expect("read"),
            user
        );
        assert_eq!(core::load(&paths, &user_dir).expect("load"), Some(edited));

        // Reset shows the default; a fresh show (no edits) re-reads the file.
        click(&app, "Reset to default");
        assert_eq!(state.get_values().pip_gap, 0.0);
        click(&app, "Dismiss");
        assert_eq!(state.get_result_title().as_str(), "");
        state.invoke_shown();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(state.get_values().pip_gap, 0.0, "unapplied reset is kept");
        // Applying the reset and showing again reads the default back.
        state.invoke_apply();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        state.invoke_shown();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(state.get_values().pip_gap, 0.0);
        assert_eq!(
            core::load(&paths, &user_dir).expect("load"),
            Some(CrosshairConfig::default())
        );

        // No Steam: reading and applying both say so; editing still works.
        no_steam.store(true, Ordering::Relaxed);
        state.invoke_shown();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert!(state.get_result_error());
        assert_eq!(
            state.get_result_title().as_str(),
            "Couldn't read your crosshair settings"
        );
        assert!(
            state.get_result_detail().contains("Steam was not found"),
            "{}",
            state.get_result_detail()
        );
        state.invoke_edit(CrosshairField::PipGap, 3.0);
        assert_eq!(state.get_values().pip_gap, 3.0);
        state.invoke_apply();
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(
            state.get_result_title().as_str(),
            "Couldn't apply the crosshair"
        );
        assert!(state.get_result_detail().contains("Steam was not found"));
        no_steam.store(false, Ordering::Relaxed);
    }

    /// Launch against a temp install with a fake launcher and a fake game
    /// process (Steam is never started): the startup check fills the sidebar
    /// quietly; Launch shows the dialog; Cancel launches nothing; Launch
    /// anyway after Review re-checks and shows changed findings instead of
    /// launching; screen changes refresh the count; a clean install launches
    /// at once; an opener failure shows. After a launch the status says
    /// Launching until the game process appears, then Running until it exits;
    /// Cancel launch stops waiting (and stops a still-running opener); no game
    /// process before the timeout says the launch could not be confirmed; a
    /// game started elsewhere is followed instead of launched again.
    fn launch_flow() {
        use shiv_core::game::GamePaths;
        use shiv_core::launch::LaunchError;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let root = tempfile::tempdir().expect("tempdir");
        let paths = GamePaths::from_root(root.path().to_path_buf());
        std::fs::create_dir_all(&paths.addons).expect("addons");
        std::fs::write(
            &paths.gameinfo,
            "GameInfo { FileSystem { SearchPaths { Game citadel/addons\n Game citadel } } }",
        )
        .expect("gameinfo");
        let bad = paths.addons.join("pak01_dir.vpk");
        let bad2 = paths.addons.join("pak02_dir.vpk");
        std::fs::write(&bad, b"").expect("zero-byte vpk");

        let launches = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        // The fake game process; a launch starts it while `starts` is set.
        let game = Arc::new(AtomicBool::new(false));
        let starts = Arc::new(AtomicBool::new(true));
        // While set, the launcher blocks like an opener that has not returned.
        let hold = Arc::new(AtomicBool::new(false));
        let opener_stopped = Arc::new(AtomicBool::new(false));
        let (counter, failing, game_in, starts_in, hold_in, stopped) = (
            launches.clone(),
            fail.clone(),
            game.clone(),
            starts.clone(),
            hold.clone(),
            opener_stopped.clone(),
        );
        let located = paths.clone();
        let vanilla_log = Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
        let vanilla_in = vanilla_log.clone();
        let game_seen = game.clone();
        let app = App::new().expect("app");
        launch::attach_with(
            &app,
            launch::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                check: Arc::new(shiv_core::health::check),
                launch: Arc::new(move |_, cancel| {
                    if failing.load(Ordering::SeqCst) {
                        return Err(LaunchError::OpenerFailed {
                            program: "xdg-open".into(),
                            status: "exit status: 4".into(),
                        });
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    while hold_in.load(Ordering::SeqCst) {
                        if cancel.load(Ordering::SeqCst) {
                            stopped.store(true, Ordering::SeqCst);
                            return Err(LaunchError::Cancelled);
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if starts_in.load(Ordering::SeqCst) {
                        game_in.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                }),
                running: Arc::new(move || game_seen.load(Ordering::SeqCst)),
                vanilla: Arc::new(move |_, on| {
                    vanilla_in.lock().unwrap().push(on);
                    Ok(())
                }),
                can_detect: true,
                poll: Duration::from_millis(10),
                timeout: Duration::from_millis(1500),
            },
        );
        app.show().expect("show");
        let state = app.global::<LaunchState>();
        let count = |n: usize| {
            move |a: &App| {
                let s = a.global::<LaunchState>();
                !s.get_busy() && s.get_findings().row_count() == n
            }
        };
        // Launched `n` times and the game is up.
        let launched = |n: usize| {
            let l = launches.clone();
            move |a: &App| {
                let s = a.global::<LaunchState>();
                !s.get_busy() && s.get_running() && l.load(Ordering::SeqCst) == n
            }
        };
        let dialog = |a: &App| a.global::<LaunchState>().get_dialog_open();
        let close_game = |app: &App| {
            game.store(false, Ordering::SeqCst);
            assert!(run_until(app, Duration::from_secs(20), |a| {
                let s = a.global::<LaunchState>();
                !s.get_running() && !s.get_busy()
            }));
            assert_eq!(
                app.global::<LaunchState>().get_status_text().as_str(),
                launch::EXITED_TEXT
            );
        };

        // Startup check: quiet (no busy state), sidebar count, no dialog, no launch.
        assert!(!state.get_busy());
        assert!(run_until(&app, Duration::from_secs(20), count(1)));
        assert_eq!(state.get_problems_short().as_str(), "1 problem");
        assert!(!state.get_dialog_open());
        assert_eq!(launches.load(Ordering::SeqCst), 0);

        // Launch: re-checks, then asks. Cancel launches nothing.
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), dialog));
        let row = state.get_findings().row_data(0).expect("row");
        assert_eq!(row.title.as_str(), "Empty VPK");
        assert_eq!(row.file.as_str(), "addons/pak01_dir.vpk");
        click(&app, "Cancel");
        assert!(!state.get_dialog_open());
        run_until(&app, Duration::from_millis(200), |_| false);
        assert_eq!(launches.load(Ordering::SeqCst), 0);

        // Review shows the earlier result; the disk changes meanwhile, so
        // Launch anyway re-checks and shows the new findings instead.
        click(&app, "Review");
        assert!(state.get_dialog_open());
        std::fs::write(&bad2, b"").expect("second zero-byte vpk");
        click(&app, "Launch anyway");
        assert!(run_until(&app, Duration::from_secs(20), move |a| dialog(a)
            && a.global::<LaunchState>().get_findings().row_count() == 2));
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        // These findings are fresh: Launch anyway launches directly. The
        // status follows the game process: Running, then closed.
        click(&app, "Launch anyway");
        assert!(run_until(&app, Duration::from_secs(20), launched(1)));
        assert_eq!(state.get_status_text().as_str(), launch::RUNNING_TEXT);
        assert!(!state.get_waiting());
        // Launch is refused while the game runs.
        state.invoke_launch();
        run_until(&app, Duration::from_millis(200), |_| false);
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        close_game(&app);

        // Review with unchanged findings: the re-check matches, so it launches.
        click(&app, "Review");
        click(&app, "Launch anyway");
        assert!(run_until(&app, Duration::from_secs(20), launched(2)));
        assert!(!state.get_dialog_open());
        close_game(&app);

        // Fixing the files elsewhere: a screen change refreshes the count.
        std::fs::remove_file(&bad).expect("remove");
        std::fs::remove_file(&bad2).expect("remove");
        app.set_screen(2);
        assert!(run_until(&app, Duration::from_secs(20), count(0)));

        // Clean: Launch goes straight through.
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), launched(3)));
        assert!(!state.get_dialog_open());
        close_game(&app);

        // Launch without mods: mods go off for the session and come back
        // when the game exits.
        click(&app, "Launch without mods");
        assert!(run_until(&app, Duration::from_secs(20), launched(4)));
        assert_eq!(
            vanilla_log.lock().unwrap().iter().filter(|on| **on).count(),
            1
        );
        close_game(&app);
        let log = vanilla_log.clone();
        assert!(run_until(&app, Duration::from_secs(20), move |_| {
            log.lock().unwrap().last() == Some(&false)
        }));

        // Steam takes the link but the game does not appear: Launching (with
        // Cancel launch) until Cancel; nothing is followed afterwards.
        starts.store(false, Ordering::SeqCst);
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<LaunchState>()
            .get_waiting()));
        assert!(state.get_busy());
        assert_eq!(state.get_busy_text().as_str(), launch::LAUNCHING_TEXT);
        click(&app, "Cancel launch");
        assert!(!state.get_waiting() && !state.get_busy() && !state.get_running());
        assert_eq!(state.get_status_text().as_str(), launch::CANCELLED_TEXT);
        assert!(!state.get_status_error());
        game.store(true, Ordering::SeqCst);
        run_until(&app, Duration::from_millis(300), |_| false);
        assert!(!state.get_running(), "a cancelled launch is not followed");
        game.store(false, Ordering::SeqCst);

        // ...or until the timeout: the status says it could not confirm.
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && s.get_status_error()
        }));
        assert!(!state.get_waiting() && !state.get_running());
        assert!(
            state.get_status_text().contains("Could not confirm"),
            "{}",
            state.get_status_text()
        );
        assert_eq!(launches.load(Ordering::SeqCst), 6);

        // Cancel while the opener itself has not returned: it is stopped.
        hold.store(true, Ordering::SeqCst);
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<LaunchState>()
            .get_waiting()));
        click(&app, "Cancel launch");
        assert!(!state.get_busy());
        let stopped = opener_stopped.clone();
        assert!(run_until(&app, Duration::from_secs(20), move |_| stopped
            .load(Ordering::SeqCst)));
        assert_eq!(state.get_status_text().as_str(), launch::CANCELLED_TEXT);
        hold.store(false, Ordering::SeqCst);
        starts.store(true, Ordering::SeqCst);

        // Started outside Shiv: Launch follows it instead of launching again,
        // and the state returns to ready when it exits.
        game.store(true, Ordering::SeqCst);
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && s.get_running()
        }));
        assert_eq!(
            state.get_status_text().as_str(),
            launch::ALREADY_RUNNING_TEXT
        );
        assert_eq!(launches.load(Ordering::SeqCst), 7);
        close_game(&app);

        // The opener refusing the link is shown as an error.
        fail.store(true, Ordering::SeqCst);
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && s.get_status_error()
        }));
        assert!(
            state
                .get_status_text()
                .contains("could not open the Steam link"),
            "{}",
            state.get_status_text()
        );
        assert!(!state.get_running() && !state.get_waiting());
        assert_eq!(launches.load(Ordering::SeqCst), 7);

        // Where the game process cannot be detected (no process scanning):
        // the request is sent and that is all the status claims; no waiting,
        // no timeout, and the process check is never consulted.
        let sent = Arc::new(AtomicUsize::new(0));
        let (sent_in, located) = (sent.clone(), paths.clone());
        let app2 = App::new().expect("app");
        launch::attach_with(
            &app2,
            launch::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                check: Arc::new(shiv_core::health::check),
                launch: Arc::new(move |_, _| {
                    sent_in.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                running: Arc::new(|| panic!("process scanning is unavailable")),
                vanilla: Arc::new(|_, _| Ok(())),
                can_detect: false,
                poll: Duration::from_millis(10),
                timeout: Duration::from_millis(200),
            },
        );
        app2.show().expect("show");
        let s2 = app2.global::<LaunchState>();
        assert!(run_until(&app2, Duration::from_secs(20), |a| !a
            .global::<LaunchState>()
            .get_busy()));
        s2.invoke_launch();
        assert!(run_until(&app2, Duration::from_secs(20), |a| {
            let s = a.global::<LaunchState>();
            !s.get_busy() && s.get_status_text() != ""
        }));
        assert_eq!(s2.get_status_text().as_str(), launch::SENT_TEXT);
        assert!(!s2.get_status_error() && !s2.get_waiting() && !s2.get_running());
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        // Past the timeout nothing changes: no "could not confirm".
        run_until(&app2, Duration::from_millis(400), |_| false);
        assert_eq!(s2.get_status_text().as_str(), launch::SENT_TEXT);
        // Launch stays available.
        s2.invoke_launch();
        let again = sent.clone();
        assert!(run_until(&app2, Duration::from_secs(20), move |_| again
            .load(Ordering::SeqCst)
            == 2));
    }

    /// Installed screen against a temp addons dir (never the real one):
    /// listing, toggle, game-running gating, uninstall, and error states.
    fn installed_screen() {
        use shiv_core::game::{GameError, GamePaths};
        use shiv_core::sidecar::{self, Sidecar};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().expect("tempdir");
        let paths = GamePaths::from_root(root.path().to_path_buf());
        let addons = paths.addons.clone();
        std::fs::create_dir_all(&addons).expect("addons");
        for f in ["pak01_dir.vpk", "pak01_000.vpk", "custom.vpk"] {
            std::fs::write(addons.join(f), b"x").expect("vpk");
        }
        sidecar::write(
            &addons,
            &Sidecar {
                mod_id: "m1".into(),
                name: "Test Mod".into(),
                version: Some("1.0".into()),
                thumbnail_url: None,
                vpks: vec!["pak01_dir.vpk".into()],
                nsfw: None,
            },
        )
        .expect("sidecar");
        let exists = |n: &str| addons.join(n).exists();
        let running = Arc::new(AtomicBool::new(false));
        let located = paths.clone();
        let flag = running.clone();
        // While set, `locate` (and so every refresh) blocks on its worker thread.
        let gate = Arc::new(AtomicBool::new(false));
        let gate_in = gate.clone();
        // When set, the next game-running check panics on its worker.
        let boom = Arc::new(AtomicBool::new(false));
        let boom_in = boom.clone();
        // While set, an action's worker waits (refreshes do not).
        let hold = Arc::new(AtomicBool::new(false));
        let hold_in = hold.clone();
        let deps = installed::Deps {
            locate: Arc::new(move || {
                while gate_in.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(located.clone())
            }),
            running: Arc::new(move || {
                assert!(!boom_in.swap(false, Ordering::Relaxed), "test worker panic");
                while hold_in.load(Ordering::Relaxed)
                    && std::thread::current().name() == Some("shiv-installed-action")
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                flag.load(Ordering::Relaxed)
            }),
        };
        let app = App::new().expect("app");
        let cache = Arc::new(thumbs::Cache::with_fetcher(1 << 20, |_| None));
        installed::attach_with(&app, cache.clone(), deps);
        app.show().expect("show");
        let state = app.global::<InstalledState>();
        let enabled = |a: &App, i: usize| {
            a.global::<InstalledState>()
                .get_rows()
                .row_data(i)
                .map(|r| r.enabled)
        };

        // Showing the screen loads the list.
        app.set_screen(1);
        let ready = |a: &App| a.global::<InstalledState>().get_load_state() == LoadState::Ready;
        assert!(run_until(&app, Duration::from_secs(20), ready));
        let rows = state.get_rows();
        assert_eq!(rows.row_count(), 2);
        let (r0, r1) = (rows.row_data(0).unwrap(), rows.row_data(1).unwrap());
        assert_eq!(
            (r0.name.as_str(), r0.managed, r0.enabled),
            ("Test Mod", true, true)
        );
        assert_eq!((r1.name.as_str(), r1.managed), ("custom.vpk", false));
        assert_eq!(r1.tag.as_str(), "Not installed by Shiv");
        assert!(!state.get_game_running());

        // Toggle off: dir and chunk gain `.disabled`, the pak number is untouched.
        state.invoke_toggle("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), move |a| {
            a.global::<InstalledState>()
                .get_rows()
                .row_data(0)
                .is_some_and(|r| !r.enabled)
                && !a.global::<InstalledState>().get_busy()
        }));
        assert!(exists("pak01_dir.vpk.disabled") && exists("pak01_000.vpk.disabled"));
        assert!(!exists("pak01_dir.vpk") && !exists("pak01_000.vpk"));
        assert_eq!(
            state.get_rows().row_data(0).unwrap().name.as_str(),
            "Test Mod"
        );
        state.invoke_toggle("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), move |a| {
            a.global::<InstalledState>()
                .get_rows()
                .row_data(0)
                .is_some_and(|r| r.enabled)
                && !a.global::<InstalledState>().get_busy()
        }));
        assert!(exists("pak01_dir.vpk") && exists("pak01_000.vpk"));
        assert!(!exists("pak01_dir.vpk.disabled"));

        // A second toggle while the follow-up refresh is pending is dropped, so
        // it cannot act on stale state; the disk matches the one accepted action.
        gate.store(true, Ordering::Relaxed);
        state.invoke_toggle("m1".into());
        assert!(state.get_busy());
        state.invoke_toggle("m1".into()); // ignored: busy
        run_until(&app, Duration::from_millis(500), |_| false);
        assert!(exists("pak01_dir.vpk.disabled"), "first toggle ran");
        assert!(state.get_busy(), "still busy while the refresh is pending");
        state.invoke_toggle("m1".into()); // acted-on but refresh pending: still ignored
        run_until(&app, Duration::from_millis(300), |_| false);
        assert!(exists("pak01_dir.vpk.disabled") && !exists("pak01_dir.vpk"));
        gate.store(false, Ordering::Relaxed);
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_rows().row_data(0).is_some_and(|r| !r.enabled)
        }));
        state.invoke_toggle("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_rows().row_data(0).is_some_and(|r| r.enabled)
        }));
        assert!(exists("pak01_dir.vpk"));

        // A worker that panics mid-action: busy clears, an error shows, and
        // nothing was changed.
        boom.store(true, Ordering::Relaxed);
        state.invoke_toggle("m1".into());
        assert!(state.get_busy());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_banner_text() != ""
        }));
        assert!(
            state.get_banner_text().contains("internal error")
                && state.get_banner_text().contains("test worker panic"),
            "{}",
            state.get_banner_text()
        );
        assert!(exists("pak01_dir.vpk"));
        assert_eq!(enabled(&app, 0), Some(true));
        click(&app, "Dismiss");

        // A Refresh that finishes while an action still runs does not clear
        // busy: a second toggle cannot start beside the first.
        hold.store(true, Ordering::Relaxed);
        state.invoke_toggle("m1".into());
        assert!(state.get_busy());
        state.invoke_refresh();
        run_until(&app, Duration::from_millis(500), |_| false);
        assert!(state.get_busy(), "the action is still running");
        state.invoke_toggle("m1".into()); // ignored: an action runs
        hold.store(false, Ordering::Relaxed);
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_rows().row_data(0).is_some_and(|r| !r.enabled)
        }));
        run_until(&app, Duration::from_millis(300), |_| false);
        assert!(exists("pak01_dir.vpk.disabled") && !exists("pak01_dir.vpk"));
        state.invoke_toggle("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_rows().row_data(0).is_some_and(|r| r.enabled)
        }));

        // Game running: refresh picks it up; actions are refused with an explanation.
        running.store(true, Ordering::Relaxed);
        state.invoke_refresh();
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<InstalledState>()
            .get_game_running()));
        assert!(!state.get_busy());
        // The worker re-checks the game: the toggle is refused...
        state.invoke_toggle("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_banner_text() != ""
        }));
        assert!(state.get_banner_text().starts_with("Could not disable"));
        click(&app, "Dismiss");
        // ...and so is the uninstall, through the GameRunning mapping.
        state.invoke_uninstall("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            let s = a.global::<InstalledState>();
            !s.get_busy() && s.get_banner_text() != ""
        }));
        assert!(state.get_banner_text().starts_with("Could not uninstall"));
        assert!(state.get_banner_text().contains("Deadlock is running"));
        assert!(exists("pak01_dir.vpk") && addons.join(".shiv/m1.json").exists());
        assert_eq!(enabled(&app, 0), Some(true));
        click(&app, "Dismiss");
        assert_eq!(state.get_banner_text(), "");

        // Game closed again: a stale "running" state clears on refresh.
        running.store(false, Ordering::Relaxed);
        state.invoke_refresh();
        assert!(run_until(&app, Duration::from_secs(20), |a| !a
            .global::<InstalledState>()
            .get_game_running()));

        // Uninstall removes the VPKs, chunks and the sidecar; hand-added VPKs go too.
        state.invoke_uninstall("m1".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<InstalledState>()
            .get_rows()
            .row_count()
            == 1));
        assert!(!exists("pak01_dir.vpk") && !exists("pak01_000.vpk"));
        assert!(!addons.join(".shiv/m1.json").exists());
        assert!(exists("custom.vpk"));
        state.invoke_uninstall("custom.vpk".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<InstalledState>()
            .get_rows()
            .row_count()
            == 0));
        assert!(!exists("custom.vpk"));
        assert_eq!(state.get_load_state(), LoadState::Ready);
        assert_eq!(state.get_banner_text(), "");

        // Error states: game not found, addons folder unreadable; Retry recovers.
        let broken = Arc::new(AtomicBool::new(true));
        let missing = Arc::new(AtomicBool::new(true));
        let (b, m, p) = (broken.clone(), missing.clone(), paths.clone());
        let bad_root = tempfile::tempdir().expect("tempdir");
        let file_as_addons = GamePaths::from_root(bad_root.path().to_path_buf());
        std::fs::create_dir_all(file_as_addons.addons.parent().expect("parent")).expect("dirs");
        std::fs::write(&file_as_addons.addons, b"not a dir").expect("file");
        let app2 = App::new().expect("app");
        installed::attach_with(
            &app2,
            cache,
            installed::Deps {
                locate: Arc::new(move || {
                    if m.load(Ordering::Relaxed) {
                        Err(GameError::NotFound)
                    } else if b.load(Ordering::Relaxed) {
                        Ok(file_as_addons.clone())
                    } else {
                        Ok(p.clone())
                    }
                }),
                running: Arc::new(|| false),
            },
        );
        app2.show().expect("show");
        let s2 = app2.global::<InstalledState>();
        let failed = |a: &App| a.global::<InstalledState>().get_load_state() == LoadState::Failed;
        app2.set_screen(1);
        assert!(run_until(&app2, Duration::from_secs(20), failed));
        assert!(
            s2.get_error_text().contains("not found"),
            "{}",
            s2.get_error_text()
        );
        missing.store(false, Ordering::Relaxed);
        click(&app2, "Retry");
        assert!(run_until(&app2, Duration::from_secs(20), move |a| {
            a.global::<InstalledState>()
                .get_error_text()
                .contains("addons folder")
        }));
        assert_eq!(s2.get_load_state(), LoadState::Failed);
        broken.store(false, Ordering::Relaxed);
        click(&app2, "Retry");
        assert!(run_until(&app2, Duration::from_secs(20), |a| a
            .global::<InstalledState>()
            .get_load_state()
            == LoadState::Ready));
    }

    /// A 64x36 PNG: a white square on black, so a blurred copy differs from it.
    fn stub_png() -> Vec<u8> {
        let img = image::RgbImage::from_fn(64, 36, |x, y| {
            let on = (16..48).contains(&x) && (8..28).contains(&y);
            image::Rgb(if on { [255; 3] } else { [0; 3] })
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    /// Catalog with a mod and an NSFW mod sharing one thumbnail, a mod with
    /// two download files and one with a single file. The download lists of
    /// m1 and m2 and every file URL are 404, so an install can never reach the
    /// real addons folder.
    fn stub_api() -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub server");
        let url = format!("http://{}", server.server_addr().to_ip().expect("ip"));
        let catalog = format!(
            r#"[{{"id":"m1","remoteId":"1","name":"Test Mod","downloadable":true,"thumbnailUrl":"{url}/t.png"}},
               {{"id":"m2","remoteId":"2","name":"Spicy Skin","downloadable":true,"isNSFW":true,"thumbnailUrl":"{url}/t.png"}},
               {{"id":"m3","remoteId":"3","name":"Two Files","downloadable":true}},
               {{"id":"m4","remoteId":"4","name":"One File","downloadable":true}}]"#
        );
        let two = format!(
            r#"{{"downloads":[{{"url":"{url}/f/main.zip","name":"Main.zip","size":2500000,"md5Checksum":"x"}},
               {{"url":"{url}/f/alt.zip","name":"Alt colours.zip","size":0}}]}}"#
        );
        let one =
            format!(r#"{{"downloads":[{{"url":"{url}/f/one.zip","name":"One.zip","size":10}}]}}"#);
        let png = stub_png();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let resp = if req.url() == "/v2/mods" {
                    tiny_http::Response::from_string(catalog.clone())
                } else if req.url() == "/v2/mods/3/downloads" {
                    tiny_http::Response::from_string(two.clone())
                } else if req.url() == "/v2/mods/4/downloads" {
                    tiny_http::Response::from_string(one.clone())
                } else if req.url() == "/t.png" {
                    tiny_http::Response::from_data(png.clone())
                } else {
                    tiny_http::Response::from_string("not found").with_status_code(404)
                };
                let _ = req.respond(resp);
            }
        });
        url
    }

    /// A press and release at the middle of `el`, through the window (the
    /// testing backend's mock click needs mock time, which these tests do not use).
    fn pointer_click(app: &App, el: &ElementHandle) {
        let (pos, size) = (el.absolute_position(), el.size());
        pointer_click_at(app, pos.x + size.width / 2.0, pos.y + size.height / 2.0);
    }

    fn pointer_click_at(app: &App, x: f32, y: f32) {
        use slint::platform::{PointerEventButton, WindowEvent};
        let position = slint::LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        app.window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        app.window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
    }

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb(if (x / 8 + y / 8) % 2 == 0 {
                [250, 240, 230]
            } else {
                [10, 20, 30]
            })
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    /// Catalog for the detail page: a mod with three images (the third is a
    /// 404) and two download files, an NSFW mod with two images, and thirty
    /// fillers so the list scrolls. Every file URL is a 404, so installs never
    /// reach an addons folder.
    fn detail_api() -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub server");
        let url = format!("http://{}", server.server_addr().to_ip().expect("ip"));
        let mut mods = vec![
            format!(
                r#"{{"id":"m5","remoteId":"5","name":"Gallery Mod","downloadable":true,
                "author":"Ada","modAuthorId":"auth1","category":"Skins","hero":"Haze",
                "metadata":{{"donationLinks":[{{"url":"https://ko-fi.com/ada","platform":"Ko-fi"}}]}},"tags":["Sound"," ","Funny"],
                "likes":56,"downloadCount":1234,"remoteUrl":"https://gamebanana.com/mods/5",
                "remoteAddedAt":"2026-10-01T10:00:00Z","remoteUpdatedAt":"2026-11-05T09:00:00Z",
                "description":"<b>Hello</b> &amp; welcome.<br><br><ul><li>One</li><li>Two</li></ul><script>x()</script><a href=\"https://evil.test\">link</a>",
                "images":["{url}/g1.png","{url}/g2.png","{url}/g3.png"]}}"#
            ),
            format!(
                r#"{{"id":"m6","remoteId":"6","name":"Spicy Gallery","downloadable":true,"isNSFW":true,
                "remoteUrl":"javascript:alert(1)","images":["{url}/g1.png","{url}/g2.png"]}}"#
            ),
        ];
        for i in 1..=30 {
            mods.push(format!(
                r#"{{"id":"f{i}","remoteId":"10{i}","name":"Filler {i:02}","downloadable":true}}"#
            ));
        }
        let catalog = format!("[{}]", mods.join(","));
        let two = format!(
            r#"{{"downloads":[{{"url":"{url}/f/main.zip","name":"Main.zip","size":2500000}},
               {{"url":"{url}/f/alt.zip","name":"Alt.zip","size":0}}]}}"#
        );
        let (g1, g2) = (png_of(1920, 1080), png_of(800, 600));
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let resp = match req.url() {
                    "/v2/mods" => tiny_http::Response::from_string(catalog.clone()),
                    "/v2/mods/5/downloads" => tiny_http::Response::from_string(two.clone()),
                    "/g1.png" => tiny_http::Response::from_data(g1.clone()),
                    "/g2.png" => tiny_http::Response::from_data(g2.clone()),
                    _ => tiny_http::Response::from_string("not found").with_status_code(404),
                };
                let _ = req.respond(resp);
            }
        });
        url
    }

    /// Mod detail page: opened from a row, text from the catalog, gallery
    /// with lazy images, failed image placeholder, NSFW blur following the
    /// switch, install (with the file picker) sharing state with the list,
    /// the link, and Back restoring the list as it was and freeing images.
    fn detail_flow() {
        use browse::test_hooks as hooks;
        use i_slint_backend_testing::AccessibleRole;
        use std::sync::{Arc, Mutex};
        let api = detail_api();
        let app = App::new().expect("app");
        browse::attach(&app, &api);
        app.show().expect("show");
        let state = app.global::<BrowseState>();
        let ready = |a: &App| a.global::<BrowseState>().get_load_state() == LoadState::Ready;
        assert!(run_until(&app, Duration::from_secs(20), ready));
        let row_named = |name: &str| {
            ElementHandle::find_by_accessible_label(&app, name)
                .find(|e| e.accessible_role() == Some(AccessibleRole::ListItem))
                .unwrap_or_else(|| panic!("no row {name:?}"))
        };

        // List state before: sorted by name, scrolled down.
        state.set_sort_index(4);
        state.invoke_sort_selected(4);
        let first = |a: &App| {
            a.global::<BrowseState>()
                .get_mods()
                .row_data(0)
                .map(|r| r.name.to_string())
        };
        assert_eq!(first(&app).as_deref(), Some("Filler 01"));
        // Element searches can scroll the list, so the click position is
        // worked out before scrolling: rows are 152 px, the fourth one sits
        // at 456 px, 56 px into the list once it is scrolled by 400 px.
        let list = ElementHandle::find_by_element_id(&app, "BrowsePage::mod-list")
            .next()
            .expect("mod list");
        let top = list.absolute_position();
        state.set_list_scroll(-400.0);
        run_until(&app, Duration::from_millis(100), |_| false);
        assert_eq!(state.get_list_scroll(), -400.0);
        pointer_click_at(&app, top.x + 100.0, top.y + 56.0 + 76.0);
        assert!(state.get_detail_open());
        assert_eq!(hooks::detail_id().as_deref(), Some("f4"));
        assert_eq!(state.get_detail().name.as_str(), "Filler 04");
        assert_eq!(state.get_gallery_count(), 0);
        assert_eq!(state.get_gallery_state(), ImageState::None);
        assert_eq!(state.get_detail().description.as_str(), "");
        assert!(!state.get_detail().has_link);
        click(&app, "Back");
        assert!(!state.get_detail_open());
        assert_eq!(hooks::detail_id(), None);
        assert_eq!(state.get_sort_index(), 4);
        assert_eq!(state.get_list_scroll(), -400.0);
        assert_eq!(first(&app).as_deref(), Some("Filler 01"));
        assert_eq!(state.get_summary().as_str(), "32 mods");

        // Hero filter: pick Haze, then clear.
        assert_eq!(state.get_heroes().row_count(), 1);
        state.invoke_hero_toggled(0);
        assert_eq!(state.get_heroes_label().as_str(), "Haze");
        assert_eq!(state.get_summary().as_str(), "1 of 32 mods");
        state.invoke_heroes_cleared();
        assert_eq!(state.get_heroes_label().as_str(), "All heroes");
        assert_eq!(state.get_summary().as_str(), "32 mods");

        // Opened from another screen, Back returns there.
        assert!(!browse::show_detail_from("missing", 2));
        assert!(browse::show_detail_from("f4", 2));
        assert_eq!(app.get_screen(), 0);
        assert!(state.get_detail_open());
        click(&app, "Back");
        assert!(!state.get_detail_open());
        assert_eq!(app.get_screen(), 2);
        assert_eq!(state.get_return_screen(), -1);
        app.set_screen(0);

        // The GameBanana tabs load on first use and reset when the page closes.
        let gb = tiny_http::Server::http("127.0.0.1:0").expect("gb stub");
        let gb_url = format!("http://{}", gb.server_addr().to_ip().expect("ip"));
        std::thread::spawn(move || {
            for req in gb.incoming_requests() {
                let body = match req.url() {
                    u if u.starts_with("/Mod/5/ProfilePage") => {
                        r#"{"_aFiles":[{"_sFile":"main.zip","_nFilesize":2500000,"_tsDateAdded":1,"_nDownloadCount":9}]}"#
                    }
                    u if u.starts_with("/Mod/5/Updates") => {
                        r#"{"_aMetadata":{"_nRecordCount":1,"_bIsComplete":true},"_aRecords":[{"_sName":"Fix","_sVersion":"1.1","_tsDateAdded":1,"_aChangeLog":[{"cat":"Bugfix","text":"Fixed it"}]}]}"#
                    }
                    _ => r#"{"_sErrorCode":"COMMENT_MODE_HIDDEN"}"#,
                };
                let _ = req.respond(tiny_http::Response::from_string(body));
            }
        });
        detail_tabs::attach_with(&app, shiv_api::GameBananaClient::new(&gb_url));
        assert!(browse::show_detail_from("m5", 0));
        assert_eq!(state.get_detail_tab(), 0);
        assert_eq!(state.get_files_load(), TabLoad::Idle);
        state.invoke_detail_tab_selected(1);
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<BrowseState>().get_files_load() == TabLoad::Ready
        }));
        assert_eq!(state.get_files().row_count(), 1);
        assert_eq!(
            state.get_files().row_data(0).unwrap().title.as_str(),
            "main.zip"
        );
        state.invoke_detail_tab_selected(2);
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<BrowseState>().get_changes_load() == TabLoad::Ready
        }));
        let change = state.get_changes().row_data(0).unwrap();
        assert_eq!(change.title.as_str(), "Fix");
        assert!(change.body.contains("Bugfix: Fixed it"));
        assert!(!state.get_changes_more());
        state.invoke_detail_tab_selected(3);
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<BrowseState>().get_comments_load() == TabLoad::Hidden
        }));
        click(&app, "Back");
        assert_eq!(state.get_files().row_count(), 0);
        assert_eq!(state.get_files_load(), TabLoad::Idle);
        assert_eq!(state.get_detail_tab(), 0);

        // The author's page: Ada's mods only, then back to everything.
        assert!(browse::show_detail_from("m5", 2));
        let d = state.get_detail();
        assert_eq!(d.author_id.as_str(), "auth1");
        assert_eq!(d.donation_label.as_str(), "Support on Ko-fi");
        state.invoke_author_selected();
        assert!(!state.get_detail_open());
        assert_eq!(app.get_screen(), 0);
        assert_eq!(state.get_author_filter().as_str(), "Ada");
        assert_eq!(state.get_summary().as_str(), "1 of 32 mods");
        state.invoke_author_filter_cleared();
        assert_eq!(state.get_author_filter().as_str(), "");
        assert_eq!(state.get_summary().as_str(), "32 mods");

        // The gallery mod, opened with a real click on its row.
        state.invoke_search_edited("gallery".into());
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<BrowseState>().get_mods().row_count() == 2
        }));
        pointer_click(&app, &row_named("Gallery Mod"));
        assert!(state.get_detail_open());
        let d = state.get_detail();
        assert_eq!(d.name.as_str(), "Gallery Mod");
        assert_eq!(d.author.as_str(), "Ada");
        assert_eq!(d.kind.as_str(), "Skins \u{b7} Haze");
        assert_eq!(d.tags.as_str(), "Sound, Funny");
        assert_eq!((d.downloads.as_str(), d.likes.as_str()), ("1,234", "56"));
        assert_eq!(d.added.as_str(), "2026-10-01");
        assert!(d.updated.ends_with(" ago"), "{}", d.updated);
        assert_eq!(
            d.description.as_str(),
            "Hello & welcome.\n\n\u{2022} One\n\u{2022} Two\nlink"
        );
        assert!(d.has_link && !d.nsfw);

        // Gallery: the first image loads lazily, fitted to the gallery box.
        assert_eq!(state.get_gallery_count(), 3);
        assert_eq!(state.get_gallery_index(), 0);
        let image_ready =
            |a: &App| a.global::<BrowseState>().get_gallery_state() == ImageState::Ready;
        assert!(run_until(&app, Duration::from_secs(20), image_ready));
        let size = |a: &App| {
            let s = a.global::<BrowseState>().get_gallery_image().size();
            (s.width, s.height)
        };
        assert_eq!(size(&app), browse::GALLERY_BOX);
        // Strip thumbnails come from the list's thumbnail cache; the third
        // image is missing and keeps its placeholder.
        let strip = |a: &App| -> Vec<bool> {
            a.global::<BrowseState>()
                .get_gallery_thumbs()
                .iter()
                .map(|t| t.has_image)
                .collect()
        };
        assert!(run_until(&app, Duration::from_secs(20), move |a| strip(a)
            == [true, true, false]));
        let thumb_size = |a: &App, i: usize| {
            let s = a
                .global::<BrowseState>()
                .get_gallery_thumbs()
                .row_data(i)
                .expect("thumb")
                .image
                .size();
            (s.width, s.height)
        };
        assert_eq!(thumb_size(&app, 0), (thumbs::DISPLAY_W, thumbs::DISPLAY_H));

        click(&app, "Next");
        assert_eq!(state.get_gallery_index(), 1);
        assert!(run_until(&app, Duration::from_secs(20), image_ready));
        assert_eq!(size(&app), (720, 540));
        state.invoke_gallery_step(1);
        assert_eq!(state.get_gallery_index(), 2);
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            a.global::<BrowseState>().get_gallery_state() == ImageState::Failed
        }));
        assert_eq!(size(&app), (0, 0));
        // Wraps around; images seen before come from the cache at once.
        state.invoke_gallery_step(1);
        assert_eq!(state.get_gallery_index(), 0);
        assert_eq!(state.get_gallery_state(), ImageState::Ready);
        click(&app, "Previous");
        assert_eq!(state.get_gallery_index(), 2);
        assert_eq!(state.get_gallery_state(), ImageState::Failed);
        click(&app, "Image 2");
        assert_eq!(state.get_gallery_index(), 1);
        assert_eq!(state.get_gallery_state(), ImageState::Ready);
        state.invoke_gallery_select(7);
        assert_eq!(state.get_gallery_index(), 1);
        let (bytes, count) = hooks::gallery_usage();
        assert!(
            count <= 4 && bytes <= 4 * 960 * 540 * 4,
            "{bytes} bytes in {count}"
        );

        // Install from the detail page: the picker opens, and list and detail
        // show the same state throughout.
        let detail_state = |a: &App| {
            a.global::<BrowseState>()
                .get_detail_row()
                .row_data(0)
                .map(|r| r.state)
        };
        let list_state = |a: &App| {
            a.global::<BrowseState>()
                .get_mods()
                .row_data(0)
                .filter(|r| r.id == "m5")
                .map(|r| r.state)
        };
        let status = ElementHandle::find_by_element_id(&app, "ModDetailPage::status")
            .next()
            .expect("detail install controls");
        let in_status = |role: AccessibleRole, label: &str| {
            status
                .query_descendants()
                .match_accessible_role(role)
                .find_all()
                .into_iter()
                .find(|e| e.accessible_label().as_deref() == Some(label))
        };
        assert_eq!(detail_state(&app), Some(InstallState::Idle));
        in_status(AccessibleRole::Button, "Install")
            .expect("Install on the detail page")
            .invoke_accessible_default_action();
        assert_eq!(detail_state(&app), Some(InstallState::Downloading));
        assert_eq!(list_state(&app), Some(InstallState::Downloading));
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<BrowseState>()
            .get_picker_open()));
        assert_eq!(state.get_picker_title().as_str(), "Install Gallery Mod");
        state.invoke_pick_download(0);
        let job = hooks::job_generation("m5").expect("job started");
        assert_eq!(hooks::job_url("m5"), Some(format!("{api}/f/main.zip")));
        assert_eq!(hooks::in_flight("m5"), Some(job));
        assert_eq!(detail_state(&app), Some(InstallState::Downloading));
        assert_eq!(list_state(&app), Some(InstallState::Downloading));
        assert!(in_status(AccessibleRole::Button, "Cancel").is_some());
        // The download is a 404 (or the game is not found): the banner shows
        // on the detail page and both go back to Install.
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            a.global::<BrowseState>().get_banner_text() != "" && hooks::in_flight("m5").is_none()
        }));
        assert!(
            state
                .get_banner_text()
                .starts_with("Could not install Gallery Mod.")
        );
        assert_eq!(detail_state(&app), Some(InstallState::Idle));
        assert_eq!(list_state(&app), Some(InstallState::Idle));
        click(&app, "Dismiss");
        // An install seen on disk shows on both.
        hooks::installed_read(&["m5"], false);
        assert_eq!(detail_state(&app), Some(InstallState::Installed));
        assert_eq!(list_state(&app), Some(InstallState::Installed));
        assert!(in_status(AccessibleRole::Button, "Install").is_none());
        hooks::installed_read(&[], false);
        assert_eq!(detail_state(&app), Some(InstallState::Idle));

        // Open on GameBanana goes through the opener (stubbed here).
        let opened = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = opened.clone();
        hooks::set_opener(move |url| {
            log.lock().expect("log").push(url.to_owned());
            Err("no browser here".into())
        });
        click(&app, "Open on GameBanana");
        let seen = opened.clone();
        assert!(run_until(&app, Duration::from_secs(10), |a| {
            a.global::<BrowseState>().get_banner_text() != ""
        }));
        assert_eq!(
            *seen.lock().expect("log"),
            ["https://gamebanana.com/mods/5"]
        );
        assert_eq!(
            state.get_banner_text().as_str(),
            "Could not open the link. no browser here"
        );
        click(&app, "Dismiss");

        // Back: the list is as it was, every decoded gallery image is gone,
        // and a result arriving late is dropped.
        click(&app, "Back");
        assert!(!state.get_detail_open());
        assert_eq!(hooks::gallery_usage(), (0, 0));
        assert_eq!(state.get_gallery_thumbs().row_count(), 0);
        assert_eq!(size(&app), (0, 0));
        hooks::late_gallery_result(&format!("{api}/g1.png"));
        assert_eq!(hooks::gallery_usage(), (0, 0));
        assert_eq!(state.get_mods().row_count(), 2);
        assert_eq!(state.get_sort_index(), 4);

        // NSFW: everything image-based is blurred until Show NSFW is on, and
        // the open page follows the switch both ways. A non-https link is
        // not offered.
        row_named("Spicy Gallery").invoke_accessible_default_action();
        assert!(state.get_detail().nsfw);
        assert!(!state.get_detail().has_link);
        // Loading or blurred, never the full image while the switch is off.
        let blurred = (browse::GALLERY_BOX.0 / 4, browse::GALLERY_BOX.1 / 4);
        assert!(run_until(&app, Duration::from_secs(20), move |a| {
            a.global::<BrowseState>().get_gallery_state() == ImageState::Ready
        }));
        assert_eq!(size(&app), blurred);
        assert!(run_until(&app, Duration::from_secs(20), move |a| strip(a)
            == [true, true]));
        assert_eq!(
            thumb_size(&app, 0),
            (thumbs::OBSCURED_W, thumbs::OBSCURED_H)
        );
        assert_eq!(
            thumb_size(&app, 1),
            (thumbs::OBSCURED_W, thumbs::OBSCURED_H)
        );
        let switch = ElementHandle::find_by_element_id(&app, "ModDetailPage::nsfw-switch")
            .next()
            .expect("Show NSFW on the detail page");
        switch.invoke_accessible_default_action();
        assert!(state.get_show_nsfw());
        assert_eq!(size(&app), browse::GALLERY_BOX);
        assert_eq!(thumb_size(&app, 0), (thumbs::DISPLAY_W, thumbs::DISPLAY_H));
        state.invoke_gallery_step(1);
        assert!(run_until(&app, Duration::from_secs(20), image_ready));
        assert_eq!(size(&app), (720, 540));
        switch.invoke_accessible_default_action();
        assert!(!state.get_show_nsfw());
        assert_eq!(size(&app), (180, 135));
        assert_eq!(
            thumb_size(&app, 1),
            (thumbs::OBSCURED_W, thumbs::OBSCURED_H)
        );
        click(&app, "Back");
        assert_eq!(hooks::gallery_usage(), (0, 0));
    }

    /// Installed thumbnails follow Show NSFW too: a mod whose sidecar
    /// records NSFW and an older sidecar whose mod the catalog marks NSFW
    /// are blurred, the switch on Browse unblurs them, and without a loaded
    /// catalog an older sidecar (no flag) is blurred until the catalog says
    /// it is not NSFW.
    fn installed_nsfw_flow(api: &str) {
        use shiv_core::game::GamePaths;
        use shiv_core::sidecar::{self, Sidecar};
        use std::sync::Arc;

        let root = tempfile::tempdir().expect("tempdir");
        let paths = GamePaths::from_root(root.path().to_path_buf());
        std::fs::create_dir_all(&paths.addons).expect("addons");
        let thumb = format!("{api}/t.png");
        for (i, (id, name, nsfw)) in [
            ("m1", "Plain Mod", Some(false)),
            // Installed before the flag existed; the catalog marks m2 NSFW.
            ("m2", "Old Spicy", None),
            // Installed before the flag existed; the catalog says not NSFW.
            ("m3", "Old Plain", None),
            // Flagged at install; no longer in the catalog.
            ("gone", "Flagged Spicy", Some(true)),
        ]
        .into_iter()
        .enumerate()
        {
            let vpk = format!("pak{:02}_dir.vpk", i + 1);
            std::fs::write(paths.addons.join(&vpk), b"x").expect("vpk");
            sidecar::write(
                &paths.addons,
                &Sidecar {
                    mod_id: id.into(),
                    name: name.into(),
                    version: None,
                    thumbnail_url: Some(thumb.clone()),
                    vpks: vec![vpk],
                    nsfw,
                },
            )
            .expect("sidecar");
        }
        let deps = |p: &GamePaths| {
            let located = p.clone();
            installed::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                running: Arc::new(|| false),
            }
        };
        let width = |a: &App, name: &str| {
            a.global::<InstalledState>()
                .get_rows()
                .iter()
                .find(|r| r.name.as_str() == name)
                .filter(|r| r.has_thumb)
                .map(|r| r.thumb.size().width)
        };
        let blurred = Some(thumbs::OBSCURED_W);
        let full = Some(thumbs::DISPLAY_W);

        let app = App::new().expect("app");
        let cache = browse::attach(&app, api);
        installed::attach_with(&app, cache, deps(&paths));
        app.show().expect("show");
        let catalog_ready =
            |a: &App| a.global::<BrowseState>().get_load_state() == LoadState::Ready;
        assert!(run_until(&app, Duration::from_secs(20), catalog_ready));
        app.set_screen(1);
        let shown = move |a: &App| {
            width(a, "Plain Mod") == full
                && width(a, "Old Plain") == full
                && width(a, "Old Spicy") == blurred
                && width(a, "Flagged Spicy") == blurred
        };
        assert!(run_until(&app, Duration::from_secs(20), shown));

        // The Browse switch unblurs Installed too, and blurs it again.
        let browse_state = app.global::<BrowseState>();
        browse_state.invoke_toggle_nsfw();
        assert_eq!(width(&app, "Old Spicy"), full);
        assert_eq!(width(&app, "Flagged Spicy"), full);
        assert_eq!(width(&app, "Plain Mod"), full);
        assert_eq!(width(&app, "Old Plain"), full);
        browse_state.invoke_toggle_nsfw();
        assert!(shown(&app));

        // No catalog (service unreachable): fail closed. Recorded NSFW and
        // both older sidecars are blurred; only recorded not-NSFW is shown.
        let app2 = App::new().expect("app");
        let cache2 = browse::attach(&app2, "http://127.0.0.1:9");
        installed::attach_with(&app2, cache2, deps(&paths));
        app2.show().expect("show");
        assert!(run_until(&app2, Duration::from_secs(20), |a| {
            a.global::<BrowseState>().get_load_state() == LoadState::Failed
        }));
        app2.set_screen(1);
        assert!(run_until(&app2, Duration::from_secs(20), move |a| {
            width(a, "Flagged Spicy") == blurred
                && width(a, "Old Spicy") == blurred
                && width(a, "Old Plain") == blurred
                && width(a, "Plain Mod") == full
        }));
        // Once a catalog loads, the older sidecar it calls not NSFW unblurs.
        browse::test_hooks::catalog_loaded_with_nsfw(&["m2"]);
        assert_eq!(width(&app2, "Old Plain"), full);
        assert_eq!(width(&app2, "Old Spicy"), blurred);
        assert_eq!(width(&app2, "Flagged Spicy"), blurred);
        assert_eq!(width(&app2, "Plain Mod"), full);
    }

    /// NSFW mods are listed with a tag and a blurred thumbnail; Show NSFW
    /// shows the real one and turns back off for a new session; search still
    /// finds them.
    fn nsfw_flow(api: &str) {
        use i_slint_backend_testing::AccessibleRole;
        let app = App::new().expect("app");
        browse::attach(&app, api);
        app.show().expect("show");
        let state = app.global::<BrowseState>();
        let thumbs_loaded = |a: &App| {
            let mods = a.global::<BrowseState>().get_mods();
            mods.row_count() == 4
                && mods.row_data(0).is_some_and(|r| r.has_thumb)
                && mods.row_data(1).is_some_and(|r| r.has_thumb)
        };
        assert!(run_until(&app, Duration::from_secs(20), thumbs_loaded));
        assert!(!state.get_show_nsfw());
        let row = |i: usize| state.get_mods().row_data(i).expect("row");
        assert!(!row(0).nsfw);
        assert!(row(1).nsfw);
        assert_eq!(row(1).name.as_str(), "Spicy Skin");
        let size = |i: usize| {
            let s = row(i).thumb.size();
            (s.width, s.height)
        };
        assert_eq!(size(0), (thumbs::DISPLAY_W, thumbs::DISPLAY_H));
        // Same source image, but only the reduced blurred copy is shown.
        assert_eq!(size(1), (thumbs::OBSCURED_W, thumbs::OBSCURED_H));

        let switch = ElementHandle::find_by_accessible_label(&app, "Show NSFW")
            .find(|e| e.accessible_role() == Some(AccessibleRole::Switch))
            .expect("Show NSFW switch");
        switch.invoke_accessible_default_action();
        assert!(state.get_show_nsfw());
        assert_eq!(size(1), (thumbs::DISPLAY_W, thumbs::DISPLAY_H));
        switch.invoke_accessible_default_action();
        assert!(!state.get_show_nsfw());
        assert_eq!(size(1), (thumbs::OBSCURED_W, thumbs::OBSCURED_H));

        // Search finds NSFW mods too.
        state.invoke_search_edited("spicy".into());
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<BrowseState>().get_mods().row_count() == 1
        }));
        assert!(row(0).nsfw);
        assert_eq!(state.get_summary().as_str(), "1 of 4 mods");

        // A new session starts with NSFW blurred again.
        switch.invoke_accessible_default_action();
        let app2 = App::new().expect("app");
        browse::attach(&app2, api);
        assert!(!app2.global::<BrowseState>().get_show_nsfw());
        assert!(run_until(&app2, Duration::from_secs(20), thumbs_loaded));
        let r = app2
            .global::<BrowseState>()
            .get_mods()
            .row_data(1)
            .expect("row");
        assert_eq!(r.thumb.size().width, thumbs::OBSCURED_W);
    }

    /// Install shows progress state; Cancel returns to idle at once and later
    /// worker updates are ignored; a failing install surfaces a readable banner.
    fn install_cancel_and_failure(api: &str) {
        let app = App::new().expect("app");
        browse::attach(&app, api);
        app.show().expect("show");
        let state = app.global::<BrowseState>();
        let ready = |a: &App| a.global::<BrowseState>().get_load_state() == LoadState::Ready;
        assert!(
            run_until(&app, Duration::from_secs(20), ready),
            "{:?} {}",
            state.get_load_state(),
            state.get_error_text()
        );
        let row_state = |a: &App| {
            a.global::<BrowseState>()
                .get_mods()
                .row_data(0)
                .map(|r| r.state)
        };
        assert_eq!(row_state(&app), Some(InstallState::Idle));

        // The Installed badge follows re-reads of the addons folder (Browse
        // shown, or an uninstall on Installed); a superseded read is ignored.
        use browse::test_hooks as hooks;
        hooks::installed_read(&["m1"], false);
        assert_eq!(row_state(&app), Some(InstallState::Installed));
        hooks::installed_read(&[], true);
        assert_eq!(row_state(&app), Some(InstallState::Installed));
        hooks::installed_read(&[], false);
        assert_eq!(row_state(&app), Some(InstallState::Idle));

        // Deterministic: worker posts are only applied while the event loop
        // runs, so nothing below races with the real worker.
        // Install first asks for the download list; nothing is in flight yet.
        state.invoke_install("m1".into());
        assert_eq!(row_state(&app), Some(InstallState::Downloading));
        assert!(hooks::resolving("m1"));
        assert_eq!(hooks::in_flight("m1"), None);
        assert_eq!(hooks::job_generation("m1"), None);
        // One file: the job starts at once (the real list request's answer,
        // a 404, is ignored from now on).
        hooks::resolve("m1", &["http://127.0.0.1:9/never.zip"]);
        assert!(!hooks::resolving("m1"));
        assert!(!state.get_picker_open());
        assert_eq!(row_state(&app), Some(InstallState::Downloading));
        let first = hooks::job_generation("m1").expect("job");
        assert_eq!(hooks::in_flight("m1"), Some(first));
        // A second Install while the job runs is a no-op.
        state.invoke_install("m1".into());
        assert_eq!(hooks::job_generation("m1"), Some(first));

        // Cancel: the job goes, but the worker is still in flight, so the row
        // shows Cancelling and Install stays refused (no duplicate pak).
        state.invoke_cancel("m1".into());
        assert_eq!(hooks::job_generation("m1"), None);
        assert_eq!(row_state(&app), Some(InstallState::Cancelling));
        state.invoke_install("m1".into());
        assert_eq!(hooks::job_generation("m1"), None);
        assert_eq!(hooks::in_flight("m1"), Some(first));

        // Stale progress from the cancelled worker is ignored.
        hooks::post_progress("m1", first);
        assert_eq!(hooks::job_generation("m1"), None);
        assert_eq!(row_state(&app), Some(InstallState::Cancelling));
        // Its Done (an error) clears the marker without a banner.
        hooks::post_done_error("m1", first);
        assert_eq!(hooks::in_flight("m1"), None);
        assert_eq!(row_state(&app), Some(InstallState::Idle));
        assert_eq!(state.get_banner_text(), "");
        // Drain the real worker's queued posts: still idle, still no banner.
        run_until(&app, Duration::from_millis(500), |_| false);
        assert_eq!(row_state(&app), Some(InstallState::Idle));
        assert_eq!(state.get_banner_text(), "");

        state.invoke_install("m1".into());
        let failed = |a: &App| a.global::<BrowseState>().get_banner_text() != "";
        assert!(run_until(&app, Duration::from_secs(20), failed));
        assert!(
            state
                .get_banner_text()
                .starts_with("Could not install Test Mod.")
        );
        assert_eq!(row_state(&app), Some(InstallState::Idle));
        click(&app, "Dismiss");
        assert_eq!(state.get_banner_text(), "");
    }

    /// More than one download file opens the picker; nothing is in flight
    /// until a file is chosen, Cancel leaves no trace, and the chosen file is
    /// the one installed. One file installs without asking.
    fn download_picker_flow(api: &str) {
        use browse::test_hooks as hooks;
        let app = App::new().expect("app");
        browse::attach(&app, api);
        app.show().expect("show");
        let state = app.global::<BrowseState>();
        let ready = |a: &App| a.global::<BrowseState>().get_load_state() == LoadState::Ready;
        assert!(run_until(&app, Duration::from_secs(20), ready));
        let row_state = |a: &App, i: usize| {
            a.global::<BrowseState>()
                .get_mods()
                .row_data(i)
                .map(|r| r.state)
        };
        let picker = |a: &App| a.global::<BrowseState>().get_picker_open();

        state.invoke_install("m3".into());
        assert!(run_until(&app, Duration::from_secs(20), picker));
        assert_eq!(state.get_picker_title().as_str(), "Install Two Files");
        let files: Vec<(String, String)> = state
            .get_picker_files()
            .iter()
            .map(|r| (r.name.to_string(), r.size.to_string()))
            .collect();
        assert_eq!(
            files,
            [
                ("Main.zip".into(), "2.5 MB".into()),
                ("Alt colours.zip".into(), "Size unknown".into())
            ]
        );
        assert_eq!(hooks::in_flight("m3"), None);
        assert_eq!(hooks::job_generation("m3"), None);
        assert!(!hooks::resolving("m3"));
        assert_eq!(row_state(&app, 2), Some(InstallState::Idle));
        // Other installs wait while the picker is open.
        state.invoke_install("m4".into());
        assert!(!hooks::resolving("m4") && hooks::job_generation("m4").is_none());

        // Cancel: closed, nothing in flight, no banner.
        state.invoke_cancel_pick();
        assert!(!state.get_picker_open());
        assert_eq!(state.get_picker_files().row_count(), 0);
        assert_eq!(hooks::in_flight("m3"), None);
        assert_eq!(row_state(&app, 2), Some(InstallState::Idle));
        run_until(&app, Duration::from_millis(300), |_| false);
        assert_eq!(state.get_banner_text(), "");

        // Choosing a file starts the job for that file.
        state.invoke_install("m3".into());
        assert!(run_until(&app, Duration::from_secs(20), picker));
        state.invoke_pick_download(1);
        assert!(!state.get_picker_open());
        let job = hooks::job_generation("m3").expect("job started");
        assert_eq!(hooks::in_flight("m3"), Some(job));
        assert_eq!(
            hooks::job_url("m3").as_deref(),
            Some(format!("{api}/f/alt.zip").as_str())
        );
        // Its download is a 404 (or the game is not found): a banner, marker cleared.
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            a.global::<BrowseState>().get_banner_text() != ""
        }));
        assert!(
            state
                .get_banner_text()
                .starts_with("Could not install Two Files."),
            "{}",
            state.get_banner_text()
        );
        assert_eq!(hooks::in_flight("m3"), None);
        click(&app, "Dismiss");

        // A file picked for a mod that left the list meanwhile: a short
        // banner, nothing started.
        hooks::pick_for_vanished_mod();
        assert!(!state.get_picker_open());
        assert!(
            state.get_banner_text().contains("no longer in the list"),
            "{}",
            state.get_banner_text()
        );
        assert_eq!(hooks::in_flight("vanished"), None);
        click(&app, "Dismiss");

        // Cancel while the list is still loading: back to idle, no picker later.
        state.invoke_install("m3".into());
        assert!(hooks::resolving("m3"));
        state.invoke_cancel("m3".into());
        assert!(!hooks::resolving("m3"));
        assert_eq!(row_state(&app, 2), Some(InstallState::Idle));
        run_until(&app, Duration::from_millis(500), |_| false);
        assert!(!state.get_picker_open());
        assert_eq!(state.get_banner_text(), "");

        // One file: no picker, the job starts.
        state.invoke_install("m4".into());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            hooks::job_generation("m4").is_some()
                || hooks::in_flight("m4").is_some()
                || a.global::<BrowseState>().get_banner_text() != ""
        }));
        assert!(!state.get_picker_open());
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            a.global::<BrowseState>().get_banner_text() != "" && hooks::in_flight("m4").is_none()
        }));
        assert!(
            state
                .get_banner_text()
                .starts_with("Could not install One File."),
            "{}",
            state.get_banner_text()
        );
    }

    #[cfg(unix)]
    fn profiles_flow() {
        use shiv_core::game::GamePaths;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        std::fs::create_dir_all(&paths.addons).expect("addons");
        std::fs::write(paths.addons.join("pak01_dir.vpk"), "x").expect("vpk");
        let (located, running) = (paths.clone(), Arc::new(AtomicBool::new(false)));
        let flag = running.clone();
        let app = App::new().expect("app");
        profiles::attach_with(
            &app,
            profiles::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                running: Arc::new(move || flag.load(Ordering::Relaxed)),
            },
        );
        app.show().expect("show");
        let state = app.global::<ProfilesState>();
        fn ready(a: &App) -> bool {
            let s = a.global::<ProfilesState>();
            s.get_load_state() == LoadState::Ready && !s.get_busy()
        }
        assert!(run_until(&app, Duration::from_secs(5), ready));
        assert!(!state.get_set_up());
        assert!(paths.addons.join("pak01_dir.vpk").is_file());

        state.invoke_setup();
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            ready(a) && a.global::<ProfilesState>().get_set_up()
        }));
        assert_eq!(state.get_rows().row_count(), 1);
        assert_eq!(
            state.get_rows().row_data(0).unwrap().detail.as_str(),
            "1 mod"
        );

        state.invoke_create("Alt".into());
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            ready(a) && a.global::<ProfilesState>().get_rows().row_count() == 2
        }));
        state.invoke_switch_index(1);
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            ready(a) && a.global::<ProfilesState>().get_active_index() == 1
        }));
        assert!(!paths.addons.join("pak01_dir.vpk").exists());

        running.store(true, Ordering::Relaxed);
        state.invoke_switch_index(0);
        assert!(run_until(&app, Duration::from_secs(5), |a| {
            a.global::<ProfilesState>()
                .get_banner_text()
                .contains("Deadlock is running")
        }));
        assert_eq!(state.get_active_index(), 1);
    }
}
