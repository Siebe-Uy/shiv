//! Repair screen controller: checks installed mods with `shiv_repair::analyze`,
//! previews what a repair changes, applies it and undoes the last one.
//!
//! Threading matches `installed`: `analyze` is slow on a real install (it
//! opens every game VPK and runs the engine per mod), so it and every write run
//! on worker threads that post back with `upgrade_in_event_loop`. The check
//! runs when the screen is shown, on Re-scan and after every apply or undo; an
//! old list stays on screen while it runs. Apply and undo are refused while a
//! check or another action runs, and re-check the game process on the worker.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use shiv_core::addons;
use shiv_core::game::GamePaths;
use shiv_repair::{FixReport, ModHealth, ModId, OverlayState, RepairError, RepairStatus};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::browse::game_error_text;
use crate::installed::{Locate, Running};
use crate::{App, LoadState, RepairRow, RepairState, RepairTone};

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
            running: Arc::new(shiv_core::game::is_running),
        }
    }
}

/// Change lines shown per mod before the rest fold into "and N more".
const MAX_CHANGES: usize = 6;
const GAME_RUNNING_TEXT: &str = "Deadlock is running. Close it, then try again.";

thread_local! {
    static REPAIR: RefCell<Option<Rc<Repair>>> = const { RefCell::new(None) };
}

fn with_repair(f: impl FnOnce(&Rc<Repair>)) {
    if let Some(r) = REPAIR.with(|c| c.borrow().clone()) {
        f(&r);
    }
}

// ---------------------------------------------------------------------------
// View-models (pure, unit-tested)

pub(crate) fn tone(status: RepairStatus) -> RepairTone {
    match status {
        RepairStatus::Works => RepairTone::Works,
        RepairStatus::Fixable => RepairTone::Fixable,
        RepairStatus::NeedsUpdate => RepairTone::Update,
        RepairStatus::Fixed => RepairTone::Fixed,
        RepairStatus::NotChecked => RepairTone::Unchecked,
    }
}

pub(crate) fn status_label(status: RepairStatus) -> &'static str {
    match status {
        RepairStatus::Works => "Works",
        RepairStatus::Fixable => "Fixable",
        RepairStatus::NeedsUpdate => "Needs update",
        RepairStatus::Fixed => "Repaired",
        RepairStatus::NotChecked => "Not checked",
    }
}

pub(crate) fn capped(lines: &[String]) -> Vec<String> {
    if lines.len() <= MAX_CHANGES {
        return lines.to_vec();
    }
    let mut out = lines[..MAX_CHANGES - 1].to_vec();
    out.push(format!("and {} more", lines.len() - (MAX_CHANGES - 1)));
    out
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn strings(lines: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        lines
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

pub(crate) fn row(m: &ModHealth) -> RepairRow {
    let files = if m.files.len() == 1 && m.files[0] == m.name {
        String::new()
    } else {
        m.files.join(", ")
    };
    let mut remaining = m.remaining.clone();
    if let Some(e) = &m.error {
        remaining.push(e.clone());
    }
    if !m.enabled && m.status == RepairStatus::Fixable {
        remaining.push("Disabled: enable it to include it in a repair.".into());
    }
    RepairRow {
        id: m.mod_id.as_str().into(),
        name: m.name.as_str().into(),
        files: files.into(),
        tone: tone(m.status),
        status: status_label(m.status).into(),
        summary: m.summary.as_str().into(),
        changes: strings(capped(&m.changes)),
        remaining: strings(remaining),
        enabled: m.enabled,
    }
}

/// What Apply would repair: every enabled mod with repairs available plus
/// those already repaired (an apply repairs exactly the listed mods, so
/// leaving a repaired mod out would drop its repairs).
pub(crate) fn targets(mods: &[ModHealth]) -> Vec<ModId> {
    mods.iter()
        .filter(|m| m.enabled && matches!(m.status, RepairStatus::Fixable | RepairStatus::Fixed))
        .map(|m| m.mod_id.clone())
        .collect()
}

fn fixable(mods: &[ModHealth]) -> usize {
    mods.iter()
        .filter(|m| m.enabled && m.status == RepairStatus::Fixable)
        .count()
}

pub(crate) fn summary(mods: &[ModHealth]) -> String {
    let fixable = fixable(mods);
    let update = mods
        .iter()
        .filter(|m| m.status == RepairStatus::NeedsUpdate)
        .count();
    let mut parts = vec![plural(mods.len(), "mod", "mods")];
    if fixable > 0 {
        parts.push(format!("{fixable} fixable"));
    }
    if update > 0 {
        parts.push(format!("{update} need an update"));
    }
    if parts.len() == 1 && !mods.is_empty() {
        parts.push("nothing to repair".into());
    }
    parts.join(", ")
}

/// Apply's label, and whether there is anything to apply.
pub(crate) fn apply_label(fixable: usize, stale: bool) -> (String, bool) {
    match (fixable, stale) {
        (0, true) => ("Remove old repairs".into(), true),
        (0, false) => ("Repair mods".into(), false),
        (_, true) => ("Re-apply repairs".into(), true),
        (n, false) => (format!("Repair {}", plural(n, "mod", "mods")), true),
    }
}

fn confirm_title(fixable: usize, targets: usize) -> String {
    if targets == 0 {
        "Remove the outdated repair package?".into()
    } else if fixable == 0 {
        format!("Re-apply repairs for {}?", plural(targets, "mod", "mods"))
    } else {
        format!("Repair {}?", plural(targets, "mod", "mods"))
    }
}

pub(crate) fn error_text(e: &RepairError) -> String {
    match e {
        RepairError::GameRunning => GAME_RUNNING_TEXT.into(),
        RepairError::BackupRestoreFailed(detail) => format!("{detail}."),
        other => other.to_string(),
    }
}

/// Outcome panel for a finished apply: title and detail lines.
pub(crate) fn apply_result(report: &FixReport, names: &[(ModId, String)]) -> (String, Vec<String>) {
    let name = |id: &ModId| {
        names
            .iter()
            .find(|(i, _)| i == id)
            .map_or_else(|| id.clone(), |(_, n)| n.clone())
    };
    let title = if !report.fixed.is_empty() {
        format!("Repaired {}", plural(report.fixed.len(), "mod", "mods"))
    } else if report.backup.is_some() {
        "Removed the repair package".into()
    } else {
        "Nothing was changed".into()
    };
    let mut lines: Vec<String> = report
        .skipped
        .iter()
        .map(|(id, reason)| format!("{}: {reason}", name(id)))
        .collect();
    lines.extend(report.notes.iter().cloned());
    if report.gameinfo_changed {
        lines.push("gameinfo.gi now loads the repair package before your mods.".into());
    }
    if let Some(b) = &report.backup {
        lines.push(format!("Backup of the replaced files: {}", b.display()));
    }
    (title, lines)
}

fn names(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|p| {
            p.file_name()
                .map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn undo_result(report: &shiv_repair::RestoreReport) -> (String, Vec<String>) {
    let mut lines = Vec::new();
    if !report.restored.is_empty() {
        lines.push(format!("Put back: {}", names(&report.restored)));
    }
    if !report.removed.is_empty() {
        lines.push(format!("Removed: {}", names(&report.removed)));
    }
    lines.push(format!("Backup used: {}", report.backup.display()));
    ("Last repair undone".into(), lines)
}

// ---------------------------------------------------------------------------
// Worker side

struct Scan {
    mods: Vec<ModHealth>,
    stale: bool,
    running: bool,
    can_undo: bool,
}

/// `addons::list` runs first: `analyze` yields no rows for an unreadable
/// folder, which would read as "no mods" instead of an error.
fn scan(deps: &Deps) -> Result<Scan, String> {
    let paths = (deps.locate)().map_err(|e| game_error_text(&e))?;
    addons::list(&paths.addons).map_err(|e| format!("The addons folder could not be read. {e}"))?;
    let build = shiv_repair::game_build(&paths).unwrap_or(0);
    Ok(Scan {
        mods: shiv_repair::analyze(&paths, build),
        stale: shiv_repair::overlay_state(&paths) == OverlayState::Stale,
        running: (deps.running)(),
        can_undo: shiv_repair::can_undo(&paths),
    })
}

enum Action {
    Apply(Vec<ModId>),
    Undo,
}

type Outcome = Result<(String, Vec<String>), String>;

fn run_action(deps: &Deps, action: &Action, names: &[(ModId, String)]) -> Outcome {
    let paths: GamePaths = (deps.locate)().map_err(|e| game_error_text(&e))?;
    let running = deps.running.clone();
    match action {
        Action::Apply(ids) => shiv_repair::apply_with(&paths, ids, move || running())
            .map(|report| apply_result(&report, names))
            .map_err(|e| error_text(&e)),
        Action::Undo => shiv_repair::restore_last_with(&paths, move || running())
            .map(|report| undo_result(&report))
            .map_err(|e| error_text(&e)),
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> bool {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(f)
        .is_ok()
}

// ---------------------------------------------------------------------------
// Controller

struct Repair {
    weak: Weak<App>,
    deps: Deps,
    rows: Rc<VecModel<RepairRow>>,
    result_lines: Rc<VecModel<SharedString>>,
    /// The last check's mods, for Apply's targets and names in results.
    mods: RefCell<Vec<ModHealth>>,
    generation: Cell<u64>,
}

/// Wire the Repair screen to `app` using the real game location.
pub fn attach(app: &App) {
    attach_with(app, Deps::system());
}

pub(crate) fn attach_with(app: &App, deps: Deps) {
    let rows = Rc::new(VecModel::default());
    let result_lines = Rc::new(VecModel::default());
    let controller = Rc::new(Repair {
        weak: app.as_weak(),
        deps,
        rows: rows.clone(),
        result_lines: result_lines.clone(),
        mods: RefCell::new(Vec::new()),
        generation: Cell::new(0),
    });
    REPAIR.with(|c| *c.borrow_mut() = Some(controller));
    let state = app.global::<RepairState>();
    state.set_rows(ModelRc::from(rows));
    state.set_result_lines(ModelRc::from(result_lines));
    state.on_shown(|| with_repair(|r| r.rescan()));
    state.on_rescan(|| with_repair(|r| r.rescan()));
    state.on_apply(|| with_repair(|r| r.apply()));
    state.on_undo(|| with_repair(|r| r.act(Action::Undo)));
    state.on_dismiss_result(|| with_repair(|r| r.show_result(None)));
}

impl Repair {
    fn with_state(&self, f: impl FnOnce(&RepairState<'_>)) {
        if let Some(app) = self.weak.upgrade() {
            f(&app.global::<RepairState>());
        }
    }

    /// A check or an action is running.
    fn locked(&self) -> bool {
        self.weak.upgrade().is_none_or(|a| {
            let s = a.global::<RepairState>();
            s.get_busy() || s.get_scanning()
        })
    }

    /// Start a check unless one or an action is already running: a check
    /// reads every game VPK, so repeated clicks must not pile up threads.
    fn rescan(&self) {
        if self.locked() {
            return;
        }
        self.start_scan();
    }

    fn start_scan(&self) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.with_state(|s| {
            s.set_scanning(true);
            if s.get_load_state() != LoadState::Ready {
                s.set_load_state(LoadState::Loading);
            }
        });
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-repair-scan", move || {
            let result = scan(&deps);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_repair(|r| r.scanned(generation, result));
            });
        });
        if !started {
            self.fail("Could not start a background thread.".into());
        }
    }

    fn fail(&self, text: String) {
        self.rows.set_vec(Vec::new());
        self.mods.borrow_mut().clear();
        self.with_state(|s| {
            s.set_scanning(false);
            s.set_fixable_count(0);
            s.set_can_apply(false);
            s.set_error_text(text.into());
            s.set_load_state(LoadState::Failed);
        });
    }

    fn scanned(&self, generation: u64, result: Result<Scan, String>) {
        if generation != self.generation.get() {
            return;
        }
        let scan = match result {
            Ok(s) => s,
            Err(text) => return self.fail(text),
        };
        self.rows
            .set_vec(scan.mods.iter().map(row).collect::<Vec<_>>());
        let fixable = fixable(&scan.mods);
        let targets = targets(&scan.mods).len();
        let (label, can_apply) = apply_label(fixable, scan.stale);
        self.with_state(|s| {
            s.set_scanning(false);
            s.set_game_running(scan.running);
            s.set_stale(scan.stale);
            s.set_can_undo(scan.can_undo);
            s.set_fixable_count(i32::try_from(fixable).unwrap_or(i32::MAX));
            s.set_summary(summary(&scan.mods).into());
            s.set_apply_label(label.into());
            s.set_can_apply(can_apply);
            s.set_confirm_apply_title(confirm_title(fixable, targets).into());
            s.set_load_state(LoadState::Ready);
        });
        *self.mods.borrow_mut() = scan.mods;
    }

    fn apply(&self) {
        let ids = targets(&self.mods.borrow());
        self.act(Action::Apply(ids));
    }

    fn act(&self, action: Action) {
        self.with_state(|s| s.set_confirm(0));
        if self.locked() {
            return;
        }
        let names: Vec<(ModId, String)> = self
            .mods
            .borrow()
            .iter()
            .map(|m| (m.mod_id.clone(), m.name.clone()))
            .collect();
        let text = match action {
            Action::Apply(_) => "Backing up and writing the repair package",
            Action::Undo => "Undoing the last repair",
        };
        self.with_state(|s| {
            s.set_busy(true);
            s.set_busy_text(text.into());
        });
        self.show_result(None);
        let failed_title = match action {
            Action::Apply(_) => "Repair failed",
            Action::Undo => "Undo failed",
        };
        let deps = self.deps.clone();
        let weak = self.weak.clone();
        let started = spawn("shiv-repair-action", move || {
            let result = run_action(&deps, &action, &names);
            let _ = weak.upgrade_in_event_loop(move |_| {
                with_repair(|r| r.acted(failed_title, result));
            });
        });
        if !started {
            self.acted(
                failed_title,
                Err("Could not start a background thread.".into()),
            );
        }
    }

    fn acted(&self, failed_title: &str, result: Outcome) {
        self.show_result(Some(match result {
            Ok((title, lines)) => (title, lines, false),
            Err(text) => (failed_title.to_owned(), vec![text], true),
        }));
        self.with_state(|s| s.set_busy(false));
        // gameinfo.gi and the package may have changed: keep the Launch check
        // current, and re-check the mods (also refreshes game-running).
        crate::launch::refresh();
        self.start_scan();
    }

    fn show_result(&self, result: Option<(String, Vec<String>, bool)>) {
        let (title, lines, error) = result.unwrap_or_default();
        self.result_lines.set_vec(
            lines
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        );
        self.with_state(|s| {
            s.set_result_title(title.into());
            s.set_result_error(error);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shiv_repair::mod_health::FixModsStatus;
    use slint::Model;

    fn health(id: &str, status: RepairStatus, enabled: bool) -> ModHealth {
        ModHealth {
            mod_id: id.into(),
            name: format!("Mod {id}"),
            files: vec!["pak01_dir.vpk".into()],
            enabled,
            status,
            summary: "s".into(),
            changes: Vec::new(),
            remaining: Vec::new(),
            error: None,
            detail: None,
        }
    }

    #[test]
    fn statuses_map_to_labels_and_tones() {
        let cases = [
            (RepairStatus::Works, "Works", RepairTone::Works),
            (RepairStatus::Fixable, "Fixable", RepairTone::Fixable),
            (
                RepairStatus::NeedsUpdate,
                "Needs update",
                RepairTone::Update,
            ),
            (RepairStatus::Fixed, "Repaired", RepairTone::Fixed),
            (
                RepairStatus::NotChecked,
                "Not checked",
                RepairTone::Unchecked,
            ),
        ];
        for (status, label, t) in cases {
            assert_eq!(status_label(status), label);
            assert_eq!(tone(status), t);
        }
    }

    #[test]
    fn rows_cap_changes_and_carry_errors() {
        let mut m = health("a", RepairStatus::Fixable, true);
        m.changes = (1..=9).map(|i| format!("change {i}")).collect();
        m.remaining = vec!["Animation problems remain".into()];
        let r = row(&m);
        assert_eq!(r.changes.row_count(), MAX_CHANGES);
        assert_eq!(r.changes.row_data(0).unwrap().as_str(), "change 1");
        assert_eq!(
            r.changes.row_data(MAX_CHANGES - 1).unwrap().as_str(),
            "and 4 more"
        );
        assert_eq!(r.files.as_str(), "pak01_dir.vpk");
        assert_eq!(r.remaining.row_count(), 1);

        let mut e = health("b", RepairStatus::NotChecked, true);
        e.error = Some("Could not read the game files".into());
        assert_eq!(
            row(&e).remaining.row_data(0).unwrap().as_str(),
            "Could not read the game files"
        );
        // A hand-added VPK whose name is its file does not repeat it.
        let mut f = health("file:custom.vpk", RepairStatus::Works, true);
        f.name = "pak01_dir.vpk".into();
        assert_eq!(row(&f).files.as_str(), "");
        // Disabled fixable mods say why Apply leaves them out.
        let d = row(&health("c", RepairStatus::Fixable, false));
        assert!(d.remaining.row_data(0).unwrap().contains("Disabled"));
        assert_eq!(capped(&["x".into()]), vec!["x".to_string()]);
    }

    #[test]
    fn targets_keep_repaired_mods_and_skip_disabled_ones() {
        let mods = vec![
            health("fix", RepairStatus::Fixable, true),
            health("done", RepairStatus::Fixed, true),
            health("off", RepairStatus::Fixable, false),
            health("ok", RepairStatus::Works, true),
            health("old", RepairStatus::NeedsUpdate, true),
        ];
        assert_eq!(targets(&mods), vec!["fix".to_string(), "done".to_string()]);
        assert_eq!(fixable(&mods), 1);
        assert_eq!(summary(&mods), "5 mods, 1 fixable, 1 need an update");
        assert_eq!(
            summary(&[health("ok", RepairStatus::Works, true)]),
            "1 mod, nothing to repair"
        );
        assert_eq!(summary(&[]), "0 mods");
    }

    #[test]
    fn apply_label_follows_fixable_count_and_stale_package() {
        assert_eq!(apply_label(0, false), ("Repair mods".into(), false));
        assert_eq!(apply_label(1, false), ("Repair 1 mod".into(), true));
        assert_eq!(apply_label(3, false), ("Repair 3 mods".into(), true));
        assert_eq!(apply_label(2, true), ("Re-apply repairs".into(), true));
        assert_eq!(apply_label(0, true), ("Remove old repairs".into(), true));
        assert_eq!(confirm_title(1, 1), "Repair 1 mod?");
        assert_eq!(confirm_title(0, 2), "Re-apply repairs for 2 mods?");
        assert_eq!(confirm_title(0, 0), "Remove the outdated repair package?");
    }

    #[test]
    fn results_read_plainly() {
        let report = FixReport {
            status: FixModsStatus::Applied,
            fixed: vec!["a".into()],
            skipped: vec![("b".into(), "Nothing to repair".into())],
            overlay: None,
            gameinfo_changed: true,
            backup: Some(PathBuf::from("/x/backup-1")),
            packed_files: 1,
            notes: vec!["2 models are replaced by more than one mod".into()],
        };
        let names = vec![("b".to_string(), "Hud".to_string())];
        let (title, lines) = apply_result(&report, &names);
        assert_eq!(title, "Repaired 1 mod");
        assert_eq!(lines[0], "Hud: Nothing to repair");
        assert_eq!(lines[1], "2 models are replaced by more than one mod");
        assert!(lines[2].starts_with("gameinfo.gi now loads"));
        assert_eq!(lines[3], "Backup of the replaced files: /x/backup-1");

        let undo = shiv_repair::RestoreReport {
            backup: PathBuf::from("/x/restored-backup-1"),
            restored: vec![PathBuf::from("/g/gameinfo.gi")],
            removed: vec![PathBuf::from("/g/shiv_repair/pak01_dir.vpk")],
        };
        let (title, lines) = undo_result(&undo);
        assert_eq!(title, "Last repair undone");
        assert_eq!(lines[0], "Put back: gameinfo.gi");
        assert_eq!(lines[1], "Removed: pak01_dir.vpk");
        assert_eq!(error_text(&RepairError::GameRunning), GAME_RUNNING_TEXT);
        assert_eq!(
            error_text(&RepairError::BackupRestoreFailed(
                "There is no repair to undo".into()
            )),
            "There is no repair to undo."
        );
    }
}
