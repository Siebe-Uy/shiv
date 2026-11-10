// SPDX-License-Identifier: GPL-3.0-only
//! Shiv's repair API: [`analyze`] previews, [`apply`] writes.
//!
//! The orchestration follows Deadlock Mod Manager's (GPL-3.0)
//! `mod_manager/manager/localization/{health,receipt,history}.rs`, without its
//! profiles, manifest, Tauri events or review dialog:
//!
//! - [`analyze`] checks every installed mod on its own against the installed
//!   game (`health.rs::check_mod`) and reports what a repair would change. It
//!   reads files only.
//! - [`apply`] builds one repair package for all loaded mods, with repairs on
//!   for exactly the requested ones (`build_learning_history`), and writes it
//!   to `game/citadel/shiv_repair/pak01_dir.vpk`. A `Game citadel/shiv_repair`
//!   search path placed before `citadel/addons` makes the game load it ahead
//!   of the mods. Mod VPKs are never modified. Every existing file `apply`
//!   replaces or rewrites (the previous package, its receipt and learned game
//!   data, `gameinfo.gi`) is copied with [`create_backup_set`] first. A failed
//!   write restores that set, and [`restore_last`] undoes the last apply.
//! - A receipt beside the package records the inputs it was built from, so a
//!   later [`analyze`] reports the mods as fixed only while the package still
//!   matches them (`receipt.rs`).

use crate::error::RepairError;
use crate::gameinfo::{self, OVERLAY_DIR};
use crate::localization_overlay::asset_compatibility::{
    RepairKind as AssetRepairKind, WarningKind as AssetWarningKind,
};
use crate::localization_overlay::vdata_compatibility::{
    RepairKind as DataRepairKind, WarningReason,
};
use crate::localization_overlay::{
    self, GameIndex, LocalizationModInput, LocalizationOverlayAnalysis, LocalizationOverlayPlan,
    OVERLAY_VPK_NAME,
};
use crate::mod_health::{
    self, CompatibilityChoice, FixModsStatus, ModFindings, ModHealthSource, ModHealthStatus,
    ModHealthSymptom,
};
use crate::repair_backup::{create_backup_set, latest_backup_set, restore_backup_set};
use crate::vdata_history::{BaselineStatus, VdataHistoryIndex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use shiv_core::addons::{self, InstalledVpk};
use shiv_core::game::{self, GamePaths};
use shiv_core::sidecar;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Increment when repair rules or output change, so packages count as stale.
/// Starts from the old app's `REPAIR_REVISION` at the time of the port.
const REPAIR_REVISION: u32 = 40;
const RECEIPT_NAME: &str = "shiv-repair.json";
/// Game data tables learned from the installed game (old `history.rs`), so a
/// later game update can still tell mod edits from old game values.
const HISTORY_NAME: &str = "observed-history.bin";
const DISABLED_EXT: &str = ".disabled";

/// A mod as Shiv groups it: the sidecar's mod id, or `file:<vpk name>` (the
/// enabled-form file name) for a VPK without a sidecar.
pub type ModId = String;

/// The verdict the repair screen shows for a mod.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairStatus {
    /// Nothing to repair (including mods with nothing the engine checks).
    Works,
    /// Repairs are available; [`ModHealth::changes`] lists them.
    Fixable,
    /// Problems remain that no repair addresses; the author must update it.
    NeedsUpdate,
    /// The current repair package already holds this mod's repairs.
    Fixed,
    /// The mod could not be read; [`ModHealth::error`] says why.
    NotChecked,
}

/// One mod's health and what repairing it would change.
#[derive(Debug, Clone, PartialEq)]
pub struct ModHealth {
    pub mod_id: ModId,
    /// Sidecar name, or the file name for a VPK without a sidecar.
    pub name: String,
    /// The mod's VPK file names as they are on disk.
    pub files: Vec<String>,
    pub enabled: bool,
    pub status: RepairStatus,
    /// One line for the list, e.g. "1 repair available".
    pub summary: String,
    /// What the repair package changes for this mod, one line per repaired
    /// file or row, e.g. "scripts/heroes.vdata_c: restore game data the mod
    /// leaves out". Empty when there is nothing to repair.
    pub changes: Vec<String>,
    /// Problems the repair does not address.
    pub remaining: Vec<String>,
    pub error: Option<String>,
    /// The engine's detailed verdict (counts, symptoms); `None` when not checked.
    pub detail: Option<mod_health::ModHealth>,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct FixReport {
    pub status: FixModsStatus,
    /// Mods whose repairs are now in the package.
    pub fixed: Vec<ModId>,
    /// Requested mods that were not repaired, with the reason.
    pub skipped: Vec<(ModId, String)>,
    /// The package written, or `None` when there was nothing to repair (any
    /// previous package was then removed).
    pub overlay: Option<PathBuf>,
    /// `gameinfo.gi` gained the `Game citadel/shiv_repair` mount.
    pub gameinfo_changed: bool,
    /// Backup set written before anything changed (`files/<name>` plus
    /// `repair.json` with each file's folder); [`restore_last`] undoes this
    /// apply from it. `None` when nothing on disk changed.
    pub backup: Option<PathBuf>,
    pub packed_files: usize,
    /// Choices the old app asked the player to review; Shiv applies the
    /// game's own load order for them and reports them here.
    pub notes: Vec<String>,
}

/// Whether the repair package on disk still matches the installed mods and game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    Absent,
    Current,
    /// Mods or the game changed since [`apply`]; the package may now repair
    /// files that are gone or changed. Apply again (or with no mods) to fix it.
    Stale,
}

/// `game/citadel` of the install.
fn citadel(paths: &GamePaths) -> PathBuf {
    paths.root.join("game").join("citadel")
}

/// Folder that holds the repair package, its receipt and learned game data.
pub fn overlay_dir(paths: &GamePaths) -> PathBuf {
    citadel(paths).join(OVERLAY_DIR)
}

fn backup_dir(paths: &GamePaths) -> PathBuf {
    paths.addons.join(".shiv").join("repair-backups")
}

/// The installed game build (`ClientVersion` in `game/citadel/steam.inf`).
/// Pass it to [`analyze`]; [`apply`] reads it itself.
pub fn game_build(paths: &GamePaths) -> Option<u64> {
    fs::read_to_string(citadel(paths).join("steam.inf"))
        .ok()?
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("ClientVersion=")
                .and_then(|version| version.trim().parse().ok())
        })
}

fn version(build_id: u64) -> Option<u32> {
    u32::try_from(build_id).ok().filter(|build| *build != 0)
}

// ---------------------------------------------------------------------------
// Installed mods

struct Installed {
    id: ModId,
    name: String,
    files: Vec<String>,
    enabled: bool,
    vpks: Vec<InstalledVpk>,
}

impl Installed {
    fn input(&self) -> LocalizationModInput {
        LocalizationModInput {
            mod_id: self.id.clone(),
            vpks: self.vpks.iter().map(|vpk| vpk.file.clone()).collect(),
        }
    }

    /// VPKs the game loads: enabled `pakNN_dir.vpk` files.
    fn mounted(&self) -> impl Iterator<Item = (u32, &InstalledVpk)> {
        self.vpks
            .iter()
            .filter(|vpk| vpk.enabled)
            .filter_map(|vpk| Some((vpk.pak?, vpk)))
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn installed(addons_dir: &Path) -> Result<Vec<Installed>, RepairError> {
    // `group` hides listing errors, so surface them first.
    addons::list(addons_dir).map_err(|error| RepairError::InvalidInput(error.to_string()))?;
    Ok(sidecar::group(addons_dir)
        .into_iter()
        .map(|group| {
            let files: Vec<String> = group.vpks.iter().map(|vpk| file_name(&vpk.file)).collect();
            let (id, name) = match &group.sidecar {
                Some(sidecar) => (sidecar.mod_id.clone(), sidecar.name.clone()),
                None => {
                    let first = files.first().cloned().unwrap_or_default();
                    let stem = first.strip_suffix(DISABLED_EXT).unwrap_or(&first);
                    (format!("file:{stem}"), stem.to_owned())
                }
            };
            Installed {
                id,
                name,
                files,
                enabled: group.vpks.iter().any(|vpk| vpk.enabled),
                vpks: group.vpks,
            }
        })
        .collect())
}

/// Every loaded VPK in engine order (pak number), one input each, as in the
/// old `inventory::mounted_inputs`.
fn mounted(installed: &[Installed]) -> Vec<(u32, &str, &InstalledVpk)> {
    let mut mounted: Vec<_> = installed
        .iter()
        .flat_map(|m| m.mounted().map(move |(pak, vpk)| (pak, m.id.as_str(), vpk)))
        .collect();
    mounted.sort_by_key(|(pak, _, _)| *pak);
    mounted
}

// ---------------------------------------------------------------------------
// Receipt and learned history (old `receipt.rs`, `history.rs`)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct InputStamp {
    mod_id: String,
    file: String,
    length: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Receipt {
    revision: u32,
    game_build: Option<u32>,
    inputs: Vec<InputStamp>,
    fixed: BTreeSet<ModId>,
    overlay_hash: Option<String>,
}

fn stamp(mod_id: &str, path: &Path) -> io::Result<InputStamp> {
    let metadata = fs::metadata(path)?;
    Ok(InputStamp {
        mod_id: mod_id.to_owned(),
        file: file_name(path),
        length: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn stamps(installed: &[Installed]) -> io::Result<Vec<InputStamp>> {
    let mut out = Vec::new();
    for (_, mod_id, vpk) in mounted(installed) {
        out.push(stamp(mod_id, &vpk.file)?);
        for chunk in &vpk.chunks {
            out.push(stamp(mod_id, chunk)?);
        }
    }
    Ok(out)
}

fn digest(path: &Path) -> io::Result<Option<String>> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let length = file.read(&mut buffer)?;
        if length == 0 {
            break;
        }
        hasher.update(&buffer[..length]);
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

fn load_receipt(dir: &Path) -> Option<Receipt> {
    let bytes = fs::read(dir.join(RECEIPT_NAME)).ok()?;
    match serde_json::from_slice::<Receipt>(&bytes) {
        Ok(receipt) if receipt.revision == REPAIR_REVISION => Some(receipt),
        _ => None,
    }
}

impl Receipt {
    fn is_current(&self, game_build: Option<u32>, inputs: &[InputStamp], overlay: &Path) -> bool {
        self.revision == REPAIR_REVISION
            && self.game_build == game_build
            && self.inputs == inputs
            && self.overlay_hash.is_some()
            && digest(overlay).ok().flatten() == self.overlay_hash
    }
}

fn mounts_overlay(paths: &GamePaths) -> bool {
    fs::read_to_string(&paths.gameinfo)
        .ok()
        .and_then(|text| gameinfo::mounts_overlay(&text).ok())
        .unwrap_or(false)
}

/// Mods whose repairs the mounted, current package holds.
fn applied(paths: &GamePaths, version: Option<u32>, installed: &[Installed]) -> BTreeSet<ModId> {
    let dir = overlay_dir(paths);
    let Some(receipt) = load_receipt(&dir) else {
        return BTreeSet::new();
    };
    let Ok(inputs) = stamps(installed) else {
        return BTreeSet::new();
    };
    if receipt.is_current(version, &inputs, &dir.join(OVERLAY_VPK_NAME)) && mounts_overlay(paths) {
        receipt.fixed
    } else {
        BTreeSet::new()
    }
}

/// Whether the package on disk still matches the installed mods and game.
pub fn overlay_state(paths: &GamePaths) -> OverlayState {
    let dir = overlay_dir(paths);
    let overlay = dir.join(OVERLAY_VPK_NAME);
    if !overlay.is_file() {
        return OverlayState::Absent;
    }
    let current = load_receipt(&dir).is_some_and(|receipt| {
        installed(&paths.addons)
            .ok()
            .and_then(|installed| stamps(&installed).ok())
            .is_some_and(|inputs| {
                receipt.is_current(game_build(paths).and_then(version), &inputs, &overlay)
            })
    });
    if current {
        OverlayState::Current
    } else {
        OverlayState::Stale
    }
}

fn load_history(dir: &Path) -> VdataHistoryIndex {
    match fs::read(dir.join(HISTORY_NAME)) {
        Ok(bytes) => VdataHistoryIndex::from_bytes(&bytes).unwrap_or_else(|error| {
            log::warn!("Ignoring unreadable learned game data: {error}");
            VdataHistoryIndex::default()
        }),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!("Could not read learned game data: {error}");
            }
            VdataHistoryIndex::default()
        }
    }
}

/// Replace `path` with `bytes` through a synced temp file in the same folder.
fn write_atomic(
    path: &Path,
    bytes: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), RepairError> {
    let parent = path
        .parent()
        .ok_or_else(|| RepairError::InvalidInput(format!("{} has no parent", path.display())))?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    if let Some(permissions) = permissions {
        fs::set_permissions(staged.path(), permissions)?;
    }
    staged
        .persist(path)
        .map_err(|error| RepairError::Io(error.error))?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), RepairError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// ---------------------------------------------------------------------------
// Player-facing text

fn symptom_label(symptom: ModHealthSymptom) -> &'static str {
    match symptom {
        ModHealthSymptom::CameraAim => "Camera and aim",
        ModHealthSymptom::Animations => "Animation",
        ModHealthSymptom::Appearance => "Material and texture",
        ModHealthSymptom::Model => "Model",
        ModHealthSymptom::GameData => "Game data",
        ModHealthSymptom::CrashRisk => "Crash risk",
    }
}

fn asset_repair_label(kind: &AssetRepairKind) -> &'static str {
    match kind {
        AssetRepairKind::AnimationSkeleton | AssetRepairKind::AnimationSkeletonRebased => {
            "match the animation skeleton to the current hero"
        }
        AssetRepairKind::AnimationInterface => "update the animation interface",
        AssetRepairKind::AnimationClipTracks => "reorder animation clip tracks",
        AssetRepairKind::AnimationRigRebase => "rebase animations onto the current rig",
        AssetRepairKind::CameraInterface => "update the camera interface",
        AssetRepairKind::CameraControls => "update camera controls",
        AssetRepairKind::CameraAttachmentFrame => "fix the camera attachment",
        AssetRepairKind::MaterialBindings => "update material bindings",
        AssetRepairKind::ModelRelocation => "move the model to its current path",
        AssetRepairKind::TaggedSounds => "update sound tags",
    }
}

fn data_repair_label(kind: &DataRepairKind) -> &'static str {
    match kind {
        DataRepairKind::HeroDevelopmentState => "keep the hero's current availability",
        DataRepairKind::InheritedDefinition => "add a definition the game now requires",
        DataRepairKind::RetainedDependency => "keep an entry the mod depends on",
        DataRepairKind::CurrentReference => "point a reference at its current target",
        DataRepairKind::EnumMapEntry => "update a value the game renamed",
        DataRepairKind::RestoredGameData => "restore game data the mod leaves out",
        DataRepairKind::RestoredGameText => "restore game text the mod leaves out",
    }
}

/// One line per repair the package makes for `mod_id`.
fn changes(analysis: &LocalizationOverlayAnalysis, mod_id: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |line: String| {
        if !out.contains(&line) {
            out.push(line);
        }
    };
    for repair in analysis.asset_repairs.iter().filter(|r| r.mod_id == mod_id) {
        push(format!(
            "{}: {}",
            repair.file_path,
            asset_repair_label(&repair.kind)
        ));
    }
    for repair in analysis.data_repairs.iter().filter(|r| r.mod_id == mod_id) {
        let label = data_repair_label(&repair.kind);
        if repair.row_name.is_empty() {
            push(format!("{}: {label}", repair.file_path));
        } else {
            push(format!(
                "{} ({}): {label}",
                repair.file_path, repair.row_name
            ));
        }
    }
    out
}

fn describe(m: &Installed, detail: mod_health::ModHealth, changes: Vec<String>) -> ModHealth {
    let repairs = detail.repairs;
    let plural = if repairs == 1 { "" } else { "s" };
    let (status, summary) = match detail.status {
        ModHealthStatus::Works => (
            RepairStatus::Works,
            "Works with the current game".to_owned(),
        ),
        ModHealthStatus::NotCovered => (
            RepairStatus::Works,
            "Nothing in this mod needs checking".to_owned(),
        ),
        ModHealthStatus::Fixable | ModHealthStatus::OriginalFiles => (
            RepairStatus::Fixable,
            format!("{repairs} repair{plural} available"),
        ),
        ModHealthStatus::PartlyFixable => (
            RepairStatus::Fixable,
            format!("{repairs} repair{plural} available; some problems remain"),
        ),
        ModHealthStatus::Fixed => (RepairStatus::Fixed, "Repaired".to_owned()),
        ModHealthStatus::NeedsUpdate => (
            RepairStatus::NeedsUpdate,
            "Needs an update from its author".to_owned(),
        ),
    };
    let remaining = detail
        .remaining_symptoms
        .iter()
        .map(|symptom| {
            format!(
                "{} problems remain until the author updates the mod",
                symptom_label(*symptom)
            )
        })
        .collect();
    ModHealth {
        mod_id: m.id.clone(),
        name: m.name.clone(),
        files: m.files.clone(),
        enabled: m.enabled,
        status,
        summary,
        changes,
        remaining,
        error: None,
        detail: Some(detail),
    }
}

fn not_checked(m: &Installed, reason: String) -> ModHealth {
    ModHealth {
        mod_id: m.id.clone(),
        name: m.name.clone(),
        files: m.files.clone(),
        enabled: m.enabled,
        status: RepairStatus::NotChecked,
        summary: "Could not be checked".to_owned(),
        changes: Vec::new(),
        remaining: Vec::new(),
        error: Some(reason),
        detail: None,
    }
}

/// What the old app paused launch for (`requires_review`). Shiv has no review
/// dialog: the package keeps the game's own load order for these, so they are
/// reported instead.
fn review_notes(analysis: &LocalizationOverlayAnalysis) -> Vec<String> {
    let mut notes = Vec::new();
    let mut note = |count: usize, text: &str| {
        if count > 0 {
            notes.push(format!("{count} {text}"));
        }
    };
    note(
        analysis.conflicts.len() + analysis.compiled_data_conflicts.len(),
        "text or data entries are changed by more than one mod; the mod loaded first wins",
    );
    note(
        analysis
            .baselines
            .iter()
            .filter(|baseline| baseline.evidence.status != BaselineStatus::Matched)
            .count(),
        "game data files could not be matched to a known game build; the mod's values were kept",
    );
    note(
        analysis.snapshot_warnings.len() + analysis.parse_warnings.len(),
        "text files could not be read completely",
    );
    note(
        analysis
            .asset_warnings
            .iter()
            .filter(|warning| {
                warning.kind == AssetWarningKind::OverlappingModel
                    && !analysis.unloaded_models.contains(&warning.file_path)
            })
            .count(),
        "models are replaced by more than one mod",
    );
    note(
        analysis
            .data_warnings
            .iter()
            .filter(|warning| {
                matches!(
                    warning.reason,
                    WarningReason::CompetingDefinitions | WarningReason::UnreadableResource
                )
            })
            .count(),
        "game data definitions compete or could not be read",
    );
    notes
}

// ---------------------------------------------------------------------------
// analyze / apply

fn check_mod(
    game: &Arc<GameIndex>,
    m: &Installed,
    observed: &mut VdataHistoryIndex,
    version: Option<u32>,
    recorded: &mut BTreeSet<String>,
) -> Result<(ModFindings, Vec<String>), RepairError> {
    let input = m.input();
    if !localization_overlay::has_checkable_resources(&input)? {
        return Ok((ModFindings::uncovered(), Vec::new()));
    }
    let (plan, _) =
        LocalizationOverlayPlan::build_standalone(game, input, observed, version, recorded)?;
    Ok((
        ModFindings::from_analysis(&plan.analysis, &m.id, true),
        changes(&plan.analysis, &m.id),
    ))
}

/// Checks every installed mod, enabled or not, on its own against the
/// installed game and returns its verdict and what a repair would change.
/// Reads files only: nothing is written, renamed or created. `build_id` is
/// [`game_build`] (0 when unknown). A mod that cannot be read is reported as
/// [`RepairStatus::NotChecked`]; an unreadable addons folder yields no rows.
pub fn analyze(paths: &GamePaths, build_id: u64) -> Vec<ModHealth> {
    let installed = match installed(&paths.addons) {
        Ok(installed) => installed,
        Err(error) => {
            log::warn!("Could not list mods for a repair check: {error}");
            return Vec::new();
        }
    };
    if installed.is_empty() {
        return Vec::new();
    }
    let version = version(build_id);
    let applied = applied(paths, version, &installed);
    let game = match GameIndex::open(&citadel(paths)) {
        Ok(game) => Arc::new(game),
        Err(error) => {
            let reason = format!("Could not read the game files: {error}");
            return installed
                .iter()
                .map(|m| not_checked(m, reason.clone()))
                .collect();
        }
    };
    // Learned only in memory here; `apply` saves what it learns.
    let mut observed = load_history(&overlay_dir(paths));
    // Decoded once for all mods here and freed when the check returns.
    let _embedded = VdataHistoryIndex::embedded();
    let mut recorded = BTreeSet::new();
    let checked_at = chrono::Utc::now();
    let mut out = Vec::with_capacity(installed.len());
    for m in &installed {
        match check_mod(&game, m, &mut observed, version, &mut recorded) {
            Ok((findings, changes)) => {
                let choice = if applied.contains(&m.id) {
                    CompatibilityChoice::Enabled
                } else {
                    CompatibilityChoice::Unset
                };
                let detail = findings.health(
                    &m.id,
                    choice,
                    version,
                    checked_at,
                    ModHealthSource::Standalone,
                );
                out.push(describe(m, detail, changes));
            }
            Err(error) => {
                log::warn!("Skipped repair check for mod {}: {error}", m.id);
                out.push(not_checked(m, error.to_string()));
            }
        }
    }
    out
}

/// Repairs exactly `mods` (repairs of mods not listed are dropped; an empty
/// list removes the package). Refuses while the game runs. See the module
/// docs for what is written and backed up.
pub fn apply(paths: &GamePaths, mods: &[ModId]) -> Result<FixReport, RepairError> {
    apply_with(paths, mods, game::is_running)
}

/// [`apply`] with an injectable running-game check.
pub fn apply_with(
    paths: &GamePaths,
    mods: &[ModId],
    game_running: impl Fn() -> bool,
) -> Result<FixReport, RepairError> {
    // Checked first: nothing may be written while the game runs.
    if game_running() {
        return Err(RepairError::GameRunning);
    }
    let installed = installed(&paths.addons)?;
    let citadel = citadel(paths);
    let dir = overlay_dir(paths);
    let overlay = dir.join(OVERLAY_VPK_NAME);
    let version = game_build(paths).and_then(version);

    let mut selected = BTreeSet::new();
    let mut skipped = Vec::new();
    for id in mods {
        match installed.iter().find(|m| &m.id == id) {
            None => skipped.push((id.clone(), "Not installed".to_owned())),
            Some(m) if m.mounted().next().is_none() => skipped.push((
                id.clone(),
                "The game does not load this mod; enable it first".to_owned(),
            )),
            Some(_) => {
                selected.insert(id.clone());
            }
        }
    }

    let inputs: Vec<LocalizationModInput> = mounted(&installed)
        .into_iter()
        .map(|(_, mod_id, vpk)| LocalizationModInput {
            mod_id: mod_id.to_owned(),
            vpks: vec![vpk.file.clone()],
        })
        .collect();
    let stamps = stamps(&installed)?;

    // Build and validate everything before the first write.
    let mut plan = None;
    let mut learned = None;
    let mut notes = Vec::new();
    // Decoded once for this repair and freed when it returns.
    let _embedded = VdataHistoryIndex::embedded();
    if !selected.is_empty() {
        let (built, observed, changed) = LocalizationOverlayPlan::build_learning_history(
            &citadel,
            &inputs,
            load_history(&dir),
            version,
            &selected,
            &|_| {},
        )?;
        for id in selected.clone() {
            let repaired = built.analysis.asset_repairs.iter().any(|r| r.mod_id == id)
                || built.analysis.data_repairs.iter().any(|r| r.mod_id == id);
            if !repaired {
                selected.remove(&id);
                skipped.push((id, "Nothing to repair".to_owned()));
            }
        }
        notes = review_notes(&built.analysis);
        learned = changed.then_some(observed);
        plan = Some(built);
    }
    let mut new_gameinfo = None;
    let mut gameinfo_permissions = None;
    if !selected.is_empty() {
        let text = fs::read_to_string(&paths.gameinfo)?;
        new_gameinfo = gameinfo::with_overlay(&text)?;
        gameinfo_permissions = Some(fs::metadata(&paths.gameinfo)?.permissions());
    }

    // Removes the package folder again if this call created it and leaves it
    // empty (nothing to repair, or a rolled-back write).
    let dir_guard = DirGuard {
        dir: dir.clone(),
        existed: dir.is_dir(),
    };
    let mut result = None;
    let mut staged = None;
    if let Some(plan) = plan.as_ref().filter(|_| !selected.is_empty()) {
        fs::create_dir_all(&dir)?;
        // The packer replaces this path; staged beside the package so the
        // final rename stays on one filesystem.
        let path = tempfile::NamedTempFile::new_in(&dir)?.into_temp_path();
        let written = plan.write(&path, &[])?;
        if written.has_overlay {
            result = Some(written);
            staged = Some(path);
        }
    }
    if result.is_none() {
        // Nothing to package: no mount is needed and the receipt goes too.
        new_gameinfo = None;
        selected.clear();
    }

    // Back up, in one set, every file this call may replace, create or
    // remove; absent files are recorded so a restore removes them again.
    let mut targets: Vec<(&Path, &str)> = vec![(&dir, OVERLAY_VPK_NAME), (&dir, RECEIPT_NAME)];
    if learned.is_some() {
        targets.push((&dir, HISTORY_NAME));
    }
    let gameinfo_name = file_name(&paths.gameinfo);
    let gameinfo_dir = paths.gameinfo.parent().unwrap_or(&citadel);
    if new_gameinfo.is_some() {
        targets.push((gameinfo_dir, &gameinfo_name));
    }
    let writes = result.is_some() || targets.iter().any(|(d, n)| d.join(n).is_file());
    let backup = if writes {
        let new_dirs: Vec<&Path> = if dir_guard.existed {
            Vec::new()
        } else {
            vec![&dir]
        };
        Some(create_backup_set(&backup_dir(paths), &targets, &new_dirs)?)
    } else {
        None
    };

    let gameinfo_changed = new_gameinfo.is_some();
    let packed_files = result.as_ref().map_or(0, |result| result.packed_files);
    let fixed: Vec<ModId> = selected.iter().cloned().collect();
    let write = || -> Result<(), RepairError> {
        // Package, learned data, receipt, then the mount.
        if let Some(staged) = staged {
            staged
                .persist(&overlay)
                .map_err(|error| RepairError::Io(error.error))?;
        } else {
            remove_if_exists(&overlay)?;
        }
        if let Some(observed) = learned {
            // Learning is reproducible; a failed save must not fail the repair.
            match observed.to_bytes() {
                Ok(bytes) => {
                    if let Err(error) = write_atomic(&dir.join(HISTORY_NAME), &bytes, None) {
                        log::warn!("Could not save learned game data: {error}");
                    }
                }
                Err(error) => log::warn!("Could not encode learned game data: {error}"),
            }
        }
        if result.is_some() {
            let receipt = Receipt {
                revision: REPAIR_REVISION,
                game_build: version,
                inputs: stamps,
                fixed: selected,
                overlay_hash: digest(&overlay)?,
            };
            let bytes = serde_json::to_vec_pretty(&receipt).map_err(|error| {
                RepairError::InvalidInput(format!("Could not record the repair package: {error}"))
            })?;
            write_atomic(&dir.join(RECEIPT_NAME), &bytes, None)?;
        } else {
            remove_if_exists(&dir.join(RECEIPT_NAME))?;
        }
        if let Some(text) = new_gameinfo {
            write_atomic(&paths.gameinfo, text.as_bytes(), gameinfo_permissions)?;
        }
        Ok(())
    };
    if let Err(error) = write() {
        // Put back the state from before this call, then report the cause.
        if let Some(backup) = &backup {
            match restore_backup_set(backup) {
                Ok(_) => {
                    if let Err(cleanup) = fs::remove_dir_all(backup) {
                        log::warn!("Could not remove the rolled-back backup: {cleanup}");
                    }
                }
                Err(rollback) => log::error!(
                    "Repair failed ({error}) and rolling back failed ({rollback}); the previous files are in {}",
                    backup.display()
                ),
            }
        }
        return Err(error);
    }

    Ok(FixReport {
        status: FixModsStatus::Applied,
        fixed,
        skipped,
        overlay: result.is_some().then_some(overlay),
        gameinfo_changed,
        backup,
        packed_files,
        notes,
    })
}

struct DirGuard {
    dir: PathBuf,
    existed: bool,
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        if !self.existed {
            // Fails while the folder holds anything, which is what we want.
            let _ = fs::remove_dir(&self.dir);
        }
    }
}

/// What [`restore_last`] put back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// The backup that was restored, renamed with a `restored-` prefix so the
    /// next undo goes one repair further back.
    pub backup: PathBuf,
    pub restored: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

/// Whether [`restore_last`] has a repair to undo (a backup set not yet restored).
pub fn can_undo(paths: &GamePaths) -> bool {
    latest_backup_set(&backup_dir(paths)).is_ok_and(|backup| backup.is_some())
}

/// Undo the last [`apply`]: puts back the package, receipt, learned data and
/// `gameinfo.gi` exactly as they were before it, and removes files it created.
/// Refuses while the game runs.
pub fn restore_last(paths: &GamePaths) -> Result<RestoreReport, RepairError> {
    restore_last_with(paths, game::is_running)
}

/// [`restore_last`] with an injectable running-game check.
pub fn restore_last_with(
    paths: &GamePaths,
    game_running: impl Fn() -> bool,
) -> Result<RestoreReport, RepairError> {
    if game_running() {
        return Err(RepairError::GameRunning);
    }
    let root = backup_dir(paths);
    let Some(backup) = latest_backup_set(&root)? else {
        return Err(RepairError::BackupRestoreFailed(
            "There is no repair to undo".to_owned(),
        ));
    };
    let outcome = restore_backup_set(&backup)?;
    let consumed = root.join(format!("restored-{}", file_name(&backup)));
    let backup = match fs::rename(&backup, &consumed) {
        Ok(()) => consumed,
        Err(error) => {
            // Restoring the same set again is harmless: matching files are skipped.
            log::warn!("Could not mark {} as restored: {error}", backup.display());
            backup
        }
    };
    Ok(RestoreReport {
        backup,
        restored: outcome.restored,
        removed: outcome.removed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::localization_overlay::tests::compiled_resource;
    use shiv_core::sidecar::{self, Sidecar};
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::time::SystemTime;
    use vpkmanager::pack_directory;
    use vpkmanager::source2::kv3::Value as Kv3Value;

    const HEROES: &str = "scripts/heroes.vdata_c";
    const GAMEINFO: &str = "\"GameInfo\"\n{\n\tgame \"Deadlock\"\n\tFileSystem\n\t{\n\t\tSearchPaths\n\t\t{\n\t\t\tGame                citadel/addons\n\t\t\tMod                 citadel\n\t\t\tWrite               citadel\n\t\t\tGame                citadel\n\t\t\tMod                 core\n\t\t\tWrite               core\n\t\t\tGame                core\n\t\t}\n\t}\n}\n";

    struct Fixture {
        _temp: tempfile::TempDir,
        paths: GamePaths,
    }

    impl Fixture {
        fn citadel(&self) -> PathBuf {
            self.paths.root.join("game").join("citadel")
        }
        fn overlay(&self) -> PathBuf {
            self.citadel().join("shiv_repair").join("pak01_dir.vpk")
        }
    }

    fn hero(id: i64) -> Kv3Value {
        Kv3Value::Object(vec![("m_HeroID".to_string(), Kv3Value::Int(id))])
    }

    fn pack(scratch: &Path, rows: Vec<(String, Kv3Value)>, output: &Path) {
        let file = scratch.join(HEROES);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, compiled_resource(rows)).unwrap();
        pack_directory(scratch, output).unwrap();
    }

    /// A tiny install: the game ships two heroes, an installed mod carries an
    /// outdated copy of the table that lacks the newer hero. The engine's
    /// `RestoredGameData` repair puts the missing row back.
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(temp.path().join("Deadlock"));
        let citadel = paths.root.join("game").join("citadel");
        fs::create_dir_all(&paths.addons).unwrap();
        fs::write(citadel.join("steam.inf"), "ClientVersion=100\n").unwrap();
        fs::write(&paths.gameinfo, GAMEINFO).unwrap();
        pack(
            &temp.path().join("game-src"),
            vec![("hero_a".into(), hero(1)), ("hero_new".into(), hero(2))],
            &citadel.join("pak01_dir.vpk"),
        );
        pack(
            &temp.path().join("mod-src"),
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
        Fixture { _temp: temp, paths }
    }

    /// Every file (bytes and mtime) and every folder under `root`, except
    /// paths under `skip`.
    fn snapshot_except(
        root: &Path,
        skip: Option<&Path>,
    ) -> BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>> {
        let mut out = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if skip.is_some_and(|skip| path.starts_with(skip)) {
                    continue;
                }
                if path.is_dir() {
                    out.insert(path.clone(), None);
                    pending.push(path);
                } else {
                    let modified = fs::metadata(&path).unwrap().modified().unwrap();
                    out.insert(path.clone(), Some((fs::read(&path).unwrap(), modified)));
                }
            }
        }
        out
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<(Vec<u8>, SystemTime)>> {
        snapshot_except(root, None)
    }

    fn only(report: &[ModHealth]) -> &ModHealth {
        assert_eq!(report.len(), 1, "{report:?}");
        &report[0]
    }

    #[test]
    fn preview_does_not_modify_files() {
        let fx = fixture();
        let before = snapshot(&fx.paths.root);
        let report = analyze(&fx.paths, 100);
        assert_eq!(snapshot(&fx.paths.root), before);

        let health = only(&report);
        assert_eq!(health.mod_id, "data");
        assert_eq!(health.name, "Data mod");
        assert_eq!(health.status, RepairStatus::Fixable, "{health:?}");
        assert!(
            health.changes.iter().any(|c| c.contains(HEROES)),
            "{:?}",
            health.changes
        );
    }

    #[test]
    fn apply_creates_backup_and_changes_file() {
        let fx = fixture();
        let gameinfo = fs::read(&fx.paths.gameinfo).unwrap();
        let mod_vpk = fs::read(fx.paths.addons.join("pak01_dir.vpk")).unwrap();

        let report = apply_with(&fx.paths, &["data".into()], || false).unwrap();
        assert_eq!(report.status, FixModsStatus::Applied);
        assert_eq!(report.fixed, ["data"]);
        assert_eq!(report.overlay.as_deref(), Some(fx.overlay().as_path()));
        assert!(report.gameinfo_changed);
        assert!(fx.overlay().is_file());
        let package = source2_model::vpk_extract::VpkArchive::open(&fx.overlay()).unwrap();
        assert!(package.list_entries().iter().any(|e| e == HEROES));

        // gameinfo.gi changed by one mount line, and its original is backed up.
        let edited = fs::read_to_string(&fx.paths.gameinfo).unwrap();
        assert!(crate::gameinfo::mounts_overlay(&edited).unwrap());
        let backup = report.backup.clone().unwrap();
        let saved = backup.join("files").join("gameinfo.gi");
        assert_eq!(fs::read(saved).unwrap(), gameinfo);
        // The mod itself is never edited.
        assert_eq!(
            fs::read(fx.paths.addons.join("pak01_dir.vpk")).unwrap(),
            mod_vpk
        );
        assert_eq!(only(&analyze(&fx.paths, 100)).status, RepairStatus::Fixed);

        // Applying again replaces the package; the previous one is backed up.
        let first = fs::read(fx.overlay()).unwrap();
        let again = apply_with(&fx.paths, &["data".into()], || false).unwrap();
        assert!(!again.gameinfo_changed);
        assert_eq!(
            fs::read(again.backup.unwrap().join("files").join("pak01_dir.vpk")).unwrap(),
            first
        );
        assert_eq!(only(&analyze(&fx.paths, 100)).status, RepairStatus::Fixed);
    }

    #[test]
    fn apply_refuses_while_the_game_runs() {
        let fx = fixture();
        let before = snapshot(&fx.paths.root);
        assert!(matches!(
            apply_with(&fx.paths, &["data".into()], || true),
            Err(RepairError::GameRunning)
        ));
        assert_eq!(snapshot(&fx.paths.root), before);
    }

    #[test]
    fn applying_no_mods_removes_the_package_after_backing_it_up() {
        let fx = fixture();
        apply_with(&fx.paths, &["data".into()], || false).unwrap();
        let package = fs::read(fx.overlay()).unwrap();
        let report = apply_with(&fx.paths, &[], || false).unwrap();
        assert!(report.fixed.is_empty());
        assert_eq!(report.overlay, None);
        assert!(!fx.overlay().exists());
        assert_eq!(
            fs::read(report.backup.unwrap().join("files").join("pak01_dir.vpk")).unwrap(),
            package
        );
        assert_eq!(only(&analyze(&fx.paths, 100)).status, RepairStatus::Fixable);
    }

    #[test]
    fn unknown_or_disabled_mods_are_skipped() {
        let fx = fixture();
        let report = apply_with(&fx.paths, &["missing".into()], || false).unwrap();
        assert!(report.fixed.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "missing");
        assert_eq!(report.backup, None);
        // Nothing was repaired, so nothing is created: no empty package folder.
        assert!(!fx.citadel().join("shiv_repair").exists());
        assert!(
            !fx.paths
                .addons
                .join(".shiv")
                .join("repair-backups")
                .exists()
        );
    }

    #[test]
    fn a_mod_with_nothing_to_repair_leaves_no_folder_behind() {
        let fx = fixture();
        // The game's own table: the mod matches the game, so nothing to fix.
        pack(
            &fx._temp.path().join("same-src"),
            vec![("hero_a".into(), hero(1)), ("hero_new".into(), hero(2))],
            &fx.paths.addons.join("pak01_dir.vpk"),
        );
        let before = snapshot(&fx.paths.root);
        let report = apply_with(&fx.paths, &["data".into()], || false).unwrap();
        assert!(report.fixed.is_empty(), "{report:?}");
        assert_eq!(
            report.skipped,
            [("data".to_owned(), "Nothing to repair".to_owned())]
        );
        assert_eq!(snapshot(&fx.paths.root), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_after_the_package_rolls_back_to_the_previous_state() {
        use std::os::unix::fs::PermissionsExt;
        let fx = fixture();
        apply_with(&fx.paths, &["data".into()], || false).unwrap();
        // The game update reverted gameinfo.gi, so the next apply must mount
        // the package again; make that last write fail.
        fs::write(&fx.paths.gameinfo, GAMEINFO).unwrap();
        let citadel = fx.citadel();
        let mode = fs::metadata(&citadel).unwrap().permissions().mode();
        fs::set_permissions(&citadel, fs::Permissions::from_mode(0o555)).unwrap();
        if tempfile::NamedTempFile::new_in(&citadel).is_ok() {
            fs::set_permissions(&citadel, fs::Permissions::from_mode(mode)).unwrap();
            eprintln!("skipped: permissions are not enforced (running as root?)");
            return;
        }
        // A package on disk that the new one cannot match, so the rollback
        // must really put the old bytes back over the new package.
        fs::write(fx.overlay(), b"previous package").unwrap();
        let before = snapshot(&fx.paths.root);
        let previous = fs::read(fx.overlay()).unwrap();
        let result = apply_with(&fx.paths, &["data".into()], || false);
        let after = snapshot(&fx.paths.root);
        fs::set_permissions(&citadel, fs::Permissions::from_mode(mode)).unwrap();
        assert!(matches!(result, Err(RepairError::Io(_))), "{result:?}");
        assert_eq!(after, before);
        assert_eq!(fs::read(fx.overlay()).unwrap(), previous);
    }

    #[test]
    fn restore_last_undoes_an_apply_exactly() {
        let fx = fixture();
        let backups = fx.paths.addons.join(".shiv").join("repair-backups");
        let before = snapshot(&fx.paths.root);
        assert!(!can_undo(&fx.paths));
        apply_with(&fx.paths, &["data".into()], || false).unwrap();
        assert_ne!(snapshot_except(&fx.paths.root, Some(&backups)), before);
        assert!(can_undo(&fx.paths));

        assert!(matches!(
            restore_last_with(&fx.paths, || true),
            Err(RepairError::GameRunning)
        ));
        let report = restore_last_with(&fx.paths, || false).unwrap();
        assert!(report.restored.contains(&fx.paths.gameinfo), "{report:?}");
        assert!(report.removed.contains(&fx.overlay()), "{report:?}");
        assert_eq!(snapshot_except(&fx.paths.root, Some(&backups)), before);
        assert_eq!(only(&analyze(&fx.paths, 100)).status, RepairStatus::Fixable);

        // That repair is undone; there is nothing further back.
        assert!(!can_undo(&fx.paths));
        assert!(matches!(
            restore_last_with(&fx.paths, || false),
            Err(RepairError::BackupRestoreFailed(_))
        ));
    }

    #[test]
    fn restore_last_steps_back_one_apply_at_a_time() {
        let fx = fixture();
        apply_with(&fx.paths, &["data".into()], || false).unwrap();
        let backups = fx.paths.addons.join(".shiv").join("repair-backups");
        let after_first = snapshot_except(&fx.paths.root, Some(&backups));
        apply_with(&fx.paths, &[], || false).unwrap();
        assert!(!fx.overlay().exists());
        restore_last_with(&fx.paths, || false).unwrap();
        assert_eq!(snapshot_except(&fx.paths.root, Some(&backups)), after_first);
        assert_eq!(only(&analyze(&fx.paths, 100)).status, RepairStatus::Fixed);
    }
}
