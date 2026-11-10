// SPDX-License-Identifier: GPL-3.0-only
//! Mounting the repair package through `gameinfo.gi`.
//!
//! Deadlock Mod Manager (GPL-3.0) loads its repair package from a folder of its
//! own under `citadel`, mounted by a `Game` search path ahead of the mods
//! (`mod_manager/manager/gameinfo.rs`, `localization.rs`). Shiv keeps that
//! design: one `Game citadel/shiv_repair` line placed directly before the
//! `Game citadel/addons` line, so repaired files win over the mods' own copies.
//! Search paths are read with `kv-parser` source sites, as in
//! `shiv_core::health`, and the edit is a single inserted line; nothing else
//! in the file changes.

use crate::error::RepairError;
use kv_parser::{ParseOptions, SourceDocument, SourceKind, SourceSite, line_end, line_start};
use shiv_core::health::{ADDONS_SEARCH_PATH, client_symbol};

/// Folder under `game/citadel` that holds the repair package.
pub const OVERLAY_DIR: &str = "shiv_repair";
/// Search path that mounts [`OVERLAY_DIR`].
pub const OVERLAY_SEARCH_PATH: &str = "citadel/shiv_repair";

fn parse(text: &str) -> Result<SourceDocument, RepairError> {
    // Source 2 reads backslashes literally in gameinfo.gi.
    SourceDocument::parse(
        text,
        ParseOptions {
            allow_escape_sequences: false,
            ..ParseOptions::default()
        },
    )
    .map_err(|error| RepairError::Gameinfo(format!("gameinfo.gi could not be parsed: {error}")))
}

/// The value of an active `Game` (or `Game+...`) search path entry, if `site`
/// is one.
fn active_game_path(site: &SourceSite) -> Result<Option<&str>, RepairError> {
    let [root, fs_key, sp, role] = site.path.as_slice() else {
        return Ok(None);
    };
    let SourceKind::Scalar { value, .. } = &site.kind else {
        return Ok(None);
    };
    let is_game = role.split('+').any(|r| r.eq_ignore_ascii_case("Game"));
    if !(root.eq_ignore_ascii_case("GameInfo")
        && fs_key.eq_ignore_ascii_case("FileSystem")
        && sp.eq_ignore_ascii_case("SearchPaths")
        && is_game)
    {
        return Ok(None);
    }
    let applies = site
        .applies(client_symbol)
        .map_err(|error| RepairError::Gameinfo(format!("gameinfo.gi condition: {error}")))?;
    Ok(applies.then(|| value.trim_end_matches('/')))
}

/// True when `text` mounts the repair package.
pub fn mounts_overlay(text: &str) -> Result<bool, RepairError> {
    let doc = parse(text)?;
    for site in &doc.sites {
        if active_game_path(site)?.is_some_and(|p| p.eq_ignore_ascii_case(OVERLAY_SEARCH_PATH)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `text` with the repair package mounted directly before `citadel/addons`,
/// or `None` when it is already mounted. Fails when `citadel/addons` is not
/// mounted: then no mod loads and the package would have nothing to repair.
pub fn with_overlay(text: &str) -> Result<Option<String>, RepairError> {
    let doc = parse(text)?;
    let mut addons = None;
    for site in &doc.sites {
        let Some(path) = active_game_path(site)? else {
            continue;
        };
        if path.eq_ignore_ascii_case(OVERLAY_SEARCH_PATH) {
            return Ok(None);
        }
        if addons.is_none() && path.eq_ignore_ascii_case(ADDONS_SEARCH_PATH) {
            addons = Some(site.key_start);
        }
    }
    let Some(key_start) = addons else {
        return Err(RepairError::Gameinfo(format!(
            "gameinfo.gi has no active `Game {ADDONS_SEARCH_PATH}` search path, so mods do not load"
        )));
    };
    let start = line_start(text, key_start);
    let indent = &text[start..key_start];
    let mut out = String::with_capacity(text.len() + 64);
    if indent.chars().all(char::is_whitespace) {
        let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
        out.push_str(&text[..start]);
        out.push_str(indent);
        out.push_str("Game                ");
        out.push_str(OVERLAY_SEARCH_PATH);
        out.push_str(newline);
        out.push_str(&text[start..]);
    } else {
        // The addons entry shares its line with other tokens.
        out.push_str(&text[..key_start]);
        out.push_str("Game ");
        out.push_str(OVERLAY_SEARCH_PATH);
        out.push(' ');
        out.push_str(&text[key_start..]);
    }
    Ok(Some(out))
}

/// `text` without the repair package's `Game citadel/shiv_repair` mount, or
/// `None` when it has none. The reverse of [`with_overlay`]: a line holding
/// only the mount is removed with its line ending; a mount sharing its line
/// with other tokens loses just its key, value and the space after them.
/// Everything else (comments, other entries, line endings, a BOM) stays as it
/// is. Conditional entries (`[$...]`) are left alone: Shiv never writes them.
pub fn without_overlay(text: &str) -> Result<Option<String>, RepairError> {
    let doc = parse(text)?;
    let mut cuts: Vec<std::ops::Range<usize>> = Vec::new();
    for site in &doc.sites {
        if !site.is_unconditional() {
            continue;
        }
        let Some(path) = active_game_path(site)? else {
            continue;
        };
        if !path.eq_ignore_ascii_case(OVERLAY_SEARCH_PATH) {
            continue;
        }
        let SourceKind::Scalar { value_range, .. } = &site.kind else {
            continue;
        };
        let start = line_start(text, site.key_start);
        let end = line_end(text, value_range.end);
        let before = &text[start..site.key_start];
        let after = &text[value_range.end..end];
        if before.chars().all(char::is_whitespace) && after.trim().is_empty() {
            cuts.push(start..end);
        } else {
            let rest = &text[value_range.end..];
            let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
            cuts.push(site.key_start..value_range.end + spaces);
        }
    }
    if cuts.is_empty() {
        return Ok(None);
    }
    cuts.sort_by_key(|cut| cut.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for cut in cuts {
        if cut.start < at {
            continue;
        }
        out.push_str(&text[at..cut.start]);
        at = cut.end;
    }
    out.push_str(&text[at..]);
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOCK: &str = "\"GameInfo\"\r\n{\r\n\tFileSystem\r\n\t{\r\n\t\tSearchPaths\r\n\t\t{\r\n\t\t\tGame                citadel/addons\r\n\t\t\tMod                 citadel\r\n\t\t\tGame                citadel\r\n\t\t\tGame                core\r\n\t\t}\r\n\t}\r\n}\r\n";

    #[test]
    fn inserts_one_line_before_addons_and_keeps_line_endings() {
        let edited = with_overlay(STOCK).unwrap().unwrap();
        assert_eq!(
            edited,
            STOCK.replace(
                "\t\t\tGame                citadel/addons",
                "\t\t\tGame                citadel/shiv_repair\r\n\t\t\tGame                citadel/addons"
            )
        );
        assert!(mounts_overlay(&edited).unwrap());
        assert!(!mounts_overlay(STOCK).unwrap());
        assert_eq!(with_overlay(&edited).unwrap(), None);
    }

    #[test]
    fn skips_inactive_addons_lines_and_handles_one_line_files() {
        let text = "GameInfo { FileSystem { SearchPaths { Game citadel/addons [$LINUX] Game citadel/addons Game core } } }";
        let edited = with_overlay(text).unwrap().unwrap();
        assert_eq!(
            edited,
            "GameInfo { FileSystem { SearchPaths { Game citadel/addons [$LINUX] Game citadel/shiv_repair Game citadel/addons Game core } } }"
        );
        assert!(mounts_overlay(&edited).unwrap());
    }

    #[test]
    fn without_overlay_reverses_with_overlay_byte_for_byte() {
        let bom = format!("\u{feff}{STOCK}");
        let lf = STOCK.replace("\r\n", "\n");
        for text in [STOCK, bom.as_str(), lf.as_str()] {
            let mounted = with_overlay(text).unwrap().unwrap();
            assert_eq!(without_overlay(&mounted).unwrap().as_deref(), Some(text));
            assert_eq!(without_overlay(text).unwrap(), None);
        }
        let one_line = "GameInfo { FileSystem { SearchPaths { Game citadel/addons Game core } } }";
        let mounted = with_overlay(one_line).unwrap().unwrap();
        assert_eq!(
            without_overlay(&mounted).unwrap().as_deref(),
            Some(one_line)
        );
    }

    #[test]
    fn without_overlay_keeps_everything_else() {
        // Edited elsewhere after the mount: a comment, a ConVar, a moved line.
        let text = "\"GameInfo\"\n{\n\t// mine\n\tFileSystem\n\t{\n\t\tSearchPaths\n\t\t{\n\t\t\tGame citadel/shiv_repair\n\t\t\tGame citadel/addons // keep\n\t\t\tGame core\n\t\t}\n\t}\n\tConVars { fps_max 240 }\n}\n";
        let expected = text.replace("\t\t\tGame citadel/shiv_repair\n", "");
        assert_eq!(without_overlay(text).unwrap(), Some(expected));
        // A conditional entry was not written by Shiv and stays.
        let conditional = "GameInfo { FileSystem { SearchPaths { Game citadel/shiv_repair [$LINUX] Game core } } }";
        assert_eq!(without_overlay(conditional).unwrap(), None);
    }

    #[test]
    fn refuses_without_addons_mount() {
        let text = "GameInfo { FileSystem { SearchPaths { // Game citadel/addons\n Game core } } }";
        assert!(matches!(with_overlay(text), Err(RepairError::Gameinfo(_))));
        assert!(matches!(
            with_overlay("GameInfo {"),
            Err(RepairError::Gameinfo(_))
        ));
    }
}
