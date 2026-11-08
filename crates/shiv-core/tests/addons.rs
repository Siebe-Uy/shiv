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
    assert_eq!((list[0].pak, list[0].enabled), (1, true));
    assert_eq!((list[1].pak, list[1].enabled), (2, false));
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
    assert_eq!(off.pak, 3);
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
