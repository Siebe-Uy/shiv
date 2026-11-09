use shiv_core::crosshair::{self, CrosshairConfig, Rgb};
use shiv_core::game::GamePaths;
use std::fs;
use tempfile::tempdir;

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

fn fixture() -> (tempfile::TempDir, GamePaths) {
    let d = tempdir().unwrap();
    let paths = GamePaths::from_root(d.path().join("Deadlock"));
    (d, paths)
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

#[test]
fn parse_roundtrips() {
    for c in [custom(), CrosshairConfig::default()] {
        assert_eq!(crosshair::parse(&crosshair::to_commands(&c)), Some(c));
    }
    // Both border values round-trip (the TS test loops over them).
    for pip_border in [true, false] {
        let c = CrosshairConfig {
            pip_border,
            ..CrosshairConfig::default()
        };
        let s = crosshair::to_commands(&c);
        assert!(s.contains(&format!(
            "citadel_crosshair_pip_outline_border {}",
            u8::from(pip_border)
        )));
        assert!(!s.contains("citadel_crosshair_pip_border"));
        assert_eq!(crosshair::parse(&s), Some(c));
    }
}

#[test]
fn parse_reads_legacy_and_partial_input() {
    let legacy = crosshair::parse("citadel_crosshair_pip_border true").unwrap();
    assert!(legacy.pip_border);
    // Missing keys keep their defaults.
    let partial = crosshair::parse("citadel_crosshair_color_r 17").unwrap();
    assert_eq!(partial.color.r, 17);
    assert_eq!(partial.color.g, 255);
    assert_eq!(partial.width, CrosshairConfig::default().width);
    // One command per line, quoted values and CRLF are read too.
    let lines =
        crosshair::parse("citadel_crosshair_pip_width \"3\"\r\ncitadel_crosshair_pip_gap 5\r\n")
            .unwrap();
    assert_eq!((lines.width, lines.gap), (3.0, 5.0));
    // Nothing recognisable is not a crosshair.
    assert_eq!(crosshair::parse("bind x jump; fps_max 240"), None);
    assert_eq!(crosshair::parse(""), None);
    // Unparsable values keep the default instead of becoming NaN.
    let bad = crosshair::parse("citadel_crosshair_pip_width wide").unwrap();
    assert_eq!(bad.width, CrosshairConfig::default().width);
}

#[test]
fn apply_writes_missing_file_and_dir() {
    let (_d, paths) = fixture();
    assert!(!paths.autoexec.parent().unwrap().exists());
    assert_eq!(crosshair::load(&paths).unwrap(), None);
    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert!(text.contains(REFERENCE));
    assert!(text.ends_with('\n'));
    assert_eq!(crosshair::load(&paths).unwrap(), Some(custom()));
}

#[test]
fn apply_preserves_user_lines() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    let user = "// my binds\nbind mouse4 \"+jump\"\nfps_max 240\n\n// trailing comment";
    fs::write(&paths.autoexec, user).unwrap();
    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert!(text.starts_with(user), "{text}");
    assert!(text.contains(REFERENCE));
    // Applying again leaves the user's part exactly as it was.
    crosshair::apply(&paths, &CrosshairConfig::default()).unwrap();
    let again = fs::read_to_string(&paths.autoexec).unwrap();
    assert!(again.starts_with(user), "{again}");
    assert!(!again.contains(REFERENCE));
    assert!(again.contains(REFERENCE_DEFAULT));
    // No temp file is left behind.
    let names: Vec<_> = fs::read_dir(paths.autoexec.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("autoexec.cfg")]);
}

#[test]
fn apply_replaces_previous_managed_section() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    fs::write(&paths.autoexec, "fps_max 240\n").unwrap();
    crosshair::apply(&paths, &CrosshairConfig::default()).unwrap();
    // A user line added after the managed block stays after it.
    let mut text = fs::read_to_string(&paths.autoexec).unwrap();
    text.push_str("bind f1 noclip\n");
    fs::write(&paths.autoexec, &text).unwrap();

    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert_eq!(text.matches(crosshair::SECTION_START).count(), 1, "{text}");
    assert_eq!(text.matches(crosshair::SECTION_END).count(), 1, "{text}");
    assert_eq!(text.matches("citadel_crosshair_color_r").count(), 1);
    assert!(text.contains(REFERENCE));
    assert!(text.starts_with("fps_max 240\n"));
    assert!(text.ends_with("bind f1 noclip\n"), "{text}");
    let start = text.find(crosshair::SECTION_START).unwrap();
    assert!(start < text.find("bind f1 noclip").unwrap());
}

#[test]
fn apply_takes_over_the_old_apps_section_and_merges_duplicates() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    let old = "fps_max 240\n\
        // === Deadlock Mod Manager - Crosshair Settings (DO NOT EDIT) ===\n\
        citadel_crosshair_color_r 1; citadel_crosshair_pip_gap 7\n\
        // === End Crosshair Settings ===\n\
        bind f1 noclip\n";
    fs::write(&paths.autoexec, old).unwrap();
    // The old app's block is read when Shiv has none of its own.
    let loaded = crosshair::load(&paths).unwrap().unwrap();
    assert_eq!((loaded.color.r, loaded.gap), (1, 7.0));

    // Two Shiv blocks (e.g. pasted twice by hand) collapse into one.
    let doubled = format!(
        "{old}{s}\nx\n{e}\n{s}\ny\n{e}\n",
        s = crosshair::SECTION_START,
        e = crosshair::SECTION_END
    );
    fs::write(&paths.autoexec, doubled).unwrap();
    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert_eq!(text.matches(crosshair::SECTION_START).count(), 1, "{text}");
    assert!(!text.contains("Deadlock Mod Manager - Crosshair"), "{text}");
    assert_eq!(text.matches("citadel_crosshair_color_r").count(), 1);
    assert!(text.starts_with("fps_max 240\n"));
    assert!(text.contains("bind f1 noclip\n"));
    assert_eq!(crosshair::load(&paths).unwrap(), Some(custom()));
}

#[test]
fn apply_keeps_crlf_line_endings() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    fs::write(&paths.autoexec, "fps_max 240\r\nbind x jump\r\n").unwrap();
    crosshair::apply(&paths, &custom()).unwrap();
    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert!(
        text.starts_with("fps_max 240\r\nbind x jump\r\n"),
        "{text:?}"
    );
    assert_eq!(text.matches('\n').count(), text.matches("\r\n").count());
    assert_eq!(text.matches(crosshair::SECTION_START).count(), 1);
    assert_eq!(crosshair::load(&paths).unwrap(), Some(custom()));
}

#[test]
fn unterminated_section_is_replaced_to_the_end() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    let broken = format!(
        "fps_max 240\n{}\ncitadel_crosshair_pip_gap 9\n",
        crosshair::SECTION_START
    );
    fs::write(&paths.autoexec, broken).unwrap();
    crosshair::apply(&paths, &custom()).unwrap();
    let text = fs::read_to_string(&paths.autoexec).unwrap();
    assert!(!text.contains("pip_gap 9"), "{text}");
    assert_eq!(text.matches(crosshair::SECTION_END).count(), 1);
    assert!(text.starts_with("fps_max 240\n"));
}

#[test]
fn non_utf8_autoexec_is_left_alone() {
    let (_d, paths) = fixture();
    fs::create_dir_all(paths.autoexec.parent().unwrap()).unwrap();
    let bytes = b"bind x \xff\xfe\n".to_vec();
    fs::write(&paths.autoexec, &bytes).unwrap();
    assert!(crosshair::apply(&paths, &custom()).is_err());
    assert_eq!(fs::read(&paths.autoexec).unwrap(), bytes);
}
