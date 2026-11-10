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

use shiv_core::game::GamePaths;
use shiv_core::health::{self, Finding, FindingKind};
use shiv_core::launch::{self, LaunchError};
use shiv_repair::OverlayState;
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

#[derive(Clone, PartialEq, Eq)]
enum Purpose {
    /// App start, screen changes and finished installs: updates the sidebar
    /// quietly (no busy state) and never opens the dialog or launches.
    Background,
    /// Launch click: launch if clean, otherwise show the findings.
    Launch,
    /// Launch anyway on findings the user saw earlier (via Review): launch
    /// only if the disk still shows exactly those, otherwise show the new ones.
    Confirm(Vec<Finding>, bool),
}

struct Launch {
    weak: Weak<App>,
    deps: Deps,
    rows: Rc<VecModel<FindingRow>>,
    /// Result of the last check: where it ran, what it found, and whether
    /// the repair package is out of date.
    last: RefCell<Option<(GamePaths, Vec<Finding>, bool)>>,
    /// The open dialog shows the result of the check that the Launch click
    /// itself ran, so Launch anyway needs no re-check.
    fresh_dialog: Cell<bool>,
    /// The status line holds a check error (cleared by the next good check),
    /// not a launch result.
    status_from_check: Cell<bool>,
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
        last: RefCell::new(None),
        fresh_dialog: Cell::new(false),
        status_from_check: Cell::new(false),
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
            if l.rows.row_count() > 0
                || l.weak
                    .upgrade()
                    .is_some_and(|a| a.global::<LaunchState>().get_repair_stale())
            {
                l.fresh_dialog.set(false);
                l.with_state(|s| s.set_dialog_open(true));
            }
        });
    });
    state.on_refresh(refresh);
    state.on_open_repair(|| {
        with_launch(|l| {
            l.with_state(|s| s.set_dialog_open(false));
            if let Some(app) = l.weak.upgrade() {
                crate::open_repair(&app);
            }
        });
    });
    with_launch(|l| l.check(Purpose::Background));
}

/// Re-check quietly so the sidebar count follows the disk: called on screen
/// changes and when an install, toggle or uninstall finishes. Skipped while a
/// Launch is in progress or the dialog is open (its list stays put).
pub(crate) fn refresh() {
    with_launch(|l| {
        let dialog = l
            .weak
            .upgrade()
            .is_some_and(|a| a.global::<LaunchState>().get_dialog_open());
        if !l.busy() && !dialog {
            l.check(Purpose::Background);
        }
    });
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

    /// A Launch (check or launch) is in progress.
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

    /// Supersedes any check in flight (a Launch click overrides a background check).
    fn check(&self, purpose: Purpose) {
        let generation = self.next_generation();
        if purpose != Purpose::Background {
            self.with_state(|s| {
                s.set_busy(true);
                s.set_busy_text("Checking game files".into());
                s.set_status_text("".into());
                s.set_status_error(false);
            });
        }
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-health",
            move || {
                (deps.locate)()
                    .map(|paths| {
                        // A repair package built for other mods or another
                        // game build keeps loading until it is rebuilt or removed.
                        let stale = shiv_repair::overlay_state(&paths) == OverlayState::Stale;
                        ((deps.check)(&paths), stale, paths)
                    })
                    .map_err(|e| game_error_text(&e))
            },
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_launch(|l| l.checked(generation, purpose, result));
                });
            },
        );
        if !started {
            self.status_from_check.set(true);
            self.finish(Err("Could not start a background thread.".into()));
        }
    }

    fn checked(
        &self,
        generation: u64,
        purpose: Purpose,
        result: Result<(Vec<Finding>, bool, GamePaths), String>,
    ) {
        if generation != self.generation.get() {
            return;
        }
        let (findings, stale, paths) = match result {
            Ok(r) => r,
            Err(text) => {
                self.rows.set_vec(Vec::new());
                *self.last.borrow_mut() = None;
                self.with_state(|s| s.set_repair_stale(false));
                self.status_from_check.set(true);
                return self.finish(Err(text));
            }
        };
        self.rows.set_vec(rows(&findings, &paths));
        let problems = findings.len() + usize::from(stale);
        self.with_state(|s| {
            s.set_busy(false);
            s.set_repair_stale(stale);
            s.set_problems_text(problems_text(problems).into());
            s.set_problems_short(problems_short(problems).into());
            if self.status_from_check.replace(false) {
                s.set_status_text("".into());
                s.set_status_error(false);
            }
        });
        *self.last.borrow_mut() = Some((paths.clone(), findings.clone(), stale));
        let launch_now = match &purpose {
            Purpose::Background => {
                // Debug builds only: open the review dialog for startup findings
                // (for screenshots); it never launches anything.
                #[cfg(debug_assertions)]
                if problems > 0 && std::env::var_os("SHIV_REVIEW").is_some_and(|v| !v.is_empty()) {
                    self.fresh_dialog.set(false);
                    self.with_state(|s| s.set_dialog_open(true));
                }
                return;
            }
            Purpose::Launch => problems == 0,
            Purpose::Confirm(seen, seen_stale) => {
                problems == 0 || (*seen == findings && *seen_stale == stale)
            }
        };
        if launch_now {
            self.start_launch(paths);
        } else {
            self.fresh_dialog.set(true);
            self.with_state(|s| s.set_dialog_open(true));
        }
    }

    fn launch_anyway(&self) {
        self.with_state(|s| s.set_dialog_open(false));
        if self.busy() {
            return;
        }
        let last = self.last.borrow().clone();
        let Some((paths, findings, stale)) = last else {
            return;
        };
        if self.fresh_dialog.get() {
            self.start_launch(paths);
        } else {
            // Opened via Review: those findings may be stale.
            self.check(Purpose::Confirm(findings, stale));
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
        let started = crate::spawn_worker(
            "shiv-launch",
            move || launcher(&paths).map_err(|e| format!("Could not launch Deadlock: {e}.")),
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_launch(|l| {
                        if generation == l.generation.get() {
                            l.finish(result.map(|()| "Deadlock is starting through Steam.".into()));
                        }
                    });
                });
            },
        );
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
