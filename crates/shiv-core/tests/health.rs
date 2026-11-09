//! Health check against throwaway game folders (never the real install).

use std::fs;
use std::path::Path;

use shiv_core::game::GamePaths;
use shiv_core::health::{self, Finding, FindingKind};

const GAMEINFO_OK: &str = r#""GameInfo"
{
	game "Deadlock"
	FileSystem
	{
		SearchPaths
		{
			Game                citadel/addons
			Mod                 citadel
			Write               citadel
			Game                citadel
			Mod                 core
			Write               core
			Game                core
		}
	}
}
"#;

/// The addons line only appears in a comment and under a non-Windows
/// condition: a string search would find it, the engine would not mount it.
const GAMEINFO_NO_ADDONS: &str = r#""GameInfo"
{
	FileSystem
	{
		SearchPaths
		{
			// Game citadel/addons
			Game                citadel/addons [$LINUX]
			Game_Language       citadel/addons
			Mod                 citadel
			Game                citadel
			Game                core
		}
	}
}
"#;

/// A minimal version 2 VPK: one file (`dir/a.txt`, 5 bytes) stored inline.
fn vpk_bytes() -> Vec<u8> {
    let mut tree = Vec::new();
    tree.extend_from_slice(b"txt\0dir\0a\0");
    tree.extend_from_slice(&0u32.to_le_bytes()); // crc
    tree.extend_from_slice(&0u16.to_le_bytes()); // preload bytes
    tree.extend_from_slice(&0x7fffu16.to_le_bytes()); // archive index: inline
    tree.extend_from_slice(&0u32.to_le_bytes()); // offset
    tree.extend_from_slice(&5u32.to_le_bytes()); // length
    tree.extend_from_slice(&0xffffu16.to_le_bytes()); // terminator
    tree.extend_from_slice(b"\0\0\0");
    let data = b"hello";
    let mut out = Vec::new();
    out.extend_from_slice(&0x55aa_1234u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(tree.len() as u32).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&tree);
    out.extend_from_slice(data);
    out
}

struct Game {
    _dir: tempfile::TempDir,
    paths: GamePaths,
}

impl Game {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        fs::create_dir_all(&paths.addons).unwrap();
        fs::write(&paths.gameinfo, GAMEINFO_OK).unwrap();
        Self { _dir: dir, paths }
    }

    fn put(&self, name: &str, bytes: &[u8]) {
        let path = self.paths.addons.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn check(&self) -> Vec<Finding> {
        health::check(&self.paths)
    }
}

fn kinds(findings: &[Finding]) -> Vec<FindingKind> {
    findings.iter().map(|f| f.kind).collect()
}

fn file_name(f: &Finding) -> String {
    f.file
        .as_deref()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[test]
fn clean_folder_has_no_findings() {
    let game = Game::new();
    game.put("pak01_dir.vpk", &vpk_bytes());
    // A chunk belongs to its dir VPK: not a duplicate, not parsed as a dir VPK.
    game.put("pak01_000.vpk", b"chunk data");
    // A disabled mod is fine as long as its file is intact.
    game.put("pak02_dir.vpk.disabled", &vpk_bytes());
    game.put("custom.vpk", &vpk_bytes());
    game.put(".shiv/m1.json", b"{}");
    game.put("readme.txt", b"not a vpk, ignored");
    let findings = game.check();
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn missing_addons_folder_is_clean() {
    let game = Game::new();
    fs::remove_dir(&game.paths.addons).unwrap();
    assert!(game.check().is_empty());
}

#[test]
fn zero_byte_vpk_is_reported_even_when_disabled() {
    let game = Game::new();
    game.put("pak01_dir.vpk", b"");
    game.put("pak02_dir.vpk.disabled", b"");
    game.put("pak03_dir.vpk", &vpk_bytes());
    game.put("pak03_000.vpk", b"");
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::ZeroByte; 3],
        "{findings:#?}"
    );
    let mut names: Vec<String> = findings.iter().map(file_name).collect();
    names.sort();
    assert_eq!(
        names,
        ["pak01_dir.vpk", "pak02_dir.vpk.disabled", "pak03_000.vpk"]
    );
}

#[test]
fn truncated_vpk_is_reported() {
    let game = Game::new();
    let mut bytes = vpk_bytes();
    bytes.truncate(bytes.len() - 3); // declared data section exceeds the file
    game.put("pak01_dir.vpk", &bytes);
    // Cut inside the directory tree as well.
    game.put("pak02_dir.vpk.disabled", &vpk_bytes()[..40]);
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::TruncatedVpk; 2],
        "{findings:#?}"
    );
}

#[test]
fn truncated_or_missing_chunk_is_reported() {
    let game = Game::new();
    // Same VPK, but the file lives in chunk 000 at offset 0, length 5.
    let mut bytes = vpk_bytes();
    let idx = bytes.windows(2).position(|w| w == [0xff, 0x7f]).unwrap();
    bytes[idx..idx + 2].copy_from_slice(&0u16.to_le_bytes());
    game.put("pak01_dir.vpk", &bytes);
    game.put("pak01_000.vpk", b"he");
    game.put("pak02_dir.vpk", &bytes);
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::TruncatedVpk; 2],
        "{findings:#?}"
    );
    assert!(findings.iter().any(|f| file_name(f) == "pak01_000.vpk"));
    assert!(
        findings
            .iter()
            .any(|f| file_name(f) == "pak02_dir.vpk" && f.detail.contains("pak02_000.vpk"))
    );
}

#[test]
fn unreadable_vpk_is_reported() {
    let game = Game::new();
    game.put("pak01_dir.vpk", b"this is not a vpk at all, just text");
    game.put("custom.vpk.disabled", b"PK\x03\x04 a zip renamed to vpk");
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::UnreadableVpk; 2],
        "{findings:#?}"
    );
}

#[test]
fn partial_download_is_reported() {
    let game = Game::new();
    game.put("pak04_dir.vpk.part", b"half");
    game.put("mod.zip.crdownload", b"half");
    game.put(".shiv/.m1.json.tmp", b"{");
    game.put(".shiv/tmp/stage-1/pak01_dir.vpk", &vpk_bytes());
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::PartialDownload; 4],
        "{findings:#?}"
    );
}

#[test]
fn duplicate_pak_is_reported() {
    let game = Game::new();
    game.put("pak05_dir.vpk", &vpk_bytes());
    game.put("pak05_dir.vpk.disabled", &vpk_bytes());
    game.put("pak05_000.vpk", b"chunk");
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::DuplicatePak],
        "{findings:#?}"
    );
    assert!(
        findings[0].detail.contains("pak05"),
        "{}",
        findings[0].detail
    );
}

#[test]
fn missing_search_path_is_reported() {
    let game = Game::new();
    fs::write(&game.paths.gameinfo, GAMEINFO_NO_ADDONS).unwrap();
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::MissingSearchPath],
        "{findings:#?}"
    );
    // Never modified.
    assert_eq!(
        fs::read_to_string(&game.paths.gameinfo).unwrap(),
        GAMEINFO_NO_ADDONS
    );
}

#[test]
fn unparsable_gameinfo_reports_missing_search_path() {
    let game = Game::new();
    fs::write(&game.paths.gameinfo, "\"GameInfo\" { FileSystem {").unwrap();
    assert_eq!(kinds(&game.check()), vec![FindingKind::MissingSearchPath]);
}

#[cfg(unix)]
#[test]
fn gameinfo_unwritable_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let game = Game::new();
    fs::set_permissions(&game.paths.gameinfo, fs::Permissions::from_mode(0o444)).unwrap();
    let findings = game.check();
    // Root can write anyway; the check is meaningless there.
    if fs::OpenOptions::new()
        .write(true)
        .open(&game.paths.gameinfo)
        .is_ok()
    {
        return;
    }
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::GameinfoUnwritable],
        "{findings:#?}"
    );
    assert_eq!(
        fs::read_to_string(&game.paths.gameinfo).unwrap(),
        GAMEINFO_OK
    );
}

#[test]
fn missing_gameinfo_is_reported_not_created() {
    let game = Game::new();
    fs::remove_file(&game.paths.gameinfo).unwrap();
    let findings = game.check();
    assert_eq!(
        kinds(&findings),
        vec![FindingKind::GameinfoUnwritable],
        "{findings:#?}"
    );
    assert!(
        findings[0].detail.contains("missing"),
        "{}",
        findings[0].detail
    );
    assert!(!game.paths.gameinfo.exists());
}

/// The real Deadlock gameinfo.gi (kept as a kv-parser fixture) mounts addons.
#[test]
fn real_gameinfo_mounts_addons() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../kv-parser/tests/data/gameinfo.gi");
    let game = Game::new();
    fs::copy(fixture, &game.paths.gameinfo).unwrap();
    let findings = game.check();
    assert!(findings.is_empty(), "{findings:#?}");
}
