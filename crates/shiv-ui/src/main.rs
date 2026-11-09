//! Shiv desktop UI: a thin Slint shell over shiv-core and shiv-api.

slint::include_modules!();

mod browse;
mod thumbs;

/// The API serves its routes under `/api` (`/api/v2/mods`, ...).
const DEFAULT_API_URL: &str = "https://api.deadlockmods.app/api";

fn main() -> Result<(), slint::PlatformError> {
    let app = App::new()?;
    // Override for testing (e.g. an unreachable URL to see the Retry state).
    let api_url = std::env::var("SHIV_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    browse::attach(&app, &api_url);
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

        state.invoke_install("m1".into());
        assert_eq!(row_state(&app), Some(InstallState::Downloading));
        state.invoke_cancel("m1".into());
        assert_eq!(row_state(&app), Some(InstallState::Idle));
        // The cancelled worker still finishes (with an error); nothing may change.
        assert!(!run_until(&app, Duration::from_millis(1500), |a| {
            a.global::<BrowseState>().get_banner_text() != ""
        }));
        assert_eq!(row_state(&app), Some(InstallState::Idle));

        state.invoke_install("m1".into());
        let failed = |a: &App| a.global::<BrowseState>().get_banner_text() != "";
        assert!(run_until(&app, Duration::from_secs(20), failed));
        assert!(state.get_banner_text().starts_with("Could not install Test Mod."));
        assert_eq!(row_state(&app), Some(InstallState::Idle));
        click(&app, "Dismiss");
        assert_eq!(state.get_banner_text(), "");
    }
}
