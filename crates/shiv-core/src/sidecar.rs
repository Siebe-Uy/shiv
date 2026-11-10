//! Sidecar metadata linking installed VPKs to an API mod id and version.
//!
//! Files live in `<addons>/.shiv/<mod_id>.json`. `Sidecar::vpks` holds the
//! enabled-form file names (no `.disabled` suffix) so toggling a mod never
//! breaks the link.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use crate::addons::{self, AddonsError, InstalledVpk};

const DIR: &str = ".shiv";
const DISABLED_EXT: &str = ".disabled";

#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    #[error("invalid mod id {0:?}: only [A-Za-z0-9_-] allowed, non-empty")]
    InvalidModId(String),
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("corrupt sidecar {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Addons(#[from] AddonsError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    pub mod_id: String,
    pub name: String,
    pub version: Option<String>,
    pub thumbnail_url: Option<String>,
    /// VPK file names without the `.disabled` suffix.
    pub vpks: Vec<String>,
    /// Whether the catalog marked the mod NSFW when it was installed.
    /// `None` in sidecars written before this field existed (unknown); not
    /// written when `None`, so older Shiv builds read new sidecars too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsfw: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledMod {
    pub sidecar: Option<Sidecar>,
    pub vpks: Vec<InstalledVpk>,
}

fn validate(mod_id: &str) -> Result<(), SidecarError> {
    let ok = !mod_id.is_empty()
        && mod_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(SidecarError::InvalidModId(mod_id.to_owned()))
    }
}

fn path_for(addons: &Path, mod_id: &str) -> PathBuf {
    addons.join(DIR).join(format!("{mod_id}.json"))
}

fn read_file(path: &Path) -> Result<Option<Sidecar>, SidecarError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(SidecarError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| SidecarError::Parse {
            path: path.to_path_buf(),
            source,
        })
}

/// Read the sidecar for `mod_id`. Missing is `Ok(None)`; corrupt is an error.
pub fn read(addons: &Path, mod_id: &str) -> Result<Option<Sidecar>, SidecarError> {
    validate(mod_id)?;
    read_file(&path_for(addons, mod_id))
}

/// Write the sidecar atomically (temp file in the same directory, then rename).
pub fn write(addons: &Path, sidecar: &Sidecar) -> Result<(), SidecarError> {
    validate(&sidecar.mod_id)?;
    let dir = addons.join(DIR);
    let io_err = |path: &Path, source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    };
    fs::create_dir_all(&dir).map_err(|e| io_err(&dir, e))?;
    let json = serde_json::to_vec_pretty(sidecar).map_err(|source| SidecarError::Parse {
        path: dir.clone(),
        source,
    })?;
    let final_path = path_for(addons, &sidecar.mod_id);
    let tmp = dir.join(format!(".{}.json.tmp", sidecar.mod_id));
    fs::write(&tmp, json).map_err(|e| io_err(&tmp, e))?;
    fs::rename(&tmp, &final_path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_err(&final_path, e)
    })
}

fn enabled_name(vpk: &InstalledVpk) -> Option<String> {
    let name = vpk.file.file_name()?.to_str()?;
    Some(name.strip_suffix(DISABLED_EXT).unwrap_or(name).to_owned())
}

/// All sidecars in `<addons>/.shiv`; only `<valid-id>.json` files are considered
/// and corrupt ones are skipped.
fn read_all(addons: &Path) -> Vec<Sidecar> {
    let Ok(entries) = fs::read_dir(addons.join(DIR)) else {
        return Vec::new();
    };
    let mut out = BTreeMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if validate(id).is_err() || !path.is_file() {
            continue;
        }
        if let Ok(Some(s)) = read_file(&path)
            && s.mod_id == id
        {
            out.insert(s.mod_id.clone(), s);
        }
    }
    out.into_values().collect()
}

/// Group installed VPKs by sidecar. Sidecar mods come first (by mod id), then
/// unknown VPKs (by file name), each alone. A sidecar with no VPKs on disk is
/// omitted. Never fails: unreadable or corrupt sidecars leave their VPKs unknown.
pub fn group(addons: &Path) -> Vec<InstalledMod> {
    let Ok(vpks) = addons::list(addons) else {
        return Vec::new();
    };
    let mut remaining: Vec<(String, InstalledVpk)> = vpks
        .into_iter()
        .filter_map(|v| enabled_name(&v).map(|n| (n, v)))
        .collect();
    let mut out = Vec::new();
    for s in read_all(addons) {
        let mut mine = Vec::new();
        remaining.retain(|(name, v)| {
            if s.vpks.contains(name) {
                mine.push(v.clone());
                false
            } else {
                true
            }
        });
        if !mine.is_empty() {
            out.push(InstalledMod {
                sidecar: Some(s),
                vpks: mine,
            });
        }
    }
    remaining.sort_by(|a, b| a.0.cmp(&b.0));
    out.extend(remaining.into_iter().map(|(_, v)| InstalledMod {
        sidecar: None,
        vpks: vec![v],
    }));
    out
}
