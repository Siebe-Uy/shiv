use shiv_core::crosshair::{self, CrosshairConfig, Rgb};
use shiv_core::game::GamePaths;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::tempdir;

const HEADER: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

/// Produced by `generateConfigString` in the old repo's
/// `packages/crosshair/src/config-generator.ts` (run with bun) for `custom()`.
const REFERENCE: &str = "citadel_crosshair_color_r 0; citadel_crosshair_color_g 255; citadel_crosshair_color_b 128; citadel_crosshair_pip_outline_border 1; citadel_crosshair_pip_gap_static true; citadel_crosshair_pip_opacity 0.8; citadel_crosshair_pip_width 2.5; citadel_crosshair_pip_height 12; citadel_crosshair_pip_gap -3; citadel_crosshair_dot_opacity 0; citadel_crosshair_dot_outline_opacity 0.5";

/// The same, for `DEFAULT_CROSSHAIR_CONFIG`.
const REFERENCE_DEFAULT: &str = "citadel_crosshair_color_r 255; citadel_crosshair_color_g 255; citadel_crosshair_color_b 255; citadel_crosshair_pip_outline_border 0; citadel_crosshair_pip_gap_static false; citadel_crosshair_pip_opacity 1; citadel_crosshair_pip_width 4; citadel_crosshair_pip_height 16; citadel_crosshair_pip_gap 0; citadel_crosshair_dot_opacity 1; citadel_crosshair_dot_outline_opacity 1";

fn custom() -> CrosshairConfig {
    CrosshairConfig {
        color: Rgb {
            r: 0,
            g: 255,
            b: 128,
        },
        pip_border: true,
        pip_gap_static: true,
        pip_opacity: 0.8,
        width: 2.5,
        height: 12.0,
        gap: -3.0,
        dot_opacity: 0.0,
        dot_outline_opacity: 0.5,
    }
}

/// A temp game install and a temp `userdata/<id>/1422450`; never the real
/// ones.
fn fixture() -> (tempfile::TempDir, GamePaths, PathBuf) {
    let d = tempdir().unwrap();
    let paths = GamePaths::from_root(d.path().join("Deadlock"));
    let user = d.path().join("Steam/userdata/42/1422450");
    (d, paths, user)
}

#[test]
fn to_commands_matches_reference_string() {
    assert_eq!(crosshair::to_commands(&custom()), REFERENCE);
    assert_eq!(
        crosshair::to_commands(&CrosshairConfig::default()),
        REFERENCE_DEFAULT
    );
}

#[test]
fn negative_zero_prints_like_javascript() {
    let c = CrosshairConfig {
        gap: -0.0,
        ..CrosshairConfig::default()
    };
    assert!(crosshair::to_commands(&c).contains("citadel_crosshair_pip_gap 0;"));
}

fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

/// The lines `apply` puts in `base` for `custom()`, in key order.
fn custom_lines(nl: &str, skip: &[&str]) -> String {
    [
        ("crosshair_color_b", "128"),
        ("crosshair_color_g", "255"),
        ("crosshair_color_r", "0"),
        ("crosshair_dot_opacity", "0"),
        ("crosshair_dot_outline_opacity", "0.5"),
        ("crosshair_pip_gap", "-3"),
        ("crosshair_pip_gap_static", "true"),
        ("crosshair_pip_height", "12"),
        ("crosshair_pip_opacity", "0.8"),
        ("crosshair_pip_outline_border", "1"),
        ("crosshair_pip_width", "2.5"),
        ("crosshair_themed", "false"),
    ]
    .iter()
    .filter(|(k, _)| !skip.contains(k))
    .map(|(k, v)| format!("{nl}\t\t{k} = \"{v}\""))
    .collect()
}

#[test]
fn apply_creates_both_settings_files_when_missing() {
    let (_d, paths, user) = fixture();
    assert_eq!(crosshair::load(&paths, &user).unwrap(), None);
    crosshair::apply(&paths, &user, &custom()).unwrap();
    let expected = format!(
        "{HEADER}\n{{\n\tbase = {{{}\n\t}}\n\theroes = {{}}\n}}\n",
        custom_lines("\n", &[])
    );
    let user_file = crosshair::user_settings_path(&user);
    assert_eq!(user_file, user.join("remote/cfg/citadel_hero_settings.lst"));
    assert_eq!(fs::read_to_string(&user_file).unwrap(), expected);
    let game_file = crosshair::game_settings_path(&paths);
    assert_eq!(
        game_file,
        paths
            .root
            .join("game/citadel/cfg/citadel_hero_settings.lst")
    );
    assert_eq!(fs::read_to_string(&game_file).unwrap(), expected);
    assert_eq!(crosshair::load(&paths, &user).unwrap(), Some(custom()));
    // Nothing existed before, so there is nothing to back up, and no
    // autoexec.cfg is created.
    assert!(!user.join("local/citadel_hero_settings.lst.bak").exists());
    assert!(!paths.autoexec.exists());
}

#[test]
fn apply_changes_only_crosshair_values_and_keeps_everything_else() {
    let (_d, paths, user) = fixture();
    let nl = "\r\n";
    let before = format!(
        "\u{feff}{HEADER}{nl}{{{nl}\tbase ={nl}\t{{{nl}\t\t// mine{nl}\t\tmouse_sens = \"1.25\"{nl}\t\tcrosshair_pip_gap = \"7\"{nl}\t\tcrosshair_themed = true{nl}\t}}{nl}\theroes ={nl}\t{{{nl}\t\thero_abrams = {{ crosshair_pip_gap = \"3\" }}{nl}\t}}{nl}}}{nl}"
    );
    let file = crosshair::user_settings_path(&user);
    write(&file, &before);
    crosshair::apply(&paths, &user, &custom()).unwrap();
    let expected = format!(
        "\u{feff}{HEADER}{nl}{{{nl}\tbase ={nl}\t{{{nl}\t\t// mine{nl}\t\tmouse_sens = \"1.25\"{nl}\t\tcrosshair_pip_gap = \"-3\"{nl}\t\tcrosshair_themed = \"false\"{}{nl}\t}}{nl}\theroes ={nl}\t{{{nl}\t\thero_abrams = {{ crosshair_pip_gap = \"3\" }}{nl}\t}}{nl}}}{nl}",
        custom_lines(nl, &["crosshair_pip_gap", "crosshair_themed"])
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    // A second apply of the same crosshair changes nothing.
    crosshair::apply(&paths, &user, &custom()).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
}

#[test]
fn apply_backs_up_the_previous_files() {
    let (_d, paths, user) = fixture();
    let old =
        format!("{HEADER}\n{{\n\tbase = {{ crosshair_pip_gap = \"9\" }}\n\theroes = {{}}\n}}\n");
    write(&crosshair::user_settings_path(&user), &old);
    write(&crosshair::game_settings_path(&paths), &old);
    crosshair::apply(&paths, &user, &custom()).unwrap();
    // The Steam copy's backup sits in `local`, outside the cloud-synced
    // `remote` folder.
    let user_bak = user.join("local/citadel_hero_settings.lst.bak");
    assert_eq!(fs::read_to_string(&user_bak).unwrap(), old);
    assert!(
        !user
            .join("remote/cfg/citadel_hero_settings.lst.bak")
            .exists()
    );
    let game_bak = paths
        .root
        .join("game/citadel/cfg/citadel_hero_settings.lst.bak");
    assert_eq!(fs::read_to_string(&game_bak).unwrap(), old);
    // The backup is the file as it was just before each apply.
    let applied = fs::read_to_string(crosshair::user_settings_path(&user)).unwrap();
    crosshair::apply(&paths, &user, &CrosshairConfig::default()).unwrap();
    assert_eq!(fs::read_to_string(&user_bak).unwrap(), applied);
}

#[cfg(unix)]
#[test]
fn apply_keeps_file_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, paths, user) = fixture();
    let file = crosshair::user_settings_path(&user);
    write(&file, crosshair::EMPTY_SETTINGS);
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    crosshair::apply(&paths, &user, &custom()).unwrap();
    let mode = fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640);
}

#[test]
fn load_reads_quoted_and_bare_values() {
    let (_d, paths, user) = fixture();
    write(
        &crosshair::user_settings_path(&user),
        format!(
            "{HEADER}\n{{\n\tbase =\n\t{{\n\t\tcrosshair_color_r = 10\n\t\tcrosshair_pip_gap = \"4\"\n\t\tcrosshair_pip_outline_border = \"1\"\n\t\tcrosshair_pip_gap_static = true\n\t\tcrosshair_pip_width = \"wide\"\n\t}}\n}}\n"
        ),
    );
    let c = crosshair::load(&paths, &user).unwrap().unwrap();
    assert_eq!(c.color.r, 10);
    assert_eq!(c.color.g, 255, "missing keys keep the default");
    assert_eq!(c.gap, 4.0);
    assert!(c.pip_border && c.pip_gap_static);
    assert_eq!(c.width, CrosshairConfig::default().width, "unparsable kept");
}

#[test]
fn load_falls_back_to_the_game_copy_and_ignores_files_without_a_crosshair() {
    let (_d, paths, user) = fixture();
    write(
        &crosshair::user_settings_path(&user),
        crosshair::EMPTY_SETTINGS,
    );
    assert_eq!(crosshair::load(&paths, &user).unwrap(), None);
    fs::remove_file(crosshair::user_settings_path(&user)).unwrap();
    write(
        &crosshair::game_settings_path(&paths),
        format!("{HEADER}\n{{\n\tbase = {{ crosshair_pip_height = \"20\" }}\n}}\n"),
    );
    assert_eq!(
        crosshair::load(&paths, &user).unwrap().unwrap().height,
        20.0
    );
}

#[test]
fn unreadable_settings_are_left_alone() {
    let (_d, paths, user) = fixture();
    let file = crosshair::user_settings_path(&user);
    write(&file, "not a kv3 file\n");
    let err = crosshair::apply(&paths, &user, &custom()).unwrap_err();
    assert!(
        matches!(err, crosshair::CrosshairError::InvalidSettings(_)),
        "{err}"
    );
    assert!(crosshair::load(&paths, &user).is_err());
    assert_eq!(fs::read_to_string(&file).unwrap(), "not a kv3 file\n");
    // Nothing else was written either.
    assert!(!crosshair::game_settings_path(&paths).exists());
    write(&file, b"\xff\xfe\x00");
    assert!(matches!(
        crosshair::apply(&paths, &user, &custom()),
        Err(crosshair::CrosshairError::NotText(_))
    ));
    assert_eq!(fs::read(&file).unwrap(), b"\xff\xfe\x00");
}

#[test]
fn a_failed_write_restores_the_files_already_written() {
    let (_d, paths, user) = fixture();
    let game_file = crosshair::game_settings_path(&paths);
    let old = format!("{HEADER}\n{{\n\tbase = {{}}\n}}\n");
    write(&game_file, &old);
    write(&crosshair::user_settings_path(&user), &old);
    // The Steam copy's backup cannot be written: `local` is a file.
    write(&user.join("local"), "in the way");
    assert!(crosshair::apply(&paths, &user, &custom()).is_err());
    assert_eq!(fs::read_to_string(&game_file).unwrap(), old);
    assert_eq!(
        fs::read_to_string(crosshair::user_settings_path(&user)).unwrap(),
        old
    );
}

#[test]
fn apply_removes_old_autoexec_crosshair_blocks_and_keeps_user_lines() {
    let (_d, paths, user) = fixture();
    let shiv = "// === Shiv - Crosshair Settings (DO NOT EDIT) ===\r\ncitadel_crosshair_pip_gap 5\r\n// === End Shiv Crosshair Settings ===\r\n";
    let legacy = "// === Deadlock Mod Manager - Crosshair Settings (DO NOT EDIT) ===\ncitadel_crosshair_pip_gap 2\n// === End Crosshair Settings ===";
    let before = format!("bind x jump\r\n{shiv}fps_max 240\n{legacy}");
    write(&paths.autoexec, &before);
    crosshair::apply(&paths, &user, &custom()).unwrap();
    assert_eq!(
        fs::read_to_string(&paths.autoexec).unwrap(),
        "bind x jump\r\nfps_max 240\n"
    );
    let bak = paths.autoexec.with_file_name("autoexec.cfg.bak");
    assert_eq!(fs::read_to_string(&bak).unwrap(), before);

    // An autoexec.cfg without a crosshair block is not touched.
    let mine = "// === Shiv - Crosshair Settings (DO NOT EDIT) ===\nbind y jump\n";
    write(&paths.autoexec, mine);
    fs::remove_file(&bak).unwrap();
    crosshair::apply(&paths, &user, &custom()).unwrap();
    assert_eq!(fs::read_to_string(&paths.autoexec).unwrap(), mine);
    assert!(!bak.exists());
}

// --- Steam account choice -------------------------------------------------

/// SteamID64 of account id `n`.
fn steam_id(n: u32) -> u64 {
    76_561_197_960_265_728 + u64::from(n)
}

fn login_users(steam: &Path, users: &[(u32, &str)]) {
    let body: String = users
        .iter()
        .map(|(n, extra)| {
            format!(
                "\t\"{}\"\n\t{{\n\t\t\"AccountName\"\t\t\"user{n}\"\n{extra}\t}}\n",
                steam_id(*n)
            )
        })
        .collect();
    write(
        &steam.join("config/loginusers.vdf"),
        format!("\"users\"\n{{\n{body}}}\n"),
    );
}

fn touch(path: &Path, secs_ago: u64) {
    write(path, "x");
    let when = SystemTime::now() - Duration::from_secs(secs_ago);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn user_dir_is_the_account_steam_used_last() {
    let d = tempdir().unwrap();
    let steam = d.path();
    // Account 1 is flagged MostRecent, but account 2's session file is newer.
    login_users(steam, &[(1, "\t\t\"MostRecent\"\t\t\"1\"\n"), (2, "")]);
    touch(&steam.join("userdata/1/config/localconfig.vdf"), 3600);
    touch(&steam.join("userdata/2/config/localconfig.vdf"), 10);
    assert_eq!(
        crosshair::user_dir(steam).unwrap(),
        steam.join("userdata/2/1422450")
    );
    // Without session files, the MostRecent flag decides.
    fs::remove_dir_all(steam.join("userdata")).unwrap();
    assert_eq!(
        crosshair::user_dir(steam).unwrap(),
        steam.join("userdata/1/1422450")
    );
}

#[test]
fn user_dir_without_login_users_takes_the_newest_settings_file() {
    let d = tempdir().unwrap();
    let steam = d.path();
    touch(
        &steam.join("userdata/1/1422450/remote/cfg/citadel_hero_settings.lst"),
        3600,
    );
    touch(
        &steam.join("userdata/2/1422450/remote/cfg/citadel_hero_settings.lst"),
        10,
    );
    fs::create_dir_all(steam.join("userdata/3/730")).unwrap();
    assert_eq!(
        crosshair::user_dir(steam).unwrap(),
        steam.join("userdata/2/1422450")
    );
}

#[test]
fn user_dir_needs_a_signed_in_account() {
    let d = tempdir().unwrap();
    let err = crosshair::user_dir(d.path()).unwrap_err();
    assert!(
        matches!(err, crosshair::CrosshairError::NoSteamAccount),
        "{err}"
    );
    assert!(err.to_string().contains("Sign in to Steam"), "{err}");
    assert!(
        crosshair::CrosshairError::SteamNotFound
            .to_string()
            .contains("Steam was not found")
    );
}
