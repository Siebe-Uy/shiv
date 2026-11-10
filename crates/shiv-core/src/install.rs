//! Install and uninstall mods from zip, rar and 7z archives.
//!
//! The archive is extracted (only `.vpk` entries) into a staging directory inside
//! `<addons>/.shiv/tmp`, which lives on the same filesystem as `addons` so the final
//! moves are renames. Every entry path is validated before anything is written, the
//! staged VPKs are renamed to free `pakNN_dir.vpk` slots, and the sidecar is written
//! last. Entry-path validation is adapted from the old manager's `archive_extractor.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::addons::{self, AddonsError, PakKind, parse_pak};
use crate::game::{self, GamePaths};
use crate::sidecar::{self, InstalledMod, Sidecar, SidecarError};

const DISABLED_EXT: &str = ".disabled";
const SHIV_DIR: &str = ".shiv";
const TMP_DIR: &str = "tmp";

/// Default cap on total extracted bytes (decompression-bomb guard).
pub const MAX_EXTRACTED_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Default cap on archive entries.
pub const MAX_ENTRIES: usize = 100_000;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("Deadlock is running; close the game before changing mods")]
    GameRunning,
    #[error("unsupported archive format (expected zip, rar or 7z)")]
    UnsupportedArchive,
    #[error("the archive contains no .vpk file")]
    NoVpk,
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
    #[error("the addons folder already holds 99 paks")]
    AddonsFull,
    #[error("archive expands beyond the extraction limit")]
    TooLarge,
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("rar error: {0}")]
    Rar(#[from] unrar::error::UnrarError),
    #[error("7z error: {0}")]
    SevenZ(#[from] sevenz_rust2::Error),
    #[error(transparent)]
    Addons(#[from] AddonsError),
    #[error(transparent)]
    Sidecar(#[from] SidecarError),
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> InstallError + '_ {
    move |source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct ModMeta {
    pub mod_id: String,
    pub name: String,
    pub version: Option<String>,
    pub thumbnail_url: Option<String>,
    /// Marked NSFW in the catalog (recorded in the sidecar).
    pub nsfw: bool,
}

/// Extraction limits; injectable so tests do not need huge archives.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_bytes: u64,
    pub max_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: MAX_EXTRACTED_BYTES,
            max_entries: MAX_ENTRIES,
        }
    }
}

pub fn install(archive: &Path, meta: ModMeta, paths: &GamePaths) -> Result<Sidecar, InstallError> {
    install_with(archive, meta, paths, game::is_running)
}

pub fn install_with(
    archive: &Path,
    meta: ModMeta,
    paths: &GamePaths,
    game_running: impl Fn() -> bool,
) -> Result<Sidecar, InstallError> {
    install_with_limits(archive, meta, paths, game_running, Limits::default())
}

pub fn install_with_limits(
    archive: &Path,
    meta: ModMeta,
    paths: &GamePaths,
    game_running: impl Fn() -> bool,
    limits: Limits,
) -> Result<Sidecar, InstallError> {
    // Checked first: nothing may be written while the game runs.
    if game_running() {
        return Err(InstallError::GameRunning);
    }
    let addons_dir = paths.addons.as_path();
    clean_stale_staging(addons_dir);
    let format = detect_format(archive)?;

    // Also validates the mod id before any file is written. A corrupt old sidecar
    // is treated as absent so it cannot block reinstalling forever.
    let old = match sidecar::read(addons_dir, &meta.mod_id) {
        Ok(s) => s,
        Err(SidecarError::Parse { .. }) => None,
        Err(e) => return Err(e.into()),
    };

    let staging = Staging::create(addons_dir)?;
    extract(format, archive, staging.path(), limits)?;
    let staged = collect_vpks(staging.path())?;
    if staged.is_empty() {
        return Err(InstallError::NoVpk);
    }

    let numbers = allocate_paks(addons_dir, staged.len())?;
    let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut new_names = Vec::with_capacity(staged.len());
    for (vpk, number) in staged.iter().zip(&numbers) {
        let dir_name = format!("pak{number:02}_dir.vpk");
        plan.push((vpk.file.clone(), addons_dir.join(&dir_name)));
        for (digits, chunk) in &vpk.chunks {
            plan.push((
                chunk.clone(),
                addons_dir.join(format!("pak{number:02}_{digits}.vpk")),
            ));
        }
        new_names.push(dir_name);
    }

    let mut moved: Vec<PathBuf> = Vec::new();
    let rollback = |moved: &[PathBuf]| {
        for p in moved {
            let _ = fs::remove_file(p);
        }
    };
    for (from, to) in &plan {
        if let Err(e) = move_into_place(from, to) {
            rollback(&moved);
            return Err(e);
        }
        moved.push(to.clone());
    }

    let new_sidecar = Sidecar {
        mod_id: meta.mod_id,
        name: meta.name,
        version: meta.version,
        thumbnail_url: meta.thumbnail_url,
        vpks: new_names,
        nsfw: Some(meta.nsfw),
    };
    if let Err(e) = sidecar::write(addons_dir, &new_sidecar) {
        rollback(&moved);
        return Err(e.into());
    }

    // The new mod is complete and recorded; only now drop the files of the old one.
    // Best effort: the new mod is fully installed, so leftovers must not fail the call.
    if let Some(old) = old {
        for name in &old.vpks {
            if !new_sidecar.vpks.contains(name) {
                let _ = remove_pak_files(addons_dir, name);
            }
        }
    }
    Ok(new_sidecar)
}

/// Remove all of a mod's VPK files (including disabled forms and chunk files when it
/// has a sidecar) and its sidecar. A mod without a sidecar loses only its own VPK files.
pub fn uninstall(m: &InstalledMod) -> Result<(), InstallError> {
    uninstall_with(m, game::is_running)
}

/// [`uninstall`] with an injectable running-game check; refuses before touching anything.
pub fn uninstall_with(
    m: &InstalledMod,
    game_running: impl Fn() -> bool,
) -> Result<(), InstallError> {
    if game_running() {
        return Err(InstallError::GameRunning);
    }
    let Some(sc) = &m.sidecar else {
        for vpk in &m.vpks {
            let name = vpk.file.file_name().and_then(|n| n.to_str());
            match (name, vpk.file.parent()) {
                (Some(n), Some(dir)) => {
                    remove_pak_files(dir, n.strip_suffix(DISABLED_EXT).unwrap_or(n))?;
                }
                _ => remove_if_exists(&vpk.file)?,
            }
        }
        return Ok(());
    };
    // Validates the id too, so it cannot be used to escape `.shiv`.
    let valid_id = !sc.mod_id.is_empty()
        && sc
            .mod_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !valid_id {
        return Err(SidecarError::InvalidModId(sc.mod_id.clone()).into());
    }
    let Some(dir) = m.vpks.first().and_then(|v| v.file.parent()) else {
        return Ok(());
    };
    // Sidecar content is untrusted: only plain `*.vpk` names inside `dir` are acted on.
    let mut names: BTreeSet<String> = sc
        .vpks
        .iter()
        .filter(|n| is_plain_vpk_name(n))
        .cloned()
        .collect();
    for vpk in &m.vpks {
        if let Some(n) = vpk.file.file_name().and_then(|n| n.to_str()) {
            names.insert(n.strip_suffix(DISABLED_EXT).unwrap_or(n).to_owned());
        }
    }
    for name in &names {
        remove_pak_files(dir, name)?;
    }
    remove_if_exists(&dir.join(SHIV_DIR).join(format!("{}.json", sc.mod_id)))
}

fn remove_if_exists(path: &Path) -> Result<(), InstallError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path)(e)),
    }
}

/// Remove `name` (enabled form), its `.disabled` form and, for `pakNN_dir.vpk`, the
/// `pakNN_<digits>.vpk[.disabled]` chunk files next to it.
fn remove_pak_files(dir: &Path, name: &str) -> Result<(), InstallError> {
    if !is_plain_vpk_name(name) {
        return Ok(());
    }
    remove_if_exists(&dir.join(name))?;
    remove_if_exists(&dir.join(format!("{name}{DISABLED_EXT}")))?;
    if let Some((number, PakKind::Dir)) = parse_pak(name) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io_err(dir)(e)),
        };
        for entry in entries {
            let entry = entry.map_err(io_err(dir))?;
            let file_name = entry.file_name();
            let Some(n) = file_name.to_str() else {
                continue;
            };
            let enabled = n.strip_suffix(DISABLED_EXT).unwrap_or(n);
            if matches!(parse_pak(enabled), Some((c, PakKind::Chunk(_))) if c == number) {
                remove_if_exists(&entry.path())?;
            }
        }
    }
    Ok(())
}

/// A single plain path component ending in `.vpk`: no separators, `..`, or absolute paths.
fn is_plain_vpk_name(name: &str) -> bool {
    let mut comps = Path::new(name).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(c)), None) if c == name
    ) && !name.contains('\\')
        && name.ends_with(".vpk")
}

/// Remove staging dirs left by a crashed run: owned by another process and over an hour old.
fn clean_stale_staging(addons_dir: &Path) {
    const MAX_AGE: Duration = Duration::from_secs(3600);
    let tmp_root = addons_dir.join(SHIV_DIR).join(TMP_DIR);
    let Ok(entries) = fs::read_dir(&tmp_root) else {
        return;
    };
    let me = std::process::id().to_string();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.split('-').next() == Some(me.as_str()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let old = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age > MAX_AGE);
        if meta.is_dir() && old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
    let _ = fs::remove_dir(&tmp_root);
    let _ = fs::remove_dir(addons_dir.join(SHIV_DIR));
}

// ---- pak names ----

/// `count` lowest free pak numbers, ascending. A number is taken when any file in
/// `addons` (dir, disabled or chunk) already carries it.
fn allocate_paks(addons_dir: &Path, count: usize) -> Result<Vec<u32>, InstallError> {
    match addons::next_pak(addons_dir) {
        Ok(_) => {}
        Err(AddonsError::Full { .. }) => return Err(InstallError::AddonsFull),
        Err(e) => return Err(e.into()),
    }
    let used: BTreeSet<u32> = addons::used_pak_numbers(addons_dir)?.into_iter().collect();
    let free: Vec<u32> = (1..=addons::PAK_CAPACITY)
        .filter(|n| !used.contains(n))
        .take(count)
        .collect();
    if free.len() < count {
        return Err(InstallError::AddonsFull);
    }
    Ok(free)
}

fn move_into_place(from: &Path, to: &Path) -> Result<(), InstallError> {
    let disabled = PathBuf::from(format!("{}{DISABLED_EXT}", to.display()));
    if to.exists() || disabled.exists() {
        return Err(io_err(to)(io::Error::from(io::ErrorKind::AlreadyExists)));
    }
    fs::rename(from, to).map_err(io_err(to))
}

// ---- staging ----

/// A unique directory under `<addons>/.shiv/tmp`, removed (with now-empty parents) on drop.
struct Staging {
    path: PathBuf,
    tmp_root: PathBuf,
    shiv_dir: PathBuf,
}

impl Staging {
    fn create(addons_dir: &Path) -> Result<Self, InstallError> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let shiv_dir = addons_dir.join(SHIV_DIR);
        let tmp_root = shiv_dir.join(TMP_DIR);
        let path = tmp_root.join(unique);
        let staging = Self {
            path,
            tmp_root,
            shiv_dir,
        };
        // Drop cleans up even when creation half-succeeds.
        fs::create_dir_all(&staging.path).map_err(io_err(&staging.path))?;
        Ok(staging)
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        // Fail harmlessly when other staging dirs or sidecars are still there.
        let _ = fs::remove_dir(&self.tmp_root);
        let _ = fs::remove_dir(&self.shiv_dir);
    }
}

// ---- archive detection and extraction ----

#[derive(Debug, Clone, Copy)]
enum Format {
    Zip,
    Rar,
    SevenZ,
}

fn detect_format(archive: &Path) -> Result<Format, InstallError> {
    let mut magic = [0u8; 6];
    let mut f = File::open(archive).map_err(io_err(archive))?;
    let mut filled = 0;
    while filled < magic.len() {
        match f.read(&mut magic[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_err(archive)(e)),
        }
    }
    let head = &magic[..filled];
    if head.starts_with(b"PK\x03\x04") {
        Ok(Format::Zip)
    } else if head.starts_with(b"Rar!\x1a\x07") {
        Ok(Format::Rar)
    } else if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Ok(Format::SevenZ)
    } else {
        Err(InstallError::UnsupportedArchive)
    }
}

/// Bytes still allowed to be extracted; `exceeded` records that the cap was the cause
/// of a read error so it can be reported as [`InstallError::TooLarge`].
struct Budget {
    remaining: u64,
    exceeded: bool,
}

impl Budget {
    fn charge(&mut self, bytes: u64) -> Result<(), InstallError> {
        match self.remaining.checked_sub(bytes) {
            Some(r) => {
                self.remaining = r;
                Ok(())
            }
            None => {
                self.exceeded = true;
                Err(InstallError::TooLarge)
            }
        }
    }
}

struct Budgeted<'a, R> {
    inner: R,
    budget: &'a mut Budget,
}

impl<R: Read> Read for Budgeted<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.budget.charge(n as u64).is_err() {
            return Err(io::Error::other("extraction size limit exceeded"));
        }
        Ok(n)
    }
}

/// Copy `reader` into a new file at `dest` (creating parents), counting the budget.
fn write_entry(reader: impl Read, dest: &Path, budget: &mut Budget) -> Result<(), InstallError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(io_err(parent))?;
    }
    let mut out = File::create(dest).map_err(io_err(dest))?;
    let mut limited = Budgeted {
        inner: reader,
        budget,
    };
    match io::copy(&mut limited, &mut out) {
        Ok(_) => Ok(()),
        Err(_) if limited.budget.exceeded => Err(InstallError::TooLarge),
        Err(e) => Err(io_err(dest)(e)),
    }
}

fn is_vpk_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".vpk")
}

fn starts_with_drive_letter(component: &str) -> bool {
    let b = component.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// Interpret an entry name as a relative path of plain names. Backslashes are
/// separators on every platform. Absolute paths, `..`, drive/prefix components
/// (anywhere) and empty names are rejected.
fn relative_entry_path(entry_name: &str, allow_empty: bool) -> Result<PathBuf, InstallError> {
    let unsafe_path = || InstallError::UnsafePath(entry_name.to_owned());
    let normalized = entry_name.replace('\\', "/");
    let mut relative = PathBuf::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) if starts_with_drive_letter(&part.to_string_lossy()) => {
                return Err(unsafe_path());
            }
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(unsafe_path());
            }
        }
    }
    if relative.as_os_str().is_empty() && !allow_empty {
        return Err(unsafe_path());
    }
    Ok(relative)
}

const S_IFMT: u32 = 0o170_000;
const S_IFLNK: u32 = 0o120_000;

fn extract(
    format: Format,
    archive: &Path,
    dest: &Path,
    limits: Limits,
) -> Result<(), InstallError> {
    let mut budget = Budget {
        remaining: limits.max_bytes,
        exceeded: false,
    };
    match format {
        Format::Zip => extract_zip(archive, dest, limits, &mut budget),
        Format::Rar => extract_rar(archive, dest, limits, &mut budget),
        Format::SevenZ => extract_7z(archive, dest, limits, &mut budget),
    }
}

/// Counts listed entries against the cap.
fn count_entry(seen: &mut usize, limits: Limits) -> Result<(), InstallError> {
    *seen += 1;
    if *seen > limits.max_entries {
        return Err(InstallError::TooLarge);
    }
    Ok(())
}

fn extract_zip(
    archive: &Path,
    dest: &Path,
    limits: Limits,
    budget: &mut Budget,
) -> Result<(), InstallError> {
    let file = File::open(archive).map_err(io_err(archive))?;
    let mut zip = zip::ZipArchive::new(file)?;

    // Pass 1: validate every entry and the declared sizes before writing anything.
    let mut seen = 0;
    let mut declared: u64 = 0;
    for i in 0..zip.len() {
        let entry = zip.by_index_raw(i)?;
        count_entry(&mut seen, limits)?;
        relative_entry_path(entry.name(), entry.is_dir())?;
        if entry.unix_mode().is_some_and(|m| m & S_IFMT == S_IFLNK) {
            return Err(InstallError::UnsafePath(format!(
                "symlink entry {}",
                entry.name()
            )));
        }
        if !entry.is_dir() && is_vpk_name(entry.name()) {
            declared = declared.saturating_add(entry.size());
        }
    }
    if declared > limits.max_bytes {
        return Err(InstallError::TooLarge);
    }

    // Pass 2: extract `.vpk` entries.
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        if entry.is_dir() || !is_vpk_name(entry.name()) {
            continue;
        }
        let dest_path = dest.join(relative_entry_path(entry.name(), false)?);
        write_entry(entry, &dest_path, budget)?;
    }
    Ok(())
}

fn extract_7z(
    archive: &Path,
    dest: &Path,
    limits: Limits,
    budget: &mut Budget,
) -> Result<(), InstallError> {
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())?;

    let mut seen = 0;
    let mut declared: u64 = 0;
    for entry in &reader.archive().files {
        count_entry(&mut seen, limits)?;
        relative_entry_path(entry.name(), entry.is_directory())?;
        let attrs = entry.windows_attributes();
        let reparse = attrs & 0x400 != 0;
        let unix_symlink = attrs & 0x8000 != 0 && (attrs >> 16) & S_IFMT == S_IFLNK;
        if entry.has_windows_attributes && (reparse || unix_symlink) {
            return Err(InstallError::UnsafePath(format!(
                "symlink entry {}",
                entry.name()
            )));
        }
        if !entry.is_directory() && is_vpk_name(entry.name()) {
            declared = declared.saturating_add(entry.size());
        }
    }
    if declared > limits.max_bytes {
        return Err(InstallError::TooLarge);
    }

    let mut failure: Option<InstallError> = None;
    let result = reader.for_each_entries(|entry, data| {
        if entry.is_directory() || entry.is_anti_item() || !is_vpk_name(entry.name()) {
            return Ok(true);
        }
        let outcome = relative_entry_path(entry.name(), false)
            .and_then(|rel| write_entry(data, &dest.join(rel), budget));
        match outcome {
            Ok(()) => Ok(true),
            Err(e) => {
                failure = Some(e);
                Err(sevenz_rust2::Error::Other("extraction aborted".into()))
            }
        }
    });
    if let Some(e) = failure {
        return Err(e);
    }
    result?;
    Ok(())
}

fn extract_rar(
    archive: &Path,
    dest: &Path,
    limits: Limits,
    budget: &mut Budget,
) -> Result<(), InstallError> {
    // Pass 1: list and validate.
    let mut seen = 0;
    let mut declared: u64 = 0;
    for header in unrar::Archive::new(archive).open_for_listing()? {
        let header = header?;
        let name = header.filename.to_string_lossy().into_owned();
        count_entry(&mut seen, limits)?;
        relative_entry_path(&name, header.is_directory())?;
        // Windows hosts store attribute flags (no file-type bits); 0x400 is the reparse point.
        let reparse = header.file_attr & S_IFMT == 0 && header.file_attr & 0x400 != 0;
        if header.file_attr & S_IFMT == S_IFLNK || reparse {
            return Err(InstallError::UnsafePath(format!("symlink entry {name}")));
        }
        if header.is_file() && is_vpk_name(&name) {
            declared = declared.saturating_add(header.unpacked_size);
        }
    }
    if declared > limits.max_bytes {
        return Err(InstallError::TooLarge);
    }

    // Pass 2: extract `.vpk` files to the paths validated above.
    let mut open = unrar::Archive::new(archive).open_for_processing()?;
    while let Some(header) = open.read_header()? {
        let name = header.entry().filename.to_string_lossy().into_owned();
        open = if header.entry().is_file() && is_vpk_name(&name) {
            let target = dest.join(relative_entry_path(&name, false)?);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(io_err(parent))?;
                ensure_no_symlink_chain(dest, parent)?;
            }
            let next = header.extract_to(&target)?;
            // Header metadata is untrusted: check what the library really wrote.
            let meta = fs::symlink_metadata(&target).map_err(io_err(&target))?;
            if !meta.file_type().is_file() {
                let _ = fs::remove_file(&target);
                return Err(InstallError::UnsafePath(format!(
                    "non-regular entry {name}"
                )));
            }
            budget.charge(meta.len())?;
            next
        } else {
            header.skip()?
        };
    }
    Ok(())
}

// ---- staged VPK discovery ----

struct StagedVpk {
    file: PathBuf,
    /// `pakNN` number it had in the archive, if it followed the naming.
    original: Option<u32>,
    sort_name: String,
    /// `(digits, path)` of `pakNN_<digits>.vpk` siblings that move with it.
    chunks: Vec<(String, PathBuf)>,
}

fn collect_vpks(root: &Path) -> Result<Vec<StagedVpk>, InstallError> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(io_err(&dir))? {
            let entry = entry.map_err(io_err(&dir))?;
            let kind = entry.file_type().map_err(io_err(&dir))?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
    }

    // Chunks by (directory, pak number).
    let mut chunks: BTreeMap<(PathBuf, u32), Vec<(String, PathBuf)>> = BTreeMap::new();
    let mut dirs: Vec<StagedVpk> = Vec::new();
    for file in files {
        let Some(name) = file.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
        match parse_pak(&name) {
            Some((num, PakKind::Chunk(digits))) => {
                chunks
                    .entry((parent, num))
                    .or_default()
                    .push((digits, file));
            }
            Some((num, PakKind::Dir)) => dirs.push(StagedVpk {
                original: Some(num),
                sort_name: name.to_ascii_lowercase(),
                file,
                chunks: Vec::new(),
            }),
            None if is_vpk_name(&name) => dirs.push(StagedVpk {
                original: None,
                sort_name: name.to_ascii_lowercase(),
                file,
                chunks: Vec::new(),
            }),
            None => {}
        }
    }
    for vpk in &mut dirs {
        if let (Some(num), Some(parent)) = (vpk.original, vpk.file.parent())
            && let Some(mut found) = chunks.remove(&(parent.to_path_buf(), num))
        {
            found.sort();
            vpk.chunks = found;
        }
    }
    dirs.sort_by(|a, b| {
        (a.original.unwrap_or(u32::MAX), &a.sort_name, &a.file).cmp(&(
            b.original.unwrap_or(u32::MAX),
            &b.sort_name,
            &b.file,
        ))
    });
    Ok(dirs)
}

/// No existing directory between `root` (exclusive) and `dir` (inclusive) may be a symlink.
fn ensure_no_symlink_chain(root: &Path, dir: &Path) -> Result<(), InstallError> {
    let rel = dir.strip_prefix(root).unwrap_or(dir);
    let mut cur = root.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(InstallError::UnsafePath(format!(
                    "symlinked directory {}",
                    cur.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&cur)(e)),
        }
    }
    Ok(())
}
