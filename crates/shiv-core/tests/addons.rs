use std::fs;
use std::path::Path;

use shiv_core::addons::{self, AddonsError};

fn touch(dir: &Path, name: &str) {
    fs::write(dir.join(name), b"x").unwrap();
}

#[test]
fn lists_enabled_and_disabled() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak01_dir.vpk");
    touch(dir.path(), "pak02_dir.vpk.disabled");
    touch(dir.path(), "readme.txt");
    let mut list = addons::list(dir.path()).unwrap();
    list.sort_by_key(|v| v.pak);
    assert_eq!(list.len(), 2);
    assert_eq!((list[0].pak, list[0].enabled), (Some(1), true));
    assert_eq!((list[1].pak, list[1].enabled), (Some(2), false));
}

#[test]
fn disable_then_enable_roundtrips_filename() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak03_dir.vpk");
    let vpk = addons::list(dir.path()).unwrap().remove(0);
    addons::set_enabled(&vpk, false).unwrap();
    assert!(dir.path().join("pak03_dir.vpk.disabled").exists());
    assert!(!dir.path().join("pak03_dir.vpk").exists());
    let off = addons::list(dir.path()).unwrap().remove(0);
    assert!(!off.enabled);
    assert_eq!(off.pak, Some(3));
    addons::set_enabled(&off, true).unwrap();
    assert!(dir.path().join("pak03_dir.vpk").exists());
    assert!(!dir.path().join("pak03_dir.vpk.disabled").exists());
}

#[test]
fn next_pak_skips_used_numbers_including_disabled() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(addons::next_pak(dir.path()).unwrap(), 1);
    touch(dir.path(), "pak01_dir.vpk");
    touch(dir.path(), "pak02_dir.vpk.disabled");
    assert_eq!(addons::next_pak(dir.path()).unwrap(), 3);
    touch(dir.path(), "pak04_dir.vpk");
    // Lowest free number, so the gap at 3 is reused.
    assert_eq!(addons::next_pak(dir.path()).unwrap(), 3);
}

#[test]
fn next_pak_reports_full_directory() {
    let dir = tempfile::tempdir().unwrap();
    for n in 1..=99 {
        touch(dir.path(), &format!("pak{n:02}_dir.vpk"));
    }
    assert!(matches!(
        addons::next_pak(dir.path()),
        Err(AddonsError::Full { .. })
    ));
}

#[test]
fn hand_added_vpk_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak07_dir.vpk");
    let list = addons::list(dir.path()).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].file, dir.path().join("pak07_dir.vpk"));
    assert!(list[0].enabled);
}

fn only(dir: &Path) -> addons::InstalledVpk {
    addons::list(dir).unwrap().remove(0)
}

#[test]
fn non_pak_vpk_is_listed_with_no_pak_number() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "123456_mod.vpk");
    let v = only(dir.path());
    assert_eq!((v.pak, v.enabled), (None, true));
    assert_eq!(addons::next_pak(dir.path()).unwrap(), 1);
}

#[test]
fn non_pak_disabled_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "123456_mod.vpk");
    addons::set_enabled(&only(dir.path()), false).unwrap();
    assert!(dir.path().join("123456_mod.vpk.disabled").exists());
    let off = only(dir.path());
    assert_eq!((off.pak, off.enabled), (None, false));
    addons::set_enabled(&off, true).unwrap();
    assert!(dir.path().join("123456_mod.vpk").exists());
}

#[test]
fn ignores_non_vpk_files_and_directories() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "readme.txt");
    fs::create_dir(dir.path().join(".shiv")).unwrap();
    fs::create_dir(dir.path().join("folder.vpk")).unwrap();
    assert!(addons::list(dir.path()).unwrap().is_empty());
}

#[test]
fn malformed_pak_names_are_hand_added() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak1_dir.vpk");
    touch(dir.path(), "pakXX_dir.vpk");
    let list = addons::list(dir.path()).unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|v| v.pak.is_none()));
}

#[test]
fn set_enabled_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak01_dir.vpk");
    let v = only(dir.path());
    addons::set_enabled(&v, true).unwrap();
    addons::set_enabled(&v, false).unwrap();
    addons::set_enabled(&only(dir.path()), false).unwrap();
    assert!(dir.path().join("pak01_dir.vpk.disabled").exists());
}

#[test]
fn set_enabled_reports_target_exists() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak01_dir.vpk");
    touch(dir.path(), "pak01_dir.vpk.disabled");
    let v = addons::InstalledVpk {
        file: dir.path().join("pak01_dir.vpk"),
        pak: Some(1),
        enabled: true,
    };
    assert!(matches!(
        addons::set_enabled(&v, false),
        Err(AddonsError::TargetExists { .. })
    ));
}

#[test]
fn set_enabled_with_stale_flag_still_correct() {
    let dir = tempfile::tempdir().unwrap();
    touch(dir.path(), "pak01_dir.vpk.disabled");
    // Caller believes it is enabled, but it is disabled on disk.
    let stale = addons::InstalledVpk {
        file: dir.path().join("pak01_dir.vpk"),
        pak: Some(1),
        enabled: true,
    };
    addons::set_enabled(&stale, true).unwrap();
    assert!(dir.path().join("pak01_dir.vpk").exists());
    assert!(!dir.path().join("pak01_dir.vpk.disabled").exists());
}
