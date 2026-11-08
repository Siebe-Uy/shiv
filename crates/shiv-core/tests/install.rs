use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use shiv_core::game::GamePaths;
use shiv_core::install::{self, InstallError, Limits, ModMeta};
use shiv_core::sidecar::{self, Sidecar};
use tempfile::{TempDir, tempdir};
use zip::write::SimpleFileOptions;

struct Env {
    _root: TempDir,
    paths: GamePaths,
    work: PathBuf,
}

fn env() -> Env {
    let root = tempdir().unwrap();
    let paths = GamePaths::from_root(root.path().join("Deadlock"));
    fs::create_dir_all(&paths.addons).unwrap();
    let work = root.path().join("work");
    fs::create_dir_all(&work).unwrap();
    Env {
        _root: root,
        paths,
        work,
    }
}

fn meta(id: &str) -> ModMeta {
    ModMeta {
        mod_id: id.into(),
        name: format!("Mod {id}"),
        version: Some("1.0".into()),
        thumbnail_url: None,
    }
}

fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
    for (name, data) in entries {
        z.start_file(*name, SimpleFileOptions::default()).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap();
}

fn zip_in(e: &Env, entries: &[(&str, &[u8])]) -> PathBuf {
    let p = e.work.join("mod.zip");
    write_zip(&p, entries);
    p
}

/// Every file and directory under `root` with contents (dirs map to None).
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                out.insert(p.clone(), None);
                walk(&p, out);
            } else {
                out.insert(p.clone(), Some(fs::read(&p).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != ".shiv")
        .collect();
    v.sort();
    v
}

fn not_running() -> bool {
    false
}

fn run(e: &Env, archive: &Path, id: &str) -> Result<Sidecar, InstallError> {
    install::install_with(archive, meta(id), &e.paths, not_running)
}

#[test]
fn installs_zip_with_one_vpk() {
    let e = env();
    let a = zip_in(&e, &[("readme.txt", b"hi"), ("skin.vpk", b"VPKDATA")]);
    let sc = run(&e, &a, "m1").unwrap();
    assert_eq!(sc.vpks, vec!["pak01_dir.vpk"]);
    assert_eq!(sc.mod_id, "m1");
    assert_eq!(sc.name, "Mod m1");
    assert_eq!(
        fs::read(e.paths.addons.join("pak01_dir.vpk")).unwrap(),
        b"VPKDATA"
    );
    assert_eq!(names(&e.paths.addons), vec!["pak01_dir.vpk"]);
    assert_eq!(sidecar::read(&e.paths.addons, "m1").unwrap(), Some(sc));
    assert!(!e.paths.addons.join(".shiv/tmp").exists());
}

#[test]
fn installs_multi_vpk_with_consecutive_paks() {
    let e = env();
    fs::write(e.paths.addons.join("pak01_dir.vpk"), b"other").unwrap();
    let a = zip_in(
        &e,
        &[
            ("sub/pak07_dir.vpk", b"B"),
            ("sub/pak03_dir.vpk", b"A"),
            ("x/more.vpk", b"C"),
        ],
    );
    let sc = run(&e, &a, "m1").unwrap();
    assert_eq!(
        sc.vpks,
        vec!["pak02_dir.vpk", "pak03_dir.vpk", "pak04_dir.vpk"]
    );
    assert_eq!(
        fs::read(e.paths.addons.join("pak02_dir.vpk")).unwrap(),
        b"A"
    );
    assert_eq!(
        fs::read(e.paths.addons.join("pak03_dir.vpk")).unwrap(),
        b"B"
    );
    assert_eq!(
        fs::read(e.paths.addons.join("pak04_dir.vpk")).unwrap(),
        b"C"
    );
    assert_eq!(
        fs::read(e.paths.addons.join("pak01_dir.vpk")).unwrap(),
        b"other"
    );
}

#[test]
fn moves_chunk_files_with_their_dir_vpk() {
    let e = env();
    fs::write(e.paths.addons.join("pak01_dir.vpk"), b"other").unwrap();
    let a = zip_in(
        &e,
        &[
            ("pak05_dir.vpk", b"D"),
            ("pak05_000.vpk", b"C0"),
            ("pak05_001.vpk", b"C1"),
        ],
    );
    let sc = run(&e, &a, "m1").unwrap();
    assert_eq!(sc.vpks, vec!["pak02_dir.vpk"]);
    assert_eq!(
        names(&e.paths.addons),
        vec![
            "pak01_dir.vpk",
            "pak02_000.vpk",
            "pak02_001.vpk",
            "pak02_dir.vpk"
        ]
    );
    assert_eq!(
        fs::read(e.paths.addons.join("pak02_001.vpk")).unwrap(),
        b"C1"
    );
}

#[test]
fn renames_non_pak_vpk_to_pak_dir() {
    let e = env();
    let a = zip_in(&e, &[("Cool Skin v2.VPK", b"S")]);
    let sc = run(&e, &a, "m1").unwrap();
    assert_eq!(sc.vpks, vec!["pak01_dir.vpk"]);
    assert_eq!(names(&e.paths.addons), vec!["pak01_dir.vpk"]);
}

#[test]
fn rejects_path_traversal_entry() {
    for bad in [
        "../evil.vpk",
        "a/../../evil.vpk",
        "/tmp/shiv-evil-test.vpk",
        "C:\\evil.vpk",
    ] {
        let e = env();
        let before = snapshot(e._root.path());
        let a = zip_in(&e, &[("ok.vpk", b"fine"), (bad, b"evil")]);
        let before_with_zip = {
            let mut b = before.clone();
            b.extend(snapshot(&e.work));
            b
        };
        let err = run(&e, &a, "m1").unwrap_err();
        assert!(matches!(err, InstallError::UnsafePath(_)), "{bad}: {err:?}");
        assert_eq!(snapshot(e._root.path()), before_with_zip, "{bad}");
        assert!(!Path::new("/tmp/shiv-evil-test.vpk").exists());
    }
}

#[test]
fn rejects_symlink_entry() {
    let e = env();
    let p = e.work.join("link.zip");
    let mut z = zip::ZipWriter::new(fs::File::create(&p).unwrap());
    z.start_file("skin.vpk", SimpleFileOptions::default())
        .unwrap();
    z.write_all(b"x").unwrap();
    z.add_symlink("evil.vpk", "/etc/passwd", SimpleFileOptions::default())
        .unwrap();
    z.finish().unwrap();
    let err = run(&e, &p, "m1").unwrap_err();
    assert!(matches!(err, InstallError::UnsafePath(_)), "{err:?}");
    assert!(names(&e.paths.addons).is_empty());
    assert!(!e.paths.addons.join(".shiv").exists());
}

#[test]
fn archive_without_vpk_leaves_addons_unchanged() {
    let e = env();
    fs::write(e.paths.addons.join("pak01_dir.vpk"), b"keep").unwrap();
    let a = zip_in(&e, &[("readme.txt", b"hi"), ("img/a.png", b"png")]);
    let before = snapshot(&e.paths.addons);
    let err = run(&e, &a, "m1").unwrap_err();
    assert!(matches!(err, InstallError::NoVpk), "{err:?}");
    assert_eq!(snapshot(&e.paths.addons), before);
}

#[test]
fn refuses_while_game_running() {
    let e = env();
    let a = zip_in(&e, &[("skin.vpk", b"x")]);
    let before = snapshot(&e.paths.addons);
    let err = install::install_with(&a, meta("m1"), &e.paths, || true).unwrap_err();
    assert!(matches!(err, InstallError::GameRunning), "{err:?}");
    assert_eq!(snapshot(&e.paths.addons), before);
    assert!(!e.paths.addons.join(".shiv").exists());
}

#[test]
fn addons_full_leaves_addons_unchanged() {
    let e = env();
    for n in 1..=99 {
        fs::write(e.paths.addons.join(format!("pak{n:02}_dir.vpk")), b"x").unwrap();
    }
    let a = zip_in(&e, &[("skin.vpk", b"x")]);
    let before = snapshot(&e.paths.addons);
    let err = run(&e, &a, "m1").unwrap_err();
    assert!(matches!(err, InstallError::AddonsFull), "{err:?}");
    assert_eq!(snapshot(&e.paths.addons), before);
}

#[test]
fn reinstall_replaces_old_files_and_keeps_old_on_failure() {
    let e = env();
    let a1 = zip_in(&e, &[("pak01_dir.vpk", b"v1"), ("pak01_000.vpk", b"c1")]);
    let first = run(&e, &a1, "m1").unwrap();
    assert_eq!(first.vpks, vec!["pak01_dir.vpk"]);

    // A failing reinstall leaves the old mod intact.
    let bad = e.work.join("bad.zip");
    write_zip(&bad, &[("readme.txt", b"no vpk")]);
    let before = snapshot(&e.paths.addons);
    assert!(run(&e, &bad, "m1").is_err());
    assert_eq!(snapshot(&e.paths.addons), before);
    let trav = e.work.join("trav.zip");
    write_zip(&trav, &[("a.vpk", b"n"), ("../x.vpk", b"n")]);
    assert!(run(&e, &trav, "m1").is_err());
    assert_eq!(snapshot(&e.paths.addons), before);

    // A successful reinstall replaces the files and the sidecar.
    let a2 = e.work.join("v2.zip");
    write_zip(&a2, &[("new.vpk", b"v2")]);
    let second = run(&e, &a2, "m1").unwrap();
    assert_eq!(second.vpks.len(), 1);
    let name = &second.vpks[0];
    assert_eq!(fs::read(e.paths.addons.join(name)).unwrap(), b"v2");
    assert_eq!(names(&e.paths.addons), vec![name.clone()]);
    assert_eq!(sidecar::read(&e.paths.addons, "m1").unwrap(), Some(second));
}

#[test]
fn uninstall_removes_vpks_chunks_and_sidecar() {
    let e = env();
    let a = zip_in(
        &e,
        &[
            ("pak02_dir.vpk", b"D"),
            ("pak02_000.vpk", b"C"),
            ("b.vpk", b"E"),
        ],
    );
    run(&e, &a, "m1").unwrap();
    fs::write(e.paths.addons.join("pak09_dir.vpk"), b"other").unwrap();
    // Disable one of the mod's dir files; uninstall must still find it.
    fs::rename(
        e.paths.addons.join("pak02_dir.vpk"),
        e.paths.addons.join("pak02_dir.vpk.disabled"),
    )
    .unwrap();
    let mods = sidecar::group(&e.paths.addons);
    let m = mods
        .iter()
        .find(|m| m.sidecar.as_ref().is_some_and(|s| s.mod_id == "m1"))
        .unwrap();
    install::uninstall(m).unwrap();
    assert_eq!(names(&e.paths.addons), vec!["pak09_dir.vpk"]);
    assert_eq!(sidecar::read(&e.paths.addons, "m1").unwrap(), None);

    // Unknown mod: only its own VPK goes.
    let mods = sidecar::group(&e.paths.addons);
    assert_eq!(mods.len(), 1);
    assert!(mods[0].sidecar.is_none());
    install::uninstall(&mods[0]).unwrap();
    assert!(names(&e.paths.addons).is_empty());
}

#[test]
fn unsupported_magic_is_rejected() {
    let e = env();
    // A .zip extension must not matter; magic does.
    let p = e.work.join("fake.zip");
    fs::write(&p, b"this is not an archive at all").unwrap();
    let err = run(&e, &p, "m1").unwrap_err();
    assert!(matches!(err, InstallError::UnsupportedArchive), "{err:?}");
    assert!(names(&e.paths.addons).is_empty());
    // A real zip with a wrong extension is accepted.
    let z = e.work.join("real.bin");
    write_zip(&z, &[("a.vpk", b"x")]);
    assert!(run(&e, &z, "m2").is_ok());
}

fn write_7z(path: &Path, files: &[(&str, &[u8], u32)]) {
    let mut w = sevenz_rust2::ArchiveWriter::create(path).unwrap();
    for (name, data, attrs) in files {
        let mut entry = sevenz_rust2::ArchiveEntry::new_file(name);
        if *attrs != 0 {
            entry.has_windows_attributes = true;
            entry.windows_attributes = *attrs;
        }
        w.push_archive_entry(entry, Some(*data)).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn sevenz_extracts() {
    let e = env();
    let p = e.work.join("mod.7z");
    write_7z(
        &p,
        &[
            ("docs/readme.txt", b"hi", 0),
            ("m/pak04_dir.vpk", b"SEVEN", 0),
        ],
    );
    let sc = run(&e, &p, "m7").unwrap();
    assert_eq!(sc.vpks, vec!["pak01_dir.vpk"]);
    assert_eq!(
        fs::read(e.paths.addons.join("pak01_dir.vpk")).unwrap(),
        b"SEVEN"
    );

    // Traversal and symlinks are rejected before anything is written.
    let e = env();
    let before = snapshot(e._root.path());
    let p = e.work.join("bad.7z");
    write_7z(&p, &[("ok.vpk", b"x", 0), ("../evil.vpk", b"x", 0)]);
    let before = {
        let mut b = before;
        b.extend(snapshot(&e.work));
        b
    };
    assert!(matches!(
        run(&e, &p, "m7"),
        Err(InstallError::UnsafePath(_))
    ));
    assert_eq!(snapshot(e._root.path()), before);
    let p = e.work.join("link.7z");
    let unix_symlink = (0o120777u32 << 16) | 0x8000;
    write_7z(
        &p,
        &[("ok.vpk", b"x", 0), ("l.vpk", b"/etc/passwd", unix_symlink)],
    );
    assert!(matches!(
        run(&e, &p, "m8"),
        Err(InstallError::UnsafePath(_))
    ));
    assert!(names(&e.paths.addons).is_empty());
}

fn rar_crc16(bytes: &[u8]) -> u16 {
    (crc32fast::hash(bytes) & 0xFFFF) as u16
}

/// Hand-built RAR 4.x archive holding stored (method 0x30) files.
fn rar4_stored(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"Rar!\x1a\x07\x00".to_vec();
    // Main header.
    let mut main = vec![0x73, 0, 0, 13, 0, 0, 0, 0, 0, 0, 0];
    let crc = rar_crc16(&main);
    let mut hdr = crc.to_le_bytes().to_vec();
    hdr.append(&mut main);
    out.extend(hdr);
    for (name, data) in files {
        let mut h = vec![0x74];
        h.extend(0x8000u16.to_le_bytes()); // flags: ADD_SIZE present
        h.extend((32 + name.len() as u16).to_le_bytes());
        h.extend((data.len() as u32).to_le_bytes()); // packed
        h.extend((data.len() as u32).to_le_bytes()); // unpacked
        h.push(3); // host OS: unix
        h.extend(crc32fast::hash(data).to_le_bytes());
        h.extend(0x5A21_0000u32.to_le_bytes()); // dos time
        h.push(20); // unpack version
        h.push(0x30); // stored
        h.extend((name.len() as u16).to_le_bytes());
        h.extend(0o100644u32.to_le_bytes()); // attrs
        h.extend(name.as_bytes());
        out.extend(rar_crc16(&h).to_le_bytes());
        out.extend(h);
        out.extend(*data);
    }
    out.extend([0xC4, 0x3D, 0x7B, 0x00, 0x40, 0x07, 0x00]);
    out
}

#[test]
fn rar_extracts() {
    let e = env();
    let p = e.work.join("mod.rar");
    fs::write(
        &p,
        rar4_stored(&[("readme.txt", b"hi"), ("pak06_dir.vpk", b"RARDATA")]),
    )
    .unwrap();
    let sc = run(&e, &p, "mr").unwrap();
    assert_eq!(sc.vpks, vec!["pak01_dir.vpk"]);
    assert_eq!(
        fs::read(e.paths.addons.join("pak01_dir.vpk")).unwrap(),
        b"RARDATA"
    );

    let e = env();
    let p = e.work.join("bad.rar");
    fs::write(&p, rar4_stored(&[("ok.vpk", b"x"), ("../evil.vpk", b"x")])).unwrap();
    assert!(matches!(
        run(&e, &p, "mr"),
        Err(InstallError::UnsafePath(_))
    ));
    assert!(names(&e.paths.addons).is_empty());
}

#[test]
fn size_cap_is_enforced() {
    let e = env();
    let a = zip_in(&e, &[("skin.vpk", &[7u8; 4096])]);
    let before = snapshot(&e.paths.addons);
    let small = Limits {
        max_bytes: 1000,
        max_entries: 100,
    };
    let err =
        install::install_with_limits(&a, meta("m1"), &e.paths, not_running, small).unwrap_err();
    assert!(matches!(err, InstallError::TooLarge), "{err:?}");
    assert_eq!(snapshot(&e.paths.addons), before);

    let many = zip_in(&e, &[("a.txt", b"1"), ("b.txt", b"2"), ("skin.vpk", b"3")]);
    let few = Limits {
        max_bytes: 1 << 20,
        max_entries: 2,
    };
    let err =
        install::install_with_limits(&many, meta("m1"), &e.paths, not_running, few).unwrap_err();
    assert!(matches!(err, InstallError::TooLarge), "{err:?}");
    assert_eq!(snapshot(&e.paths.addons), before);
}
