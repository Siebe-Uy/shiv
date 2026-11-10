//! Installed screen controller: lists what is in the addons folder, toggles
//! mods on and off and uninstalls them.
//!
//! Threading matches `browse`: this module runs on the Slint event loop; every
//! disk touch (listing, renaming, deleting, thumbnail fetch) runs on a worker
//! thread that posts its result back with `upgrade_in_event_loop`. Nothing
//! polls: the list is re-read when the screen is shown, on Refresh/Retry and
//! after every action, and that same read refreshes the game-running state.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use shiv_core::addons::{self, AddonsError};
use shiv_core::game::{self, GameError, GamePaths};
use shiv_core::install::{self, InstallError};
use shiv_core::sidecar::{self, InstalledMod};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use crate::browse::{game_error_text, install_error_text, to_slint};
use crate::thumbs;
use crate::{App, InstalledRow, InstalledState, LoadState};

pub(crate) type Locate = Arc<dyn Fn() -> Result<GamePaths, GameError> + Send + Sync>;
pub(crate) type Running = Arc<dyn Fn() -> bool + Send + Sync>;

/// Where the game is and whether it runs; injectable so tests never touch the
/// real install or depend on a running game.
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

const GAME_RUNNING_TEXT: &str =
    "Deadlock is running. Close it to enable, disable or uninstall mods.";
const SUFFIX: &str = ".disabled";

thread_local! {
    static INSTALLED: RefCell<Option<Rc<Installed>>> = const { RefCell::new(None) };
}

fn with_installed(f: impl FnOnce(&Rc<Installed>)) {
    if let Some(i) = INSTALLED.with(|c| c.borrow().clone()) {
        f(&i);
    }
}

// ---------------------------------------------------------------------------
// Row view-models (pure, unit-tested)

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The sidecar mod id, or the enabled-form file name for a hand-added VPK.
    pub id: String,
    pub name: String,
    pub detail: String,
    /// Installed by Shiv (has a sidecar).
    pub managed: bool,
    /// Every VPK of the mod is enabled.
    pub enabled: bool,
    pub thumb_url: Option<String>,
}

fn enabled_file_name(vpk: &addons::InstalledVpk) -> String {
    let name = vpk
        .file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(SUFFIX)
        .map_or(name.clone(), str::to_owned)
}

fn key(m: &InstalledMod) -> String {
    match (&m.sidecar, m.vpks.first()) {
        (Some(s), _) => s.mod_id.clone(),
        (None, Some(v)) => enabled_file_name(v),
        (None, None) => String::new(),
    }
}

fn entry(m: &InstalledMod) -> Entry {
    let enabled = !m.vpks.is_empty() && m.vpks.iter().all(|v| v.enabled);
    let any_enabled = m.vpks.iter().any(|v| v.enabled);
    let Some(s) = &m.sidecar else {
        return Entry {
            id: key(m),
            name: key(m),
            detail: String::new(),
            managed: false,
            enabled,
            thumb_url: None,
        };
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = s.version.as_deref().filter(|v| !v.is_empty()) {
        parts.push(format!("Version {v}"));
    }
    let mut paks: Vec<u32> = m.vpks.iter().filter_map(|v| v.pak).collect();
    paks.sort_unstable();
    paks.dedup();
    match paks.as_slice() {
        [] => {}
        [one] => parts.push(format!("pak{one:02}")),
        many => {
            let list: Vec<String> = many.iter().map(|p| format!("{p:02}")).collect();
            parts.push(format!("paks {}", list.join(", ")));
        }
    }
    if any_enabled && !enabled {
        parts.push("Partly enabled".into());
    }
    Entry {
        id: s.mod_id.clone(),
        name: if s.name.is_empty() {
            s.mod_id.clone()
        } else {
            s.name.clone()
        },
        detail: parts.join(" \u{b7} "),
        managed: true,
        enabled,
        thumb_url: s.thumbnail_url.clone().filter(|u| !u.is_empty()),
    }
}

pub(crate) fn entries(mods: &[InstalledMod]) -> Vec<Entry> {
    mods.iter().map(entry).collect()
}

fn summary(n: usize) -> String {
    if n == 1 {
        "1 mod".into()
    } else {
        format!("{n} mods")
    }
}

// ---------------------------------------------------------------------------
// Worker side

struct Loaded {
    mods: Vec<InstalledMod>,
    running: bool,
}

/// `addons::list` runs first: `sidecar::group` swallows an unreadable folder
/// and would show an empty list instead of an error.
fn load(deps: &Deps) -> Result<Loaded, String> {
    let paths = (deps.locate)().map_err(|e| game_error_text(&e))?;
    addons::list(&paths.addons).map_err(|e| format!("The addons folder could not be read. {e}"))?;
    Ok(Loaded {
        mods: sidecar::group(&paths.addons),
        running: (deps.running)(),
    })
}

#[derive(Clone, Copy)]
enum Action {
    Toggle { enable: bool },
    Uninstall,
}

impl Action {
    fn verb(self) -> &'static str {
        match self {
            Action::Toggle { enable: true } => "enable",
            Action::Toggle { enable: false } => "disable",
            Action::Uninstall => "uninstall",
        }
    }
}

fn addons_error_text(e: &AddonsError) -> String {
    match e {
        AddonsError::Full { .. } => install_error_text(&InstallError::AddonsFull),
        other => other.to_string(),
    }
}

fn action_error_text(name: &str, action: Action, detail: &str) -> String {
    format!("Could not {} {name}. {detail}", action.verb())
}

fn run_action(deps: &Deps, m: &InstalledMod, name: &str, action: Action) -> Result<(), String> {
    let fail = |detail: &str| action_error_text(name, action, detail);
    if (deps.running)() {
        return Err(fail(GAME_RUNNING_TEXT));
    }
    match action {
        Action::Toggle { enable } => {
            // `set_enabled` moves a VPK and its chunks together and keeps the pak number.
            for vpk in &m.vpks {
                addons::set_enabled(vpk, enable).map_err(|e| fail(&addons_error_text(&e)))?;
            }
            Ok(())
        }
        Action::Uninstall => {
            let running = deps.running.clone();
            install::uninstall_with(m, move || running()).map_err(|e| match e {
                InstallError::GameRunning => fail(GAME_RUNNING_TEXT),
                other => fail(&install_error_text(&other)),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Controller

struct Installed {
    weak: Weak<App>,
    deps: Deps,
    cache: Arc<thumbs::Cache>,
    rows: Rc<VecModel<InstalledRow>>,
    mods: RefCell<Vec<InstalledMod>>,
    entries: RefCell<Vec<Entry>>,
    generation: Cell<u64>,
}

/// Wire the Installed screen to `app` using the real game location.
pub fn attach(app: &App, cache: Arc<thumbs::Cache>) {
    attach_with(app, cache, Deps::system());
}

pub(crate) fn attach_with(app: &App, cache: Arc<thumbs::Cache>, deps: Deps) {
    let rows = Rc::new(VecModel::default());
    let controller = Rc::new(Installed {
        weak: app.as_weak(),
        deps,
        cache,
        rows: rows.clone(),
        mods: RefCell::new(Vec::new()),
        entries: RefCell::new(Vec::new()),
        generation: Cell::new(0),
    });
    INSTALLED.with(|c| *c.borrow_mut() = Some(controller));
    let state = app.global::<InstalledState>();
    state.set_rows(ModelRc::from(rows));
    state.on_refresh(|| with_installed(|i| i.refresh(true)));
    state.on_toggle(|id| with_installed(|i| i.toggle(id.as_str())));
    state.on_uninstall(|id| with_installed(|i| i.act(id.as_str(), Action::Uninstall)));
    state.on_dismiss_banner(|| {
        with_installed(|i| i.with_state(|s| s.set_banner_text("".into())));
    });
}

impl Installed {
    fn with_state(&self, f: impl FnOnce(&InstalledState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<InstalledState>());
        }
    }

    fn build_row(&self, e: &Entry) -> InstalledRow {
        let mut row = InstalledRow {
            id: e.id.as_str().into(),
            name: e.name.as_str().into(),
            detail: e.detail.as_str().into(),
            tag: if e.managed {
                ""
            } else {
                "Not installed by Shiv"
            }
            .into(),
            enabled: e.enabled,
            managed: e.managed,
            ..InstalledRow::default()
        };
        if let Some(img) = e.thumb_url.as_deref().and_then(|u| self.cache.get(u)) {
            row.thumb = to_slint(&img);
            row.has_thumb = true;
        }
        row
    }

    /// Re-read the addons folder. `user` refreshes (screen shown, Refresh,
    /// Retry) also clear the banner; the refresh after an action keeps it.
    fn refresh(&self, user: bool) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.with_state(|s| {
            if user {
                s.set_banner_text("".into());
            }
            // Busy until the result for this generation arrives: actions are
            // dropped while the entries may be stale.
            s.set_busy(true);
            // Keep a loaded list on screen while it is re-read.
            if s.get_load_state() != LoadState::Ready {
                s.set_load_state(LoadState::Loading);
            }
        });
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-installed",
            move || load(&deps),
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_installed(|i| i.loaded(generation, result));
                });
            },
        );
        if !started {
            self.fail("Could not start a background thread.".into());
        }
    }

    fn fail(&self, text: String) {
        self.with_state(|s| {
            s.set_busy(false);
            s.set_error_text(text.into());
            s.set_load_state(LoadState::Failed);
        });
    }

    fn loaded(&self, generation: u64, result: Result<Loaded, String>) {
        if generation != self.generation.get() {
            return;
        }
        let loaded = match result {
            Ok(l) => l,
            Err(text) => return self.fail(text),
        };
        let list = entries(&loaded.mods);
        self.rows
            .set_vec(list.iter().map(|e| self.build_row(e)).collect::<Vec<_>>());
        self.with_state(|s| {
            s.set_busy(false);
            s.set_game_running(loaded.running);
            s.set_summary(summary(list.len()).into());
            s.set_load_state(LoadState::Ready);
            let pending = s.get_confirm_id();
            if !list.iter().any(|e| e.id.as_str() == pending.as_str()) {
                s.set_confirm_id(SharedString::new());
            }
        });
        self.fetch_thumbs(&list);
        *self.mods.borrow_mut() = loaded.mods;
        *self.entries.borrow_mut() = list;
    }

    /// Fetch thumbnails the cache does not hold yet, one worker for the batch.
    fn fetch_thumbs(&self, list: &[Entry]) {
        let mut seen = HashSet::new();
        let urls: Vec<String> = list
            .iter()
            .filter_map(|e| e.thumb_url.clone())
            .filter(|u| self.cache.get(u).is_none() && !self.cache.has_failed(u))
            .filter(|u| seen.insert(u.clone()))
            .collect();
        if urls.is_empty() {
            return;
        }
        let cache = self.cache.clone();
        let weak = self.weak.clone();
        crate::spawn_worker(
            "shiv-installed-thumbs",
            move || {
                for url in urls {
                    if cache.get_or_fetch(&url).is_none() {
                        continue;
                    }
                    let posted = weak.upgrade_in_event_loop(move |_| {
                        with_installed(|i| i.thumb_ready(&url));
                    });
                    if posted.is_err() {
                        return;
                    }
                }
            },
            // Rows keep their placeholder; nothing waits on this worker.
            |_| {},
        );
    }

    fn thumb_ready(&self, url: &str) {
        let entries = self.entries.borrow();
        for (i, e) in entries.iter().enumerate() {
            if e.thumb_url.as_deref() == Some(url) && i < self.rows.row_count() {
                self.rows.set_row_data(i, self.build_row(e));
            }
        }
    }

    fn toggle(&self, id: &str) {
        let enable = match self.entries.borrow().iter().find(|e| e.id == id) {
            Some(e) => !e.enabled,
            None => return,
        };
        self.act(id, Action::Toggle { enable });
    }

    fn act(&self, id: &str, action: Action) {
        let busy = self
            .weak
            .upgrade()
            .is_none_or(|a| a.global::<InstalledState>().get_busy());
        if busy {
            return;
        }
        let found = {
            let entries = self.entries.borrow();
            let mods = self.mods.borrow();
            entries
                .iter()
                .position(|e| e.id == id)
                .and_then(|i| Some((mods.get(i)?.clone(), entries[i].name.clone())))
        };
        let Some((m, name)) = found else {
            return;
        };
        self.with_state(|s| {
            s.set_busy(true);
            // The confirmation is consumed only once the action is accepted.
            s.set_confirm_id(SharedString::new());
        });
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = crate::spawn_worker(
            "shiv-installed-action",
            move || run_action(&deps, &m, &name, action),
            move |result| {
                let result = result.and_then(|r| r);
                let _ = weak.upgrade_in_event_loop(move |_| {
                    with_installed(|i| i.acted(result));
                });
            },
        );
        if !started {
            self.acted(Err("Could not start a background thread.".into()));
        }
    }

    /// Success or failure, re-read the folder: that also refreshes the
    /// game-running state (a game-running refusal means it just started).
    fn acted(&self, result: Result<(), String>) {
        // The disk changed: keep the Launch sidebar count current.
        crate::launch::refresh();
        // `busy` stays set; the refresh below clears it when the list is current.
        self.with_state(|s| {
            if let Err(text) = &result {
                s.set_banner_text(text.as_str().into());
            }
        });
        self.refresh(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shiv_core::addons::InstalledVpk;
    use shiv_core::sidecar::Sidecar;
    use std::path::PathBuf;

    fn vpk(file: &str, pak: Option<u32>, enabled: bool) -> InstalledVpk {
        InstalledVpk {
            file: PathBuf::from("/a").join(file),
            pak,
            enabled,
            chunks: Vec::new(),
        }
    }

    fn sidecar(id: &str, name: &str) -> Sidecar {
        Sidecar {
            mod_id: id.into(),
            name: name.into(),
            version: Some("1.2".into()),
            thumbnail_url: Some("https://x.test/t.png".into()),
            vpks: Vec::new(),
        }
    }

    #[test]
    fn managed_mod_uses_sidecar_name_and_thumbnail() {
        let m = InstalledMod {
            sidecar: Some(sidecar("m1", "Neon Hud")),
            vpks: vec![vpk("pak03_dir.vpk", Some(3), true)],
        };
        let e = &entries(&[m])[0];
        assert_eq!(e.id, "m1");
        assert_eq!(e.name, "Neon Hud");
        assert!(e.managed && e.enabled);
        assert_eq!(e.detail, "Version 1.2 \u{b7} pak03");
        assert_eq!(e.thumb_url.as_deref(), Some("https://x.test/t.png"));
    }

    #[test]
    fn hand_added_vpk_shows_its_file_name() {
        let m = InstalledMod {
            sidecar: None,
            vpks: vec![vpk("custom.vpk.disabled", None, false)],
        };
        let e = &entries(&[m])[0];
        assert_eq!(e.id, "custom.vpk");
        assert_eq!(e.name, "custom.vpk");
        assert!(!e.managed && !e.enabled);
        assert_eq!(e.detail, "");
        assert_eq!(e.thumb_url, None);
    }

    #[test]
    fn mixed_state_reads_as_disabled_and_is_flagged() {
        let m = InstalledMod {
            sidecar: Some(sidecar("m1", "")),
            vpks: vec![
                vpk("pak01_dir.vpk", Some(1), true),
                vpk("pak02_dir.vpk.disabled", Some(2), false),
            ],
        };
        let e = &entries(&[m])[0];
        assert_eq!(e.name, "m1", "empty sidecar name falls back to the id");
        assert!(!e.enabled);
        assert!(e.detail.contains("paks 01, 02"), "{}", e.detail);
        assert!(e.detail.contains("Partly enabled"), "{}", e.detail);
    }
}
