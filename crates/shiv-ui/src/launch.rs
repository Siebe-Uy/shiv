//! Launch controller: runs the health check on app start and on every Launch
//! click, launches at once when clean, and otherwise shows the findings with
//! Launch anyway / Cancel.
//!
//! Threading matches `installed`: locating the game, the check and the launch
//! run on worker threads that post back with `upgrade_in_event_loop`. A
//! generation counter drops results that a newer request has superseded.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use shiv_core::game::GamePaths;
use shiv_core::health::{self, Finding, FindingKind};
use shiv_core::launch::{self, LaunchError};
use slint::{ComponentHandle, Model as _, ModelRc, VecModel, Weak};

use crate::browse::game_error_text;
use crate::installed::Locate;
use crate::{App, FindingRow, LaunchState};

pub(crate) type Check = Arc<dyn Fn(&GamePaths) -> Vec<Finding> + Send + Sync>;
pub(crate) type Launcher = Arc<dyn Fn(&GamePaths) -> Result<(), LaunchError> + Send + Sync>;

/// Injectable so tests never touch the real install or start Steam.
#[derive(Clone)]
pub(crate) struct Deps {
    pub locate: Locate,
    pub check: Check,
    pub launch: Launcher,
}

impl Deps {
    fn system() -> Self {
        Deps {
            locate: Arc::new(crate::locate_game),
            check: Arc::new(health::check),
            launch: Arc::new(launch::launch),
        }
    }
}

thread_local! {
    static LAUNCH: RefCell<Option<Rc<Launch>>> = const { RefCell::new(None) };
}

fn with_launch(f: impl FnOnce(&Rc<Launch>)) {
    if let Some(l) = LAUNCH.with(|c| c.borrow().clone()) {
        f(&l);
    }
}

// ---------------------------------------------------------------------------
// View-models (pure, unit-tested)

pub(crate) fn title(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::UnreadableVpk => "Unreadable VPK",
        FindingKind::TruncatedVpk => "Incomplete VPK",
        FindingKind::ZeroByte => "Empty VPK",
        FindingKind::PartialDownload => "Leftover partial download",
        FindingKind::DuplicatePak => "Duplicate pak number",
        FindingKind::MissingSearchPath => "Addons search path missing",
        FindingKind::GameinfoUnwritable => "gameinfo.gi not writable",
    }
}

/// The file relative to `game/citadel`, where every checked file lives.
fn display_file(file: Option<&Path>, paths: &GamePaths) -> String {
    let Some(file) = file else {
        return String::new();
    };
    let base = paths.gameinfo.parent().unwrap_or(&paths.root);
    file.strip_prefix(base)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn rows(findings: &[Finding], paths: &GamePaths) -> Vec<FindingRow> {
    findings
        .iter()
        .map(|f| FindingRow {
            title: title(f.kind).into(),
            file: display_file(f.file.as_deref(), paths).into(),
            detail: f.detail.as_str().into(),
        })
        .collect()
}

pub(crate) fn problems_text(n: usize) -> String {
    match n {
        1 => "1 problem found".into(),
        n => format!("{n} problems found"),
    }
}

pub(crate) fn problems_short(n: usize) -> String {
    match n {
        1 => "1 problem".into(),
        n => format!("{n} problems"),
    }
}

// ---------------------------------------------------------------------------
// Controller

#[derive(Clone, Copy, PartialEq, Eq)]
enum Purpose {
    Startup,
    Launch,
}

struct Launch {
    weak: Weak<App>,
    deps: Deps,
    rows: Rc<VecModel<FindingRow>>,
    /// Where the last check ran; Launch anyway launches that install.
    paths: RefCell<Option<GamePaths>>,
    generation: Cell<u64>,
}

/// Wire Launch to `app` and run the startup check.
pub fn attach(app: &App) {
    attach_with(app, Deps::system());
}

pub(crate) fn attach_with(app: &App, deps: Deps) {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(Launch {
        weak: app.as_weak(),
        deps,
        rows: rows.clone(),
        paths: RefCell::new(None),
        generation: Cell::new(0),
    });
    LAUNCH.with(|c| *c.borrow_mut() = Some(controller));
    let state = app.global::<LaunchState>();
    state.set_findings(ModelRc::from(rows));
    state.on_launch(|| with_launch(|l| l.launch_clicked()));
    state.on_launch_anyway(|| with_launch(|l| l.launch_anyway()));
    state.on_cancel(|| with_launch(|l| l.with_state(|s| s.set_dialog_open(false))));
    state.on_review(|| {
        with_launch(|l| {
            if l.rows.row_count() > 0 {
                l.with_state(|s| s.set_dialog_open(true));
            }
        });
    });
    with_launch(|l| l.check(Purpose::Startup));
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .is_ok()
}

impl Launch {
    fn with_state(&self, f: impl FnOnce(&LaunchState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<LaunchState>());
        }
    }

    fn next_generation(&self) -> u64 {
        let g = self.generation.get() + 1;
        self.generation.set(g);
        g
    }

    fn busy(&self) -> bool {
        self.weak
            .upgrade()
            .is_none_or(|a| a.global::<LaunchState>().get_busy())
    }

    fn launch_clicked(&self) {
        if !self.busy() {
            self.check(Purpose::Launch);
        }
    }

    fn check(&self, purpose: Purpose) {
        let generation = self.next_generation();
        self.with_state(|s| {
            s.set_busy(true);
            s.set_busy_text("Checking game files".into());
            s.set_status_text("".into());
            s.set_status_error(false);
        });
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-health", move || {
            let result = (deps.locate)()
                .map(|paths| ((deps.check)(&paths), paths))
                .map_err(|e| game_error_text(&e));
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_launch(|l| l.checked(generation, purpose, result));
            });
        });
        if !started {
            self.finish(Err("Could not start a background thread.".into()));
        }
    }

    fn checked(
        &self,
        generation: u64,
        purpose: Purpose,
        result: Result<(Vec<Finding>, GamePaths), String>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        let (findings, paths) = match result {
            Ok(r) => r,
            Err(text) => {
                self.rows.set_vec(Vec::new());
                return self.finish(Err(text));
            }
        };
        self.rows.set_vec(rows(&findings, &paths));
        self.with_state(|s| {
            s.set_busy(false);
            s.set_problems_text(problems_text(findings.len()).into());
            s.set_problems_short(problems_short(findings.len()).into());
        });
        *self.paths.borrow_mut() = Some(paths.clone());
        // Debug builds only: open the review dialog for startup findings (for
        // screenshots); it never launches anything.
        #[cfg(debug_assertions)]
        if purpose == Purpose::Startup
            && !findings.is_empty()
            && std::env::var_os("SHIV_REVIEW").is_some_and(|v| !v.is_empty())
        {
            self.with_state(|s| s.set_dialog_open(true));
        }
        if purpose == Purpose::Launch {
            if findings.is_empty() {
                self.start_launch(paths);
            } else {
                self.with_state(|s| s.set_dialog_open(true));
            }
        }
    }

    fn launch_anyway(&self) {
        self.with_state(|s| s.set_dialog_open(false));
        if self.busy() {
            return;
        }
        let paths = self.paths.borrow().clone();
        if let Some(paths) = paths {
            self.start_launch(paths);
        }
    }

    fn start_launch(&self, paths: GamePaths) {
        let generation = self.next_generation();
        self.with_state(|s| {
            s.set_busy(true);
            s.set_busy_text("Starting Deadlock".into());
        });
        let launcher = self.deps.launch.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-launch", move || {
            let result = launcher(&paths).map_err(|e| format!("Could not launch Deadlock: {e}."));
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_launch(|l| {
                    if generation == l.generation.get() {
                        l.finish(result.map(|()| "Deadlock is starting through Steam.".into()));
                    }
                });
            });
        });
        if !started {
            self.finish(Err("Could not start a background thread.".into()));
        }
    }

    /// Clear `busy` and show the outcome below the Launch button.
    fn finish(&self, outcome: Result<String, String>) {
        let (text, error) = match outcome {
            Ok(t) => (t, false),
            Err(t) => (t, true),
        };
        self.with_state(|s| {
            s.set_busy(false);
            s.set_status_text(text.into());
            s.set_status_error(error);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rows_show_titles_and_files_relative_to_citadel() {
        let paths = GamePaths::from_root(PathBuf::from("/g/Deadlock"));
        let findings = vec![
            Finding {
                kind: FindingKind::ZeroByte,
                file: Some(paths.addons.join("pak01_dir.vpk")),
                detail: "The file is empty".into(),
            },
            Finding {
                kind: FindingKind::MissingSearchPath,
                file: Some(paths.gameinfo.clone()),
                detail: "d".into(),
            },
            Finding {
                kind: FindingKind::PartialDownload,
                file: None,
                detail: "d".into(),
            },
        ];
        let r = rows(&findings, &paths);
        assert_eq!(r[0].title.as_str(), "Empty VPK");
        assert_eq!(r[0].file.as_str(), "addons/pak01_dir.vpk");
        assert_eq!(r[0].detail.as_str(), "The file is empty");
        assert_eq!(r[1].file.as_str(), "gameinfo.gi");
        assert_eq!(r[2].file.as_str(), "");
        assert_eq!(problems_text(1), "1 problem found");
        assert_eq!(problems_text(3), "3 problems found");
        assert_eq!(problems_short(1), "1 problem");
        assert_eq!(problems_short(2), "2 problems");
    }
}
