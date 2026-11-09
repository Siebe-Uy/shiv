//! Shiv desktop UI: a thin Slint shell over shiv-core and shiv-api.

slint::include_modules!();

use shiv_core::game::{self, GameError, GamePaths};

mod browse;
mod installed;
mod launch;
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

fn main() -> Result<(), slint::PlatformError> {
    let app = App::new()?;
    // Override for testing (e.g. an unreachable URL to see the Retry state).
    let api_url = std::env::var("SHIV_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let cache = browse::attach(&app, &api_url);
    installed::attach(&app, cache);
    launch::attach(&app);
    // Debug builds only: open on another screen (e.g. for screenshots); 0 is Browse.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("SHIV_SCREEN")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        app.set_screen(n.clamp(0, 4));
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

    /// Run the event loop until `cond` holds or `timeout` passes.
    fn run_until(app: &App, timeout: Duration, cond: impl Fn(&App) -> bool + 'static) -> bool {
        let met = Rc::new(Cell::new(false));
        let flag = met.clone();
        let weak = app.as_weak();
        let start = Instant::now();
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(10), move || {
            let done = weak.upgrade().is_some_and(|a| cond(&a));
            if done || start.elapsed() > timeout {
                flag.set(done);
                let _ = slint::quit_event_loop();
            }
        });
        slint::run_event_loop().expect("event loop");
        met.get()
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

        for (i, screen) in ["Installed", "Crosshair", "Performance", "Repair", "Browse"]
            .iter()
            .enumerate()
        {
            click(&app, screen);
            assert_eq!(app.get_screen(), ((i + 1) % 5) as i32, "{screen}");
        }
        install_cancel_and_failure(&stub_api());
        installed_screen();
        launch_flow();
    }

    /// Launch against a temp install with a fake launcher (Steam is never
    /// started): the startup check reports problems, Launch shows the dialog,
    /// Cancel launches nothing, Launch anyway does, and a clean install
    /// launches at once without the dialog.
    fn launch_flow() {
        use shiv_core::game::GamePaths;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let root = tempfile::tempdir().expect("tempdir");
        let paths = GamePaths::from_root(root.path().to_path_buf());
        std::fs::create_dir_all(&paths.addons).expect("addons");
        std::fs::write(
            &paths.gameinfo,
            "GameInfo { FileSystem { SearchPaths { Game citadel/addons\n Game citadel } } }",
        )
        .expect("gameinfo");
        let bad = paths.addons.join("pak01_dir.vpk");
        std::fs::write(&bad, b"").expect("zero-byte vpk");

        let launches = Arc::new(AtomicUsize::new(0));
        let counter = launches.clone();
        let located = paths.clone();
        let app = App::new().expect("app");
        launch::attach_with(
            &app,
            launch::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                check: Arc::new(shiv_core::health::check),
                launch: Arc::new(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            },
        );
        app.show().expect("show");
        let state = app.global::<LaunchState>();
        let idle = |a: &App| !a.global::<LaunchState>().get_busy();

        // Startup check: findings shown in the sidebar, no dialog, no launch.
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(state.get_findings().row_count(), 1);
        assert_eq!(state.get_problems_text().as_str(), "1 problem found");
        assert!(!state.get_dialog_open());
        assert_eq!(launches.load(Ordering::SeqCst), 0);

        // Launch: re-checks, then asks.
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| a
            .global::<LaunchState>()
            .get_dialog_open()));
        let row = state.get_findings().row_data(0).expect("row");
        assert_eq!(row.title.as_str(), "Empty VPK");
        assert_eq!(row.file.as_str(), "addons/pak01_dir.vpk");
        click(&app, "Cancel");
        assert!(!state.get_dialog_open());
        run_until(&app, Duration::from_millis(200), |_| false);
        assert_eq!(launches.load(Ordering::SeqCst), 0);

        // Review reopens it; Launch anyway launches.
        click(&app, "Review");
        assert!(state.get_dialog_open());
        click(&app, "Launch anyway");
        assert!(!state.get_dialog_open());
        assert!(run_until(&app, Duration::from_secs(20), idle));
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        assert!(!state.get_status_error());
        assert!(state.get_status_text().contains("starting"));

        // Clean install: Launch goes straight through.
        std::fs::remove_file(&bad).expect("remove");
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), |a| {
            !a.global::<LaunchState>().get_busy()
                && a.global::<LaunchState>().get_status_text() != ""
        }));
        assert!(!state.get_dialog_open());
        assert_eq!(state.get_findings().row_count(), 0);
        assert_eq!(launches.load(Ordering::SeqCst), 2);
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
        let deps = installed::Deps {
            locate: Arc::new(move || {
                while gate_in.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(located.clone())
            }),
            running: Arc::new(move || flag.load(Ordering::Relaxed)),
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

    /// Catalog with one mod; every other path (the downloads list) is 404, so
    /// an install can never reach the real addons folder.
    fn stub_api() -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub server");
        let url = format!("http://{}", server.server_addr().to_ip().expect("ip"));
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let resp = if req.url() == "/v2/mods" {
                    tiny_http::Response::from_string(
                        r#"[{"id":"m1","remoteId":"1","name":"Test Mod","downloadable":true}]"#,
                    )
                } else {
                    tiny_http::Response::from_string("not found").with_status_code(404)
                };
                let _ = req.respond(resp);
            }
        });
        url
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

        // Deterministic: worker posts are only applied while the event loop
        // runs, so nothing below races with the real worker.
        use browse::test_hooks as hooks;
        state.invoke_install("m1".into());
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
}
