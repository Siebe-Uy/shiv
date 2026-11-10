//! Profiles screen controller and the sidebar's quick switch.
//!
//! Same threading as `installed`: this runs on the event loop, every disk
//! touch runs on a worker that posts its result back. Switching and deleting
//! are refused while Deadlock runs (the worker checks the process itself).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use shiv_core::game::{self, GamePaths};
use shiv_core::profiles::{self, ProfileError};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::browse::game_error_text;
use crate::installed::{Locate, Running};
use crate::{App, LoadState, ProfileRow, ProfilesState};

const GAME_RUNNING_TEXT: &str = "Deadlock is running. Close it to change profiles.";

#[derive(Clone)]
pub(crate) struct Deps {
    pub locate: Locate,
    pub running: Running,
}

impl Deps {
    fn system() -> Self {
        Deps {
            locate: Arc::new(crate::locate_game),
            running: Arc::new(game::is_running),
        }
    }
}

/// What a worker read from disk.
struct Snapshot {
    supported: bool,
    set_up: bool,
    active: String,
    rows: Vec<(String, String, String)>, // id, name, detail
    running: bool,
}

enum Action {
    Setup,
    Switch(String),
    Create(String),
    Duplicate(String),
    Rename(String, String),
    Delete(String),
}

struct Profiles {
    weak: Weak<App>,
    deps: Deps,
    rows: Rc<VecModel<ProfileRow>>,
    names: Rc<VecModel<SharedString>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
}

thread_local! {
    static PROFILES: RefCell<Option<Rc<Profiles>>> = const { RefCell::new(None) };
}

fn with_profiles(f: impl FnOnce(&Rc<Profiles>)) {
    if let Some(p) = PROFILES.with(|c| c.borrow().clone()) {
        f(&p);
    }
}

pub fn attach(app: &App) {
    attach_with(app, Deps::system());
}

pub(crate) fn attach_with(app: &App, deps: Deps) {
    let controller = Rc::new(Profiles {
        weak: app.as_weak(),
        deps,
        rows: Rc::new(VecModel::default()),
        names: Rc::new(VecModel::default()),
        generation: Cell::new(0),
        busy: Cell::new(false),
    });
    let state = app.global::<ProfilesState>();
    state.set_rows(ModelRc::from(controller.rows.clone()));
    state.set_names(ModelRc::from(controller.names.clone()));
    PROFILES.with(|c| *c.borrow_mut() = Some(controller));
    state.on_shown(|| with_profiles(|p| p.refresh()));
    state.on_setup(|| with_profiles(|p| p.act(Action::Setup)));
    state.on_switch_to(|id| with_profiles(|p| p.act(Action::Switch(id.to_string()))));
    state.on_switch_index(|i| {
        with_profiles(|p| {
            let id = usize::try_from(i)
                .ok()
                .and_then(|i| p.rows.row_data(i))
                .map(|r| r.id.to_string());
            match id {
                Some(id) => p.act(Action::Switch(id)),
                None => p.refresh(),
            }
        });
    });
    state.on_create(|name| with_profiles(|p| p.act(Action::Create(name.to_string()))));
    state.on_duplicate(|id| with_profiles(|p| p.act(Action::Duplicate(id.to_string()))));
    state.on_rename(|id, name| {
        with_profiles(|p| p.act(Action::Rename(id.to_string(), name.to_string())));
    });
    state.on_remove(|id| with_profiles(|p| p.act(Action::Delete(id.to_string()))));
    state.on_dismiss_banner(|| with_profiles(|p| p.set_banner("")));
    with_profiles(|p| p.refresh());
}

fn read(deps: &Deps, paths: &GamePaths) -> Snapshot {
    let running = (deps.running)();
    if !profiles::supported() {
        return Snapshot {
            supported: false,
            set_up: false,
            active: String::new(),
            rows: Vec::new(),
            running,
        };
    }
    if !profiles::is_set_up(paths) {
        return Snapshot {
            supported: true,
            set_up: false,
            active: String::new(),
            rows: Vec::new(),
            running,
        };
    }
    // Set up already, so loading only reads (and tidies the index).
    match profiles::load(paths) {
        Ok(p) => Snapshot {
            supported: true,
            set_up: true,
            rows: p
                .profiles
                .iter()
                .map(|x| {
                    let n = profiles::mod_count(paths, &x.id);
                    let detail = format!("{n} {}", if n == 1 { "mod" } else { "mods" });
                    (x.id.clone(), x.name.clone(), detail)
                })
                .collect(),
            active: p.active,
            running,
        },
        Err(_) => Snapshot {
            supported: true,
            set_up: false,
            active: String::new(),
            rows: Vec::new(),
            running,
        },
    }
}

fn error_text(e: &ProfileError) -> String {
    e.to_string()
}

impl Profiles {
    fn with_state(&self, f: impl FnOnce(&ProfilesState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<ProfilesState>());
        }
    }

    fn set_banner(&self, text: &str) {
        self.with_state(|s| s.set_banner_text(text.into()));
    }

    fn refresh(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-profiles-read",
            move || {
                (deps.locate)()
                    .map(|paths| read(&deps, &paths))
                    .map_err(|e| game_error_text(&e))
            },
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_profiles(|p| p.loaded(generation, result));
                });
            },
        );
        if !started {
            self.loaded(
                generation,
                Err("Could not start a background thread.".into()),
            );
        }
    }

    fn loaded(&self, generation: u64, result: Result<Snapshot, String>) {
        if generation != self.generation.get() {
            return;
        }
        match result {
            Err(text) => self.with_state(|s| {
                s.set_error_text(text.into());
                s.set_load_state(LoadState::Failed);
            }),
            Ok(snap) => {
                let active_index = snap.rows.iter().position(|r| r.0 == snap.active);
                self.names.set_vec(
                    snap.rows
                        .iter()
                        .map(|r| SharedString::from(r.1.as_str()))
                        .collect::<Vec<_>>(),
                );
                self.rows.set_vec(
                    snap.rows
                        .iter()
                        .map(|r| ProfileRow {
                            id: r.0.as_str().into(),
                            name: r.1.as_str().into(),
                            detail: r.2.as_str().into(),
                            active: r.0 == snap.active,
                        })
                        .collect::<Vec<_>>(),
                );
                self.with_state(|s| {
                    s.set_supported(snap.supported);
                    s.set_set_up(snap.set_up);
                    s.set_game_running(snap.running);
                    s.set_active_index(active_index.map_or(0, |i| i as i32));
                    s.set_busy(false);
                    s.set_load_state(LoadState::Ready);
                });
            }
        }
    }

    /// Run a change on a worker, then re-read. Refused while the game runs.
    fn act(&self, action: Action) {
        if self.busy.get() {
            return;
        }
        self.busy.set(true);
        self.with_state(|s| {
            s.set_busy(true);
            s.set_banner_text("".into());
        });
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-profiles-act",
            move || -> Result<(), String> {
                let paths = (deps.locate)().map_err(|e| game_error_text(&e))?;
                if (deps.running)() {
                    return Err(GAME_RUNNING_TEXT.into());
                }
                let result = match action {
                    Action::Setup => profiles::load(&paths).map(|_| ()),
                    Action::Switch(id) => profiles::switch(&paths, &id),
                    Action::Create(name) => profiles::create(&paths, &name).map(|_| ()),
                    Action::Duplicate(id) => {
                        let name = profiles::load(&paths)
                            .ok()
                            .and_then(|p| {
                                p.profiles
                                    .iter()
                                    .find(|x| x.id == id)
                                    .map(|x| x.name.clone())
                            })
                            .unwrap_or_default();
                        copy_name(&paths, &name)
                            .and_then(|n| profiles::duplicate(&paths, &id, &n))
                            .map(|_| ())
                    }
                    Action::Rename(id, name) => profiles::rename(&paths, &id, &name),
                    Action::Delete(id) => profiles::delete(&paths, &id),
                };
                result.map_err(|e| error_text(&e))
            },
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_profiles(|p| p.acted(result));
                });
            },
        );
        if !started {
            self.acted(Err("Could not start a background thread.".into()));
        }
    }

    fn acted(&self, result: Result<(), String>) {
        self.busy.set(false);
        self.with_state(|s| s.set_busy(false));
        if let Err(text) = result {
            self.set_banner(&text);
        } else {
            // The mod list changed under every screen that shows it.
            crate::launch::refresh();
            self.with_state(|s| s.set_confirm_id("".into()));
        }
        self.refresh();
    }
}

/// "Name copy", "Name copy 2", ... the first one no profile uses.
fn copy_name(paths: &GamePaths, name: &str) -> Result<String, ProfileError> {
    let existing = profiles::load(paths)?;
    let base = if name.is_empty() {
        "Profile".to_owned()
    } else {
        name.to_owned()
    };
    let mut candidate = format!("{base} copy");
    let mut n = 2;
    while existing
        .profiles
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case(&candidate))
    {
        candidate = format!("{base} copy {n}");
        n += 1;
    }
    Ok(candidate.chars().take(40).collect())
}
