//! Pre-launch health check: read-only inspection of the addons folder and
//! `gameinfo.gi`. Nothing here writes, renames or deletes; repair is separate.
//!
//! Search-path detection follows the old manager's `gameinfo_search_paths.rs`
//! (KeyValues sites under `GameInfo/FileSystem/SearchPaths`, conditions resolved
//! for the Windows game client) without its editing logic.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use kv_parser::{ParseOptions, SourceDocument, SourceKind};
use vpk_parser::{VpkError, VpkParser};

use crate::addons::{self, PakKind, parse_pak};
use crate::game::GamePaths;

const DISABLED_EXT: &str = ".disabled";
/// `archive_index` of an entry whose data follows the directory tree.
const INLINE_ARCHIVE: u16 = 0x7fff;
/// Suffixes download tools and our own atomic writes leave behind.
const PARTIAL_SUFFIXES: [&str; 6] = [
    ".part",
    ".partial",
    ".crdownload",
    ".download",
    ".tmp",
    ".temp",
];
/// The mount mods need: `Game citadel/addons`.
pub const ADDONS_SEARCH_PATH: &str = "citadel/addons";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingKind {
    /// Not a VPK the engine can read (bad signature, version, or directory).
    UnreadableVpk,
    /// The header or directory declares more data than the file (or a chunk) holds.
    TruncatedVpk,
    /// An empty `.vpk` file.
    ZeroByte,
    /// A leftover download or staging file.
    PartialDownload,
    /// Two entries claim the same pak number.
    DuplicatePak,
    /// `gameinfo.gi` does not mount `citadel/addons`, so mods will not load.
    MissingSearchPath,
    /// `gameinfo.gi` is missing, unreadable or read-only.
    GameinfoUnwritable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub file: Option<PathBuf>,
    pub detail: String,
}

impl Finding {
    fn new(kind: FindingKind, file: &Path, detail: impl Into<String>) -> Self {
        Self {
            kind,
            file: Some(file.to_path_buf()),
            detail: detail.into(),
        }
    }
}

/// Inspect the install. Never fails and never modifies anything: problems that
/// stop the inspection itself (an unreadable folder) become findings too.
pub fn check(paths: &GamePaths) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_addons(&paths.addons, &mut findings);
    check_gameinfo(&paths.gameinfo, &mut findings);
    findings
}

// ---------------------------------------------------------------------------
// Addons

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_partial(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    PARTIAL_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

fn check_addons(addons_dir: &Path, findings: &mut Vec<Finding>) {
    let vpks = match addons::list(addons_dir) {
        Ok(v) => v,
        Err(e) => {
            findings.push(Finding::new(
                FindingKind::UnreadableVpk,
                addons_dir,
                format!("The addons folder could not be read: {e}"),
            ));
            return;
        }
    };

    let mut by_pak: BTreeMap<u32, Vec<&Path>> = BTreeMap::new();
    for vpk in &vpks {
        if let Some(n) = vpk.pak {
            by_pak.entry(n).or_default().push(&vpk.file);
        }
        check_vpk(&vpk.file, findings);
        for chunk in &vpk.chunks {
            check_data_file(chunk, findings);
        }
    }
    for (n, files) in by_pak {
        if let [first, rest @ ..] = files.as_slice()
            && !rest.is_empty()
        {
            let names: Vec<String> = files.iter().map(|f| name_of(f)).collect();
            findings.push(Finding::new(
                FindingKind::DuplicatePak,
                first,
                format!("pak{n:02} is claimed by {}", names.join(" and ")),
            ));
        }
    }

    check_partials(addons_dir, findings);
}

/// Leftover download files in `addons` and `addons/.shiv`, and anything in the
/// installer's staging directory (`addons/.shiv/tmp`).
fn check_partials(addons_dir: &Path, findings: &mut Vec<Finding>) {
    let shiv = addons_dir.join(".shiv");
    for dir in [addons_dir, shiv.as_path()] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .filter(|e| is_partial(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        found.sort();
        for path in found {
            findings.push(Finding::new(
                FindingKind::PartialDownload,
                &path,
                "Leftover from an interrupted download or write",
            ));
        }
    }
    if let Ok(entries) = fs::read_dir(shiv.join("tmp")) {
        let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        found.sort();
        for path in found {
            findings.push(Finding::new(
                FindingKind::PartialDownload,
                &path,
                "Leftover staging from an interrupted install",
            ));
        }
    }
}

/// Size of a non-empty file, or a finding explaining why there is none.
fn file_len(path: &Path, findings: &mut Vec<Finding>) -> Option<u64> {
    match fs::metadata(path) {
        Ok(m) if m.len() == 0 => {
            findings.push(Finding::new(
                FindingKind::ZeroByte,
                path,
                "The file is empty",
            ));
            None
        }
        Ok(m) => Some(m.len()),
        Err(e) => {
            findings.push(Finding::new(
                FindingKind::UnreadableVpk,
                path,
                format!("The file could not be read: {e}"),
            ));
            None
        }
    }
}

/// Chunk files hold raw data without a header; only their size can be checked.
fn check_data_file(path: &Path, findings: &mut Vec<Finding>) {
    let _ = file_len(path, findings);
}

/// `pakNN_<digits>.vpk[.disabled]`.
fn is_chunk(path: &Path) -> bool {
    let name = name_of(path);
    let stem = name.strip_suffix(DISABLED_EXT).unwrap_or(&name);
    matches!(parse_pak(stem), Some((_, PakKind::Chunk(_))))
}

fn check_vpk(path: &Path, findings: &mut Vec<Finding>) {
    // A chunk without its dir VPK is listed on its own; it has no header.
    if is_chunk(path) {
        return check_data_file(path, findings);
    }
    let Some(len) = file_len(path, findings) else {
        return;
    };
    let truncated = |detail: String| Finding::new(FindingKind::TruncatedVpk, path, detail);
    let unreadable = |e: &VpkError| {
        Finding::new(
            FindingKind::UnreadableVpk,
            path,
            format!("Not a readable VPK: {e}"),
        )
    };

    let header = match VpkParser::header_from_file(path) {
        Ok(h) => h,
        // A valid signature followed by a short header: the file was cut off.
        Err(VpkError::BufferTooSmall { actual, .. }) if actual >= 4 => {
            return findings.push(truncated(format!(
                "The header is cut off after {len} bytes"
            )));
        }
        Err(e) => return findings.push(unreadable(&e)),
    };
    let data_start = VpkParser::header_size(header.version) + u64::from(header.tree_length);
    let declared = data_start
        + [
            header.file_data_section_size,
            header.archive_md5_section_size,
            header.other_md5_section_size,
            header.signature_section_size,
        ]
        .into_iter()
        .flatten()
        .map(u64::from)
        .sum::<u64>();
    if declared > len {
        return findings.push(truncated(format!(
            "The header declares {declared} bytes but the file has {len}"
        )));
    }
    let entries = match VpkParser::parse_directory_from_file(path) {
        Ok(e) => e,
        Err(e) => return findings.push(unreadable(&e)),
    };

    let mut chunk_needs: BTreeMap<u16, u64> = BTreeMap::new();
    for e in &entries {
        let end = u64::from(e.entry_offset) + u64::from(e.entry_length);
        if e.archive_index == INLINE_ARCHIVE {
            if data_start + end > len {
                return findings.push(truncated(format!(
                    "{} ends past the end of the file",
                    e.full_path
                )));
            }
        } else if e.entry_length > 0 {
            let need = chunk_needs.entry(e.archive_index).or_default();
            *need = (*need).max(end);
        }
    }
    for (index, need) in chunk_needs {
        check_chunk(path, index, need, findings);
    }
}

/// `pakNN_dir.vpk[.disabled]` -> its chunk `pakNN_<index>.vpk`, in either form.
fn check_chunk(dir_vpk: &Path, index: u16, need: u64, findings: &mut Vec<Finding>) {
    let name = name_of(dir_vpk);
    let stem = name.strip_suffix(DISABLED_EXT).unwrap_or(&name);
    let Some(prefix) = stem.strip_suffix("_dir.vpk") else {
        // Not a dir VPK name, so its chunks cannot be located.
        return;
    };
    let chunk = format!("{prefix}_{index:03}.vpk");
    let parent = dir_vpk.parent().unwrap_or(Path::new(""));
    let candidates = [
        parent.join(&chunk),
        parent.join(format!("{chunk}{DISABLED_EXT}")),
    ];
    let Some(found) = candidates.iter().find(|p| p.is_file()) else {
        findings.push(Finding::new(
            FindingKind::TruncatedVpk,
            dir_vpk,
            format!("The chunk file {chunk} is missing"),
        ));
        return;
    };
    match fs::metadata(found) {
        // An empty chunk is already reported as ZeroByte.
        Ok(m) if m.len() == 0 => {}
        Ok(m) if m.len() < need => findings.push(Finding::new(
            FindingKind::TruncatedVpk,
            found,
            format!(
                "{} declares {need} bytes in this chunk but it has {}",
                name_of(dir_vpk),
                m.len()
            ),
        )),
        Ok(_) => {}
        Err(e) => findings.push(Finding::new(
            FindingKind::UnreadableVpk,
            found,
            format!("The file could not be read: {e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// gameinfo.gi

/// Conditions as the Windows game client resolves them (Deadlock runs under
/// Proton on Linux, so the host OS is irrelevant).
pub fn client_symbol(name: &str) -> Option<bool> {
    match name.to_ascii_uppercase().as_str() {
        "WIN32" | "WIN64" | "WINDOWS" => Some(true),
        "LINUX" | "OSX" | "OSX64" | "POSIX" | "X360" | "PS3" | "GAMECONSOLE" => Some(false),
        _ => None,
    }
}

fn check_gameinfo(gameinfo: &Path, findings: &mut Vec<Finding>) {
    let bytes = match fs::read(gameinfo) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            findings.push(Finding::new(
                FindingKind::GameinfoUnwritable,
                gameinfo,
                "gameinfo.gi is missing; verify the game files in Steam",
            ));
            return;
        }
        Err(e) => {
            findings.push(Finding::new(
                FindingKind::GameinfoUnwritable,
                gameinfo,
                format!("gameinfo.gi could not be read: {e}"),
            ));
            return;
        }
    };
    // Opening for writing without create, truncate or append changes nothing on
    // disk; it is the reliable test (ownership, ACLs, read-only mounts).
    if let Err(e) = OpenOptions::new().write(true).open(gameinfo) {
        findings.push(Finding::new(
            FindingKind::GameinfoUnwritable,
            gameinfo,
            format!("gameinfo.gi is not writable: {e}"),
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    match mounts_addons(&text) {
        Ok(true) => {}
        Ok(false) => findings.push(Finding::new(
            FindingKind::MissingSearchPath,
            gameinfo,
            format!("SearchPaths has no active `Game {ADDONS_SEARCH_PATH}` entry"),
        )),
        Err(e) => findings.push(Finding::new(
            FindingKind::MissingSearchPath,
            gameinfo,
            format!("gameinfo.gi could not be parsed, so the addons search path is unknown: {e}"),
        )),
    }
}

/// True when `GameInfo/FileSystem/SearchPaths` holds a `Game` (or `Game+...`)
/// entry for `citadel/addons` that applies to the Windows client.
fn mounts_addons(text: &str) -> Result<bool, kv_parser::KvError> {
    // Source 2 reads backslashes literally in gameinfo.gi.
    let doc = SourceDocument::parse(
        text,
        ParseOptions {
            allow_escape_sequences: false,
            ..ParseOptions::default()
        },
    )?;
    for site in &doc.sites {
        let [root, fs_key, sp, role] = site.path.as_slice() else {
            continue;
        };
        let SourceKind::Scalar { value, .. } = &site.kind else {
            continue;
        };
        let is_game = role.split('+').any(|r| r.eq_ignore_ascii_case("Game"));
        if root.eq_ignore_ascii_case("GameInfo")
            && fs_key.eq_ignore_ascii_case("FileSystem")
            && sp.eq_ignore_ascii_case("SearchPaths")
            && is_game
            && value
                .trim_end_matches('/')
                .eq_ignore_ascii_case(ADDONS_SEARCH_PATH)
            && site.applies(client_symbol)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_suffixes() {
        assert!(is_partial("pak01_dir.vpk.part"));
        assert!(is_partial("X.CRDOWNLOAD"));
        assert!(!is_partial("pak01_dir.vpk"));
        assert!(!is_partial("pak01_dir.vpk.disabled"));
    }

    #[test]
    fn search_path_roles_and_conditions() {
        let doc = |entry: &str| {
            format!("GameInfo {{ FileSystem {{ SearchPaths {{ {entry}\n Game core }} }} }}")
        };
        assert!(mounts_addons(&doc("Game citadel/addons")).unwrap());
        assert!(mounts_addons(&doc("\"Game+Mod\" \"citadel/addons\"")).unwrap());
        assert!(mounts_addons(&doc("Game citadel/addons [$WIN64]")).unwrap());
        assert!(!mounts_addons(&doc("Game citadel/addons [!$WIN64]")).unwrap());
        assert!(!mounts_addons(&doc("Mod citadel/addons")).unwrap());
        assert!(!mounts_addons(&doc("Game citadel/addons2")).unwrap());
    }
}
