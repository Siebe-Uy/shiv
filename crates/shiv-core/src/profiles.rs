//! Mod profiles: separate addons folders you can switch between.
//!
//! Each profile owns a whole folder under `citadel/shiv_profiles/<id>/`, and
//! the game's `citadel/addons` is a symlink to the active one. The symlink is
//! the truth about which profile is active; `profiles.json` only holds the
//! display names. Everything lives inside `citadel/`, so moves are renames on
//! one filesystem and the game, health checks and installs follow the link
//! without knowing about profiles.
//!
//! Callers must not switch while Deadlock runs (the UI checks).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::game::GamePaths;

const DIR: &str = "shiv_profiles";
const INDEX: &str = "profiles.json";
pub const DEFAULT_ID: &str = "default";
const MAX_NAME: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("profiles need symlinks, which this platform does not support here")]
    Unsupported,
    #[error(
        "the addons folder is a link Shiv did not create ({0}); remove it or move its target before using profiles"
    )]
    ForeignLink(PathBuf),
    #[error("the profile name {0:?} is not usable: use 1 to 40 visible characters")]
    BadName(String),
    #[error("a profile named {0:?} already exists")]
    NameTaken(String),
    #[error("there is no such profile")]
    Unknown,
    #[error("the active profile cannot be deleted; switch to another first")]
    DeleteActive,
    #[error("file error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profiles {
    pub active: String,
    pub profiles: Vec<Profile>,
}

#[derive(Serialize, Deserialize, Default)]
struct Index {
    #[serde(default)]
    profiles: Vec<Profile>,
}

/// Whether this platform can use profiles.
pub const fn supported() -> bool {
    cfg!(unix)
}

fn root(paths: &GamePaths) -> PathBuf {
    paths.addons.with_file_name(DIR)
}

fn folder(paths: &GamePaths, id: &str) -> PathBuf {
    root(paths).join(id)
}

fn read_index(paths: &GamePaths) -> Index {
    fs::read(root(paths).join(INDEX))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_index(paths: &GamePaths, index: &Index) -> Result<(), ProfileError> {
    let path = root(paths).join(INDEX);
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(index).map_err(io::Error::other)?,
    )?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(unix)]
fn link(target: &Path, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(not(unix))]
fn link(_target: &Path, _at: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// Point `addons` at `target`, atomically replacing a previous link.
fn point_addons(paths: &GamePaths, target: &Path) -> io::Result<()> {
    let tmp = paths.addons.with_file_name("addons.shiv-link");
    let _ = fs::remove_file(&tmp);
    link(target, &tmp)?;
    fs::rename(&tmp, &paths.addons).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// The profile id `addons` currently points at, when it is one of ours.
fn active_id(paths: &GamePaths) -> Option<String> {
    let target = fs::read_link(&paths.addons).ok()?;
    let target = if target.is_relative() {
        paths.addons.parent()?.join(target)
    } else {
        target
    };
    (target.parent()? == root(paths))
        .then(|| target.file_name()?.to_str().map(str::to_owned))
        .flatten()
}

fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if s.is_empty() { "profile".into() } else { s }
}

fn unique_id(paths: &GamePaths, name: &str) -> String {
    let base = slug(name);
    let mut id = base.clone();
    let mut n = 2;
    while folder(paths, &id).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

fn clean_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(ProfileError::BadName(name.to_owned()));
    }
    Ok(name.to_owned())
}

/// Whether profiles are turned on: `addons` is a link to one of ours.
pub fn is_set_up(paths: &GamePaths) -> bool {
    supported() && active_id(paths).is_some()
}

/// How many mod packages (`*_dir.vpk`, enabled or not) a profile holds.
pub fn mod_count(paths: &GamePaths, id: &str) -> usize {
    fs::read_dir(folder(paths, id)).map_or(0, |entries| {
        entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                name.ends_with("_dir.vpk") || name.ends_with("_dir.vpk.disabled")
            })
            .count()
    })
}

/// The profiles, setting profiles up on first use: a real `addons` folder
/// becomes the "Default" profile and `addons` turns into a link to it.
pub fn load(paths: &GamePaths) -> Result<Profiles, ProfileError> {
    if !supported() {
        return Err(ProfileError::Unsupported);
    }
    fs::create_dir_all(root(paths))?;
    let mut index = read_index(paths);
    let meta = fs::symlink_metadata(&paths.addons);
    let mut changed = false;

    match meta {
        Ok(m) if m.file_type().is_symlink() => {
            if active_id(paths).is_none() {
                return Err(ProfileError::ForeignLink(paths.addons.clone()));
            }
        }
        Ok(m) if m.is_dir() => {
            // Mods that were in a plain addons folder: the first time they are
            // the Default profile; later (Steam recreated the folder, a manual
            // install) they are kept as a profile of their own, not lost.
            let (id, name) = if index.profiles.is_empty() {
                (DEFAULT_ID.to_owned(), "Default".to_owned())
            } else {
                (unique_id(paths, "Recovered"), "Recovered".to_owned())
            };
            let dest = folder(paths, &id);
            fs::rename(&paths.addons, &dest)?;
            index.profiles.push(Profile {
                id: id.clone(),
                name,
            });
            changed = true;
            if index.profiles.len() == 1 {
                point_addons(paths, &dest)?;
            } else {
                // Keep using the profile that was active before.
                let previous = index.profiles.iter().find(|p| p.id != id).cloned();
                let keep = previous.map_or(dest, |p| folder(paths, &p.id));
                point_addons(paths, &keep)?;
            }
        }
        Ok(_) => return Err(ProfileError::ForeignLink(paths.addons.clone())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let id = index
                .profiles
                .first()
                .map_or(DEFAULT_ID, |p| p.id.as_str())
                .to_owned();
            let dir = folder(paths, &id);
            fs::create_dir_all(&dir)?;
            if !index.profiles.iter().any(|p| p.id == id) {
                index.profiles.push(Profile {
                    id,
                    name: "Default".into(),
                });
                changed = true;
            }
            point_addons(paths, &dir)?;
        }
        Err(e) => return Err(e.into()),
    }

    // Folders without a name entry (index lost or edited) get one; entries
    // whose folder is gone are dropped.
    let before = index.profiles.len();
    index.profiles.retain(|p| folder(paths, &p.id).is_dir());
    changed |= index.profiles.len() != before;
    for entry in fs::read_dir(root(paths))?.flatten() {
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if entry.path().is_dir() && !index.profiles.iter().any(|p| p.id == id) {
            index.profiles.push(Profile {
                name: id.clone(),
                id,
            });
            changed = true;
        }
    }
    if changed {
        write_index(paths, &index)?;
    }
    let active = active_id(paths).ok_or_else(|| ProfileError::ForeignLink(paths.addons.clone()))?;
    Ok(Profiles {
        active,
        profiles: index.profiles,
    })
}

fn find<'a>(profiles: &'a Profiles, id: &str) -> Result<&'a Profile, ProfileError> {
    profiles
        .profiles
        .iter()
        .find(|p| p.id == id)
        .ok_or(ProfileError::Unknown)
}

fn check_free(profiles: &Profiles, name: &str, except: Option<&str>) -> Result<(), ProfileError> {
    let taken = profiles
        .profiles
        .iter()
        .any(|p| Some(p.id.as_str()) != except && p.name.eq_ignore_ascii_case(name));
    if taken {
        Err(ProfileError::NameTaken(name.to_owned()))
    } else {
        Ok(())
    }
}

/// A new, empty profile.
pub fn create(paths: &GamePaths, name: &str) -> Result<Profile, ProfileError> {
    let name = clean_name(name)?;
    let profiles = load(paths)?;
    check_free(&profiles, &name, None)?;
    let id = unique_id(paths, &name);
    fs::create_dir_all(folder(paths, &id))?;
    add(paths, Profile { id, name })
}

fn add(paths: &GamePaths, profile: Profile) -> Result<Profile, ProfileError> {
    let mut index = read_index(paths);
    index.profiles.push(profile.clone());
    write_index(paths, &index)?;
    Ok(profile)
}

/// A new profile holding a copy of `id`'s mods. Files are copied (on
/// copy-on-write filesystems this costs no extra space) and never linked,
/// because repairs rewrite VPKs in place.
pub fn duplicate(paths: &GamePaths, id: &str, name: &str) -> Result<Profile, ProfileError> {
    let name = clean_name(name)?;
    let profiles = load(paths)?;
    find(&profiles, id)?;
    check_free(&profiles, &name, None)?;
    let new_id = unique_id(paths, &name);
    let dest = folder(paths, &new_id);
    if let Err(e) = copy_tree(&folder(paths, id), &dest) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e.into());
    }
    add(paths, Profile { id: new_id, name })
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn rename(paths: &GamePaths, id: &str, name: &str) -> Result<(), ProfileError> {
    let name = clean_name(name)?;
    let profiles = load(paths)?;
    find(&profiles, id)?;
    check_free(&profiles, &name, Some(id))?;
    let mut index = read_index(paths);
    if let Some(p) = index.profiles.iter_mut().find(|p| p.id == id) {
        p.name = name;
    }
    write_index(paths, &index)
}

/// Delete a profile and its mods. The active profile cannot be deleted.
pub fn delete(paths: &GamePaths, id: &str) -> Result<(), ProfileError> {
    let profiles = load(paths)?;
    find(&profiles, id)?;
    if profiles.active == id {
        return Err(ProfileError::DeleteActive);
    }
    fs::remove_dir_all(folder(paths, id))?;
    let mut index = read_index(paths);
    index.profiles.retain(|p| p.id != id);
    write_index(paths, &index)
}

/// Make `id` the active profile. The game must not be running.
pub fn switch(paths: &GamePaths, id: &str) -> Result<(), ProfileError> {
    let profiles = load(paths)?;
    find(&profiles, id)?;
    if profiles.active == id {
        return Ok(());
    }
    point_addons(paths, &folder(paths, id))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn game() -> (tempfile::TempDir, GamePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        fs::create_dir_all(paths.addons.parent().unwrap()).unwrap();
        (dir, paths)
    }

    fn touch(dir: &Path, name: &str) {
        fs::write(dir.join(name), name).unwrap();
    }

    #[test]
    fn a_plain_addons_folder_becomes_the_default_profile() {
        let (_d, paths) = game();
        fs::create_dir(&paths.addons).unwrap();
        touch(&paths.addons, "pak01_dir.vpk");
        let p = load(&paths).unwrap();
        assert_eq!(p.active, "default");
        assert_eq!(
            p.profiles,
            [Profile {
                id: "default".into(),
                name: "Default".into()
            }]
        );
        assert!(
            fs::symlink_metadata(&paths.addons)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(paths.addons.join("pak01_dir.vpk").is_file());
        assert_eq!(load(&paths).unwrap(), p, "loading again changes nothing");
    }

    #[test]
    fn a_missing_addons_folder_gets_an_empty_default() {
        let (_d, paths) = game();
        assert_eq!(load(&paths).unwrap().active, "default");
        assert!(paths.addons.is_dir());
    }

    #[test]
    fn create_switch_and_isolation() {
        let (_d, paths) = game();
        load(&paths).unwrap();
        touch(&paths.addons, "pak01_dir.vpk");
        let sweaty = create(&paths, "Sweaty Setup").unwrap();
        assert_eq!(sweaty.id, "sweaty-setup");
        switch(&paths, &sweaty.id).unwrap();
        assert!(
            !paths.addons.join("pak01_dir.vpk").exists(),
            "new profile is empty"
        );
        touch(&paths.addons, "pak02_dir.vpk");
        assert_eq!(load(&paths).unwrap().active, "sweaty-setup");
        switch(&paths, "default").unwrap();
        assert!(paths.addons.join("pak01_dir.vpk").is_file());
        assert!(!paths.addons.join("pak02_dir.vpk").exists());
    }

    #[test]
    fn duplicate_copies_files_independently() {
        let (_d, paths) = game();
        load(&paths).unwrap();
        fs::create_dir(paths.addons.join(".shiv")).unwrap();
        touch(&paths.addons, "pak01_dir.vpk");
        touch(&paths.addons.join(".shiv"), "m.json");
        let copy = duplicate(&paths, "default", "Copy").unwrap();
        let copied = folder(&paths, &copy.id);
        assert!(copied.join(".shiv/m.json").is_file());
        fs::write(copied.join("pak01_dir.vpk"), "changed").unwrap();
        assert_eq!(
            fs::read_to_string(paths.addons.join("pak01_dir.vpk")).unwrap(),
            "pak01_dir.vpk"
        );
    }

    #[test]
    fn names_are_validated_and_unique() {
        let (_d, paths) = game();
        load(&paths).unwrap();
        assert!(matches!(
            create(&paths, "  "),
            Err(ProfileError::BadName(_))
        ));
        assert!(matches!(
            create(&paths, &"x".repeat(41)),
            Err(ProfileError::BadName(_))
        ));
        assert!(matches!(
            create(&paths, "default"),
            Err(ProfileError::NameTaken(_))
        ));
        create(&paths, "A/B").unwrap();
        create(&paths, "a b").unwrap(); // slug clash gets a numbered folder
        let ids: Vec<_> = load(&paths)
            .unwrap()
            .profiles
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["default", "a-b", "a-b-2"]);
    }

    #[test]
    fn rename_and_delete() {
        let (_d, paths) = game();
        load(&paths).unwrap();
        let other = create(&paths, "Other").unwrap();
        rename(&paths, &other.id, "Renamed").unwrap();
        assert!(matches!(
            rename(&paths, &other.id, "default"),
            Err(ProfileError::NameTaken(_))
        ));
        assert!(matches!(
            delete(&paths, "default"),
            Err(ProfileError::DeleteActive)
        ));
        delete(&paths, &other.id).unwrap();
        assert!(!folder(&paths, &other.id).exists());
        assert!(matches!(delete(&paths, "nope"), Err(ProfileError::Unknown)));
    }

    #[test]
    fn a_folder_recreated_by_steam_is_kept_as_its_own_profile() {
        let (_d, paths) = game();
        load(&paths).unwrap();
        touch(&paths.addons, "mine.vpk");
        let other = create(&paths, "Other").unwrap();
        switch(&paths, &other.id).unwrap();
        // Something replaces the link with a real folder.
        fs::remove_file(&paths.addons).unwrap();
        fs::create_dir(&paths.addons).unwrap();
        touch(&paths.addons, "stray.vpk");
        let p = load(&paths).unwrap();
        assert!(p.profiles.iter().any(|x| x.name == "Recovered"));
        assert!(folder(&paths, "recovered").join("stray.vpk").is_file());
        assert!(
            fs::symlink_metadata(&paths.addons)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_link_we_did_not_make_is_refused() {
        let (dir, paths) = game();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        link(&elsewhere, &paths.addons).unwrap();
        assert!(matches!(load(&paths), Err(ProfileError::ForeignLink(_))));
    }
}
