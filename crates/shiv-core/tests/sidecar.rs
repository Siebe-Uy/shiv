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
        nsfw: None,
        hero: None,
        category: None,
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
fn nsfw_flag_roundtrips_and_old_sidecars_read_as_unknown() {
    let d = tempdir().unwrap();
    for (id, flag) in [
        ("spicy", Some(true)),
        ("plain", Some(false)),
        ("unknown", None),
    ] {
        let mut s = sc(id, &["pak01_dir.vpk"]);
        s.nsfw = flag;
        sidecar::write(d.path(), &s).unwrap();
        assert_eq!(sidecar::read(d.path(), id).unwrap(), Some(s));
    }
    // Unknown is not written, so the file matches what older builds wrote.
    let raw = fs::read_to_string(d.path().join(".shiv/unknown.json")).unwrap();
    assert!(!raw.contains("nsfw"), "{raw}");
    let raw = fs::read_to_string(d.path().join(".shiv/plain.json")).unwrap();
    assert!(raw.contains("\"nsfw\": false"), "{raw}");
    // Written before the field existed.
    fs::write(
        d.path().join(".shiv/old.json"),
        r#"{"mod_id":"old","name":"Old","version":null,"thumbnail_url":null,"vpks":["pak02_dir.vpk"]}"#,
    )
    .unwrap();
    let old = sidecar::read(d.path(), "old").unwrap().unwrap();
    assert_eq!(old.nsfw, None);
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

fn installed(
    id: &str,
    hero: Option<&str>,
    category: Option<&str>,
    enabled: bool,
) -> shiv_core::sidecar::InstalledMod {
    let mut s = sc(id, &["pak01_dir.vpk"]);
    s.hero = hero.map(Into::into);
    s.category = category.map(Into::into);
    shiv_core::sidecar::InstalledMod {
        sidecar: Some(s),
        vpks: vec![shiv_core::addons::InstalledVpk {
            file: format!("/a/pak01_dir.vpk{}", if enabled { "" } else { ".disabled" }).into(),
            pak: Some(1),
            enabled,
            chunks: Vec::new(),
        }],
    }
}

#[test]
fn conflicts_are_enabled_mods_for_the_same_hero_and_category() {
    let mods = vec![
        installed("same", Some("Haze"), Some("Skins"), true),
        installed("off", Some("Haze"), Some("Skins"), false),
        installed("other-hero", Some("Seven"), Some("Skins"), true),
        installed("other-kind", Some("Haze"), Some("Audio"), true),
        installed("unknown", None, None, true),
        installed("me", Some("Haze"), Some("Skins"), true),
    ];
    let ids: Vec<&str> = sidecar::conflicts(&mods, "me", "Haze", Some("Skins"))
        .iter()
        .map(|s| s.mod_id.as_str())
        .collect();
    assert_eq!(ids, ["same"]);
}

#[test]
fn conflicts_with_falls_back_to_the_catalog_for_old_sidecars() {
    let mods = vec![
        installed("old", None, None, true),
        installed("new", Some("Bebop"), Some("Skins"), true),
    ];
    let kind =
        |id: &str| (id == "old").then(|| (Some("Bebop".to_string()), Some("Skins".to_string())));
    assert_eq!(
        sidecar::conflicts_with(&mods, "new", kind),
        [("old".to_string(), "Mod old".to_string())]
    );
    assert!(sidecar::conflicts_with(&mods, "new", |_| None).is_empty());
}

#[test]
fn hero_and_category_roundtrip_and_stay_out_of_old_sidecars() {
    let d = tempdir().unwrap();
    let mut s = sc("h1", &["pak01_dir.vpk"]);
    s.hero = Some("Haze".into());
    s.category = Some("Skins".into());
    sidecar::write(d.path(), &s).unwrap();
    assert_eq!(sidecar::read(d.path(), "h1").unwrap(), Some(s));
    let plain = sc("h2", &["pak02_dir.vpk"]);
    sidecar::write(d.path(), &plain).unwrap();
    let raw = fs::read_to_string(d.path().join(".shiv/h2.json")).unwrap();
    assert!(!raw.contains("hero") && !raw.contains("category"), "{raw}");
}
