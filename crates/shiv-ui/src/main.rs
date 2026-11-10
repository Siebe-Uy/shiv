//! Shiv desktop UI: a thin Slint shell over shiv-core and shiv-api.

slint::include_modules!();

use shiv_core::game::{self, GameError, GamePaths};

mod browse;
mod crosshair;
mod installed;
mod launch;
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

fn main() -> Result<(), slint::PlatformError> {
    let app = App::new()?;
    // Override for testing (e.g. an unreachable URL to see the Retry state).
    let api_url = std::env::var("SHIV_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let cache = browse::attach(&app, &api_url);
    installed::attach(&app, cache);
    launch::attach(&app);
    repair::attach(&app);
    crosshair::attach(&app);
    // Debug builds only: open on another screen (e.g. for screenshots); 0 is Browse.
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("SHIV_SCREEN")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        app.set_screen(n.clamp(0, 4));
        // Setting the screen from Rust does not run `changed screen`.
        match app.get_screen() {
            2 => app.global::<CrosshairState>().invoke_shown(),
            4 => app.global::<RepairState>().invoke_shown(),
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
        repair_flow();
        crosshair_flow();
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
    }

    /// Crosshair against a temp install and a temp Steam userdata folder
    /// (never the real ones): showing the screen loads the hero settings
    /// off-thread; a slider edit updates the values the preview draws and the
    /// commands; a running game refuses Apply; Apply writes the hero settings,
    /// keeps the user's other settings and drops the old autoexec block;
    /// edits are dropped while an apply writes; unapplied edits survive a
    /// re-show; a missing Steam shows a clear error.
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

    /// Launch against a temp install with a fake launcher (Steam is never
    /// started): the startup check fills the sidebar quietly; Launch shows the
    /// dialog; Cancel launches nothing; Launch anyway after Review re-checks
    /// and shows changed findings instead of launching; screen changes refresh
    /// the count; a clean install launches at once; an opener failure shows.
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
        let (counter, failing) = (launches.clone(), fail.clone());
        let located = paths.clone();
        let app = App::new().expect("app");
        launch::attach_with(
            &app,
            launch::Deps {
                locate: Arc::new(move || Ok(located.clone())),
                check: Arc::new(shiv_core::health::check),
                launch: Arc::new(move |_| {
                    if failing.load(Ordering::SeqCst) {
                        return Err(LaunchError::OpenerFailed {
                            program: "xdg-open".into(),
                            status: "exit status: 4".into(),
                        });
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
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
        let launched = |n: usize| {
            let l = launches.clone();
            move |a: &App| !a.global::<LaunchState>().get_busy() && l.load(Ordering::SeqCst) == n
        };
        let dialog = |a: &App| a.global::<LaunchState>().get_dialog_open();

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
        // These findings are fresh: Launch anyway launches directly.
        click(&app, "Launch anyway");
        assert!(run_until(&app, Duration::from_secs(20), launched(1)));
        assert!(state.get_status_text().contains("starting"));

        // Review with unchanged findings: the re-check matches, so it launches.
        click(&app, "Review");
        click(&app, "Launch anyway");
        assert!(run_until(&app, Duration::from_secs(20), launched(2)));
        assert!(!state.get_dialog_open());

        // Fixing the files elsewhere: a screen change refreshes the count.
        std::fs::remove_file(&bad).expect("remove");
        std::fs::remove_file(&bad2).expect("remove");
        app.set_screen(2);
        assert!(run_until(&app, Duration::from_secs(20), count(0)));

        // Clean: Launch goes straight through.
        click(&app, "Launch");
        assert!(run_until(&app, Duration::from_secs(20), launched(3)));
        assert!(!state.get_dialog_open());

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
        assert_eq!(launches.load(Ordering::SeqCst), 3);
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
