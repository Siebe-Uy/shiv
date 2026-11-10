use shiv_core::sidecar::{self, Sidecar, SidecarError};
use std::fs;
use tempfile::tempdir;

fn sc(id: &str, vpks: &[&str]) -> Sidecar {
    Sidecar {
        mod_id: id.into(),
        name: format!("Mod {id}"),
        version: Some("1.0".into()),
        thumbnail_url: None,
        vpks: vpks.iter().map(|s| s.to_string()).collect(),
        nsfw: false,
    }
}

#[test]
fn write_then_read_roundtrips() {
    let d = tempdir().unwrap();
    let s = sc("abc-1", &["pak01_dir.vpk"]);
    sidecar::write(d.path(), &s).unwrap();
    assert_eq!(sidecar::read(d.path(), "abc-1").unwrap(), Some(s));
    assert_eq!(sidecar::read(d.path(), "nope").unwrap(), None);
}

#[test]
fn nsfw_flag_roundtrips_and_old_sidecars_read_as_not_nsfw() {
    let d = tempdir().unwrap();
    let mut s = sc("spicy", &["pak01_dir.vpk"]);
    s.nsfw = true;
    sidecar::write(d.path(), &s).unwrap();
    assert_eq!(sidecar::read(d.path(), "spicy").unwrap(), Some(s));
    // Written before the field existed.
    fs::write(
        d.path().join(".shiv/old.json"),
        r#"{"mod_id":"old","name":"Old","version":null,"thumbnail_url":null,"vpks":["pak02_dir.vpk"]}"#,
    )
    .unwrap();
    let old = sidecar::read(d.path(), "old").unwrap().unwrap();
    assert!(!old.nsfw);
    assert_eq!(old.vpks, ["pak02_dir.vpk"]);
}

#[test]
fn group_joins_vpks_to_their_mod() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("pak01_dir.vpk"), b"x").unwrap();
    fs::write(d.path().join("pak02_dir.vpk"), b"x").unwrap();
    sidecar::write(d.path(), &sc("m1", &["pak01_dir.vpk", "pak02_dir.vpk"])).unwrap();
    let g = sidecar::group(d.path());
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].sidecar.as_ref().unwrap().mod_id, "m1");
    assert_eq!(g[0].vpks.len(), 2);
}

#[test]
fn group_matches_disabled_vpks() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("pak03_dir.vpk.disabled"), b"x").unwrap();
    sidecar::write(d.path(), &sc("m1", &["pak03_dir.vpk"])).unwrap();
    let g = sidecar::group(d.path());
    assert_eq!(g.len(), 1);
    assert!(g[0].sidecar.is_some());
    assert!(!g[0].vpks[0].enabled);
}

#[test]
fn unknown_vpk_groups_alone() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("pak01_dir.vpk"), b"x").unwrap();
    fs::write(d.path().join("123456_mod.vpk"), b"x").unwrap();
    sidecar::write(d.path(), &sc("m1", &["pak01_dir.vpk"])).unwrap();
    let g = sidecar::group(d.path());
    assert_eq!(g.len(), 2);
    assert!(g[0].sidecar.is_some());
    assert!(g[1].sidecar.is_none());
    assert_eq!(g[1].vpks.len(), 1);
}

#[test]
fn corrupt_sidecar_is_ignored_not_fatal() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("pak01_dir.vpk"), b"x").unwrap();
    fs::create_dir(d.path().join(".shiv")).unwrap();
    fs::write(d.path().join(".shiv/bad.json"), b"{not json").unwrap();
    fs::write(d.path().join(".shiv/readme.txt"), b"hi").unwrap();
    fs::create_dir(d.path().join(".shiv/tmp")).unwrap();
    let g = sidecar::group(d.path());
    assert_eq!(g.len(), 1);
    assert!(g[0].sidecar.is_none());
    assert!(matches!(
        sidecar::read(d.path(), "bad"),
        Err(SidecarError::Parse { .. })
    ));
}

#[test]
fn rejects_mod_id_with_path_separators() {
    let d = tempdir().unwrap();
    for id in ["../evil", "a/b", "a\\b", "", "a.b"] {
        assert!(matches!(
            sidecar::read(d.path(), id),
            Err(SidecarError::InvalidModId(_))
        ));
        assert!(matches!(
            sidecar::write(d.path(), &sc(id, &[])),
            Err(SidecarError::InvalidModId(_))
        ));
    }
    assert!(!d.path().join(".shiv").exists());
}

#[test]
fn group_does_not_show_chunks_as_mods() {
    let d = tempdir().unwrap();
    fs::write(d.path().join("pak01_dir.vpk"), b"x").unwrap();
    fs::write(d.path().join("pak01_000.vpk"), b"x").unwrap();
    fs::write(d.path().join("pak01_001.vpk"), b"x").unwrap();
    sidecar::write(d.path(), &sc("m1", &["pak01_dir.vpk"])).unwrap();
    let g = sidecar::group(d.path());
    assert_eq!(g.len(), 1);
    assert_eq!(g[0].sidecar.as_ref().unwrap().mod_id, "m1");
    assert_eq!(g[0].vpks[0].chunks.len(), 2);
}
