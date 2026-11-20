//! Restoring the `Game citadel/addons` mount in `gameinfo.gi`.
//!
//! Valve's stock `gameinfo.gi` does not mount `citadel/addons`, so a game
//! update that rewrites the file turns every mod off. [`fix_addons_mount`]
//! inserts the `Game citadel/addons` line directly before the stock
//! `Game citadel` entry (so mods win over base files), plus `Mod citadel` and
//! `Write citadel`. Without those two, the engine takes the first `Game` path
//! (the addons folder) as its `MOD` and write path, can no longer find
//! `cfg/user_keys_default.vcfg`, and aborts at launch. Nothing else changes.

use std::fs;
use std::io;

use kv_parser::{ParseOptions, SourceDocument, SourceKind, line_start};

use crate::game::GamePaths;
use crate::health::{ADDONS_SEARCH_PATH, client_symbol, mounts_addons};
use crate::vanilla;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// `text` with `Game citadel/addons` mounted, or `None` when it already is.
/// Fails when the file cannot be parsed or has no active `Game citadel` entry
/// to anchor the line on.
pub fn with_addons_mount(text: &str) -> io::Result<Option<String>> {
    let parse_error =
        |e: kv_parser::KvError| invalid(format!("gameinfo.gi could not be parsed: {e}"));
    if mounts_addons(text).map_err(parse_error)? {
        return Ok(None);
    }
    // Source 2 reads backslashes literally in gameinfo.gi.
    let doc = SourceDocument::parse(
        text,
        ParseOptions {
            allow_escape_sequences: false,
            ..ParseOptions::default()
        },
    )
    .map_err(parse_error)?;
    let mut anchor = None;
    for site in &doc.sites {
        let [root, fs_key, sp, role] = site.path.as_slice() else {
            continue;
        };
        let SourceKind::Scalar { value, .. } = &site.kind else {
            continue;
        };
        if root.eq_ignore_ascii_case("GameInfo")
            && fs_key.eq_ignore_ascii_case("FileSystem")
            && sp.eq_ignore_ascii_case("SearchPaths")
            && role.split('+').any(|r| r.eq_ignore_ascii_case("Game"))
            && value.trim_end_matches('/').eq_ignore_ascii_case("citadel")
            && site
                .applies(client_symbol)
                .map_err(|e| invalid(format!("gameinfo.gi condition: {e}")))?
        {
            anchor = Some(site.key_start);
            break;
        }
    }
    let Some(key_start) = anchor else {
        return Err(invalid(
            "gameinfo.gi has no active `Game citadel` search path to place the addons mount before",
        ));
    };
    let start = line_start(text, key_start);
    let indent = &text[start..key_start];
    let mut out = String::with_capacity(text.len() + 48);
    if indent.chars().all(char::is_whitespace) {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        out.push_str(&text[..start]);
        for (key, path) in [
            ("Game", ADDONS_SEARCH_PATH),
            ("Mod", "citadel"),
            ("Write", "citadel"),
        ] {
            out.push_str(indent);
            out.push_str(&format!("{key:<20}{path}"));
            out.push_str(newline);
        }
        out.push_str(&text[start..]);
    } else {
        // The entry shares its line with other tokens.
        out.push_str(&text[..key_start]);
        out.push_str("Game ");
        out.push_str(ADDONS_SEARCH_PATH);
        out.push_str(" Mod citadel Write citadel ");
        out.push_str(&text[key_start..]);
    }
    Ok(Some(out))
}

/// Mount `citadel/addons` in the game's `gameinfo.gi`. `Ok(false)` when it
/// was already mounted. Refused while a vanilla launch session is active, which
/// leaves the mount out on purpose.
pub fn fix_addons_mount(paths: &GamePaths) -> io::Result<bool> {
    if vanilla::is_active(paths) {
        return Err(invalid(
            "mods are turned off for a vanilla session; close the game first",
        ));
    }
    let text = fs::read_to_string(&paths.gameinfo)?;
    let Some(fixed) = with_addons_mount(&text)? else {
        return Ok(false);
    };
    vanilla::write_replace(&paths.gameinfo, &fixed)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOCK: &str = "\"GameInfo\"\r\n{\r\n\tFileSystem\r\n\t{\r\n\t\tSearchPaths\r\n\t\t{\r\n\t\t\tGame_LowViolence\tcitadel_lv\r\n\r\n\t\t\tGame\t\t\t\tcitadel\r\n\t\t\tGame\t\t\t\tcore\r\n\t\t}\r\n\t}\r\n}\r\n";

    #[test]
    fn inserts_one_line_before_citadel_keeping_crlf() {
        let out = with_addons_mount(STOCK).unwrap().unwrap();
        assert_eq!(
            out,
            STOCK.replace(
                "\t\t\tGame\t\t\t\tcitadel\r\n",
                "\t\t\tGame                citadel/addons\r\n\t\t\tMod                 citadel\r\n\t\t\tWrite               citadel\r\n\t\t\tGame\t\t\t\tcitadel\r\n"
            )
        );
        assert_eq!(with_addons_mount(&out).unwrap(), None);
    }

    #[test]
    fn handles_a_shared_line_and_skips_conditional_anchors() {
        let one = "GameInfo { FileSystem { SearchPaths { Game citadel Game core } } }";
        assert_eq!(
            with_addons_mount(one).unwrap().unwrap(),
            "GameInfo { FileSystem { SearchPaths { Game citadel/addons Mod citadel Write citadel Game citadel Game core } } }"
        );
        let conditional =
            "GameInfo { FileSystem { SearchPaths { Game citadel [!$WIN64]\n Game core } } }";
        assert!(with_addons_mount(conditional).is_err());
    }

    #[test]
    fn a_disabled_mount_does_not_count() {
        let text =
            "GameInfo { FileSystem { SearchPaths { // Game citadel/addons\n Game citadel } } }";
        assert!(with_addons_mount(text).unwrap().is_some());
    }

    #[test]
    fn fix_writes_once_and_refuses_during_a_vanilla_session() {
        let dir = tempfile::tempdir().unwrap();
        let paths = GamePaths::from_root(dir.path().to_path_buf());
        fs::create_dir_all(paths.gameinfo.parent().unwrap()).unwrap();
        fs::write(&paths.gameinfo, STOCK).unwrap();
        assert!(fix_addons_mount(&paths).unwrap());
        assert!(!fix_addons_mount(&paths).unwrap());
        assert!(
            fs::read_to_string(&paths.gameinfo)
                .unwrap()
                .contains("citadel/addons")
        );

        fs::write(&paths.gameinfo, STOCK).unwrap();
        fs::write(vanilla::backup_path(&paths), "x").unwrap();
        assert!(fix_addons_mount(&paths).is_err());
        assert_eq!(fs::read_to_string(&paths.gameinfo).unwrap(), STOCK);
    }
}
