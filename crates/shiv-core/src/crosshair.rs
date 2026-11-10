//! Crosshair settings, stored where the game's own crosshair menu keeps
//! them: Steam's per-user `citadel_hero_settings.lst`.
//!
//! The file is KV3 text under `userdata/<account>/1422450/remote/cfg/`; the
//! crosshair is a set of `crosshair_*` keys in its `base` object. The game
//! reads it at launch and writes it when the player changes the crosshair in
//! game, so a crosshair applied here is a starting point the player can still
//! change in game (an `autoexec.cfg` crosshair would override that at every
//! launch). Like the old app (`crosshair_settings.rs`), Shiv also writes the
//! copy in `game/citadel/cfg/` and removes the crosshair blocks older
//! versions put in `autoexec.cfg`.
//!
//! `to_commands` gives the same crosshair as console commands, byte for byte
//! as `generateConfigString` in the old app's `config-generator.ts`.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use kv_parser::{KeyValuesValue, KvDocument};

use crate::game::GamePaths;
use crate::hero_settings::HeroSettings;
use crate::platform;

pub use crate::hero_settings::EMPTY_SETTINGS;

/// Steam app id of Deadlock: its folder under `userdata/<account>`.
const DEADLOCK_APP_ID: &str = "1422450";
/// The settings file, in Steam userdata and in the game's `cfg` folder.
pub const SETTINGS_FILE: &str = "citadel_hero_settings.lst";
const BACKUP_SUFFIX: &str = ".bak";

/// Crosshair blocks earlier versions (Shiv's first crosshair release and the
/// old Deadlock Mod Manager) wrote to `autoexec.cfg`. `apply` removes them so
/// they no longer override the hero settings at every launch.
const AUTOEXEC_BLOCKS: [(&str, &str); 2] = [
    (
        "// === Shiv - Crosshair Settings (DO NOT EDIT) ===",
        "// === End Shiv Crosshair Settings ===",
    ),
    (
        "// === Deadlock Mod Manager - Crosshair Settings (DO NOT EDIT) ===",
        "// === End Crosshair Settings ===",
    ),
];

/// Value ranges from the old app's `crosshairConfigSchema` (gap steps by 1,
/// the others by 0.1).
pub const GAP_RANGE: (f32, f32) = (-20.0, 50.0);
pub const SIZE_RANGE: (f32, f32) = (0.0, 100.0);
pub const OPACITY_RANGE: (f32, f32) = (0.0, 1.0);

/// Gap the game draws for the default hero (`calculator.ts`, hero "Default"):
/// a base offset of 11, then 1 per step at or below 0 and 2 per step above.
const BASE_GAP: f32 = 11.0;
/// Base offset when the gap is static (`STATIC_GAP_BASE`).
const STATIC_GAP_BASE: f32 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// One crosshair, as the game's convars describe it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrosshairConfig {
    pub color: Rgb,
    pub pip_border: bool,
    pub pip_gap_static: bool,
    pub pip_opacity: f32,
    pub width: f32,
    pub height: f32,
    pub gap: f32,
    pub dot_opacity: f32,
    pub dot_outline_opacity: f32,
}

impl Default for CrosshairConfig {
    /// `DEFAULT_CROSSHAIR_CONFIG` from the old app.
    fn default() -> Self {
        Self {
            color: Rgb {
                r: 255,
                g: 255,
                b: 255,
            },
            pip_border: false,
            pip_gap_static: false,
            pip_opacity: 1.0,
            width: 4.0,
            height: 16.0,
            gap: 0.0,
            dot_opacity: 1.0,
            dot_outline_opacity: 1.0,
        }
    }
}

impl CrosshairConfig {
    /// Distance in game pixels from the centre to the middle of each pip
    /// (`calculateGap` for the default hero).
    pub fn gap_px(&self) -> f32 {
        if self.pip_gap_static {
            return STATIC_GAP_BASE + self.gap;
        }
        let increment = if self.gap <= 0.0 { 1.0 } else { 2.0 };
        BASE_GAP + self.gap * increment
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CrosshairError {
    #[error("Steam was not found. Install Steam and sign in once, then try again")]
    SteamNotFound,
    #[error("no Steam account has signed in on this computer. Sign in to Steam, then try again")]
    NoSteamAccount,
    #[error("{} is not plain text, so Shiv left it unchanged", .0.display())]
    NotText(PathBuf),
    #[error("{} is not a crosshair settings file Shiv can read, so Shiv left it unchanged", .0.display())]
    InvalidSettings(PathBuf),
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{error}; {} could not be put back as it was", path.display())]
    NotRestored {
        error: Box<CrosshairError>,
        path: PathBuf,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> CrosshairError {
    let path = path.to_path_buf();
    move |source| CrosshairError::Io {
        action,
        path,
        source,
    }
}

// ---------------------------------------------------------------------------
// Values

/// Format a number the way JavaScript's template literals do for the values
/// the editor produces: shortest round-trip form, no trailing `.0`, `-0` as `0`.
fn js_number(v: f32) -> String {
    if v == 0.0 || !v.is_finite() {
        return "0".into();
    }
    v.to_string()
}

/// The console commands for `c`, byte for byte as `generateConfigString`.
pub fn to_commands(c: &CrosshairConfig) -> String {
    format!(
        "citadel_crosshair_color_r {}; citadel_crosshair_color_g {}; citadel_crosshair_color_b {}; \
         citadel_crosshair_pip_outline_border {}; citadel_crosshair_pip_gap_static {}; \
         citadel_crosshair_pip_opacity {}; citadel_crosshair_pip_width {}; \
         citadel_crosshair_pip_height {}; citadel_crosshair_pip_gap {}; \
         citadel_crosshair_dot_opacity {}; citadel_crosshair_dot_outline_opacity {}",
        c.color.r,
        c.color.g,
        c.color.b,
        u8::from(c.pip_border),
        c.pip_gap_static,
        js_number(c.pip_opacity),
        js_number(c.width),
        js_number(c.height),
        js_number(c.gap),
        js_number(c.dot_opacity),
        js_number(c.dot_outline_opacity),
    )
}

/// The hero-settings keys for `c` with their quoted values, as the old app's
/// `CrosshairConfig::settings` writes them. `crosshair_themed = false` makes
/// the game use these values instead of each hero's themed crosshair.
fn settings(c: &CrosshairConfig) -> BTreeMap<String, String> {
    [
        ("crosshair_themed", "false".to_string()),
        ("crosshair_color_r", c.color.r.to_string()),
        ("crosshair_color_g", c.color.g.to_string()),
        ("crosshair_color_b", c.color.b.to_string()),
        (
            "crosshair_pip_outline_border",
            u8::from(c.pip_border).to_string(),
        ),
        ("crosshair_pip_gap_static", c.pip_gap_static.to_string()),
        ("crosshair_pip_opacity", js_number(c.pip_opacity)),
        ("crosshair_pip_width", js_number(c.width)),
        ("crosshair_pip_height", js_number(c.height)),
        ("crosshair_pip_gap", js_number(c.gap)),
        ("crosshair_dot_opacity", js_number(c.dot_opacity)),
        (
            "crosshair_dot_outline_opacity",
            js_number(c.dot_outline_opacity),
        ),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), format!("\"{value}\"")))
    .collect()
}

fn number(value: &str) -> Option<f32> {
    value.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// `parseInt` semantics for colours: leading integer part, clamped to 0..=255.
fn channel(value: &str) -> Option<u8> {
    let end = value
        .char_indices()
        .find(|&(i, ch)| !(ch.is_ascii_digit() || (i == 0 && (ch == '-' || ch == '+'))))
        .map_or(value.len(), |(i, _)| i);
    let n: i64 = value[..end].parse().ok()?;
    Some(u8::try_from(n.clamp(0, 255)).unwrap_or(u8::MAX))
}

fn truthy(value: &str) -> bool {
    value == "true" || number(value).is_some_and(|v| v > 0.0)
}

fn set<T>(slot: &mut T, value: Option<T>) {
    if let Some(v) = value {
        *slot = v;
    }
}

/// The crosshair in a parsed settings file, starting from the defaults for
/// keys it lacks. `None` when it has no crosshair key at all. Values may be
/// quoted or bare; an unparsable number keeps the default.
fn read_crosshair(s: &HeroSettings<'_>) -> Result<Option<CrosshairConfig>, ()> {
    let mut c = CrosshairConfig::default();
    let mut found = false;
    for key in settings(&c).keys() {
        let Some(raw) = s.get(key).map_err(|_| ())? else {
            continue;
        };
        let value = raw.trim_matches('"');
        match key.as_str() {
            "crosshair_color_r" => set(&mut c.color.r, channel(value)),
            "crosshair_color_g" => set(&mut c.color.g, channel(value)),
            "crosshair_color_b" => set(&mut c.color.b, channel(value)),
            "crosshair_pip_outline_border" => c.pip_border = truthy(value),
            "crosshair_pip_gap_static" => c.pip_gap_static = truthy(value),
            "crosshair_pip_opacity" => set(&mut c.pip_opacity, number(value)),
            "crosshair_pip_width" => set(&mut c.width, number(value)),
            "crosshair_pip_height" => set(&mut c.height, number(value)),
            "crosshair_pip_gap" => set(&mut c.gap, number(value)),
            "crosshair_dot_opacity" => set(&mut c.dot_opacity, number(value)),
            "crosshair_dot_outline_opacity" => set(&mut c.dot_outline_opacity, number(value)),
            _ => continue,
        }
        found = true;
    }
    Ok(found.then_some(c))
}

// ---------------------------------------------------------------------------
// Steam

/// The Steam installation: steamlocate first, then the usual folders.
pub fn locate_steam() -> Result<PathBuf, CrosshairError> {
    if let Ok(steam) = steamlocate::SteamDir::locate() {
        return Ok(steam.path().to_path_buf());
    }
    platform::default_steam_dirs()
        .into_iter()
        .find(|dir| dir.join("userdata").is_dir())
        .ok_or(CrosshairError::SteamNotFound)
}

fn mtime_secs(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

fn kv_text(value: &KeyValuesValue) -> Option<String> {
    match value {
        KeyValuesValue::String(s) => Some(s.clone()),
        KeyValuesValue::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// One remembered account from `config/loginusers.vdf`.
struct Remembered {
    account_id: u32,
    most_recent: bool,
    auto_login: bool,
    timestamp: u64,
}

fn remembered_accounts(steam: &Path) -> Vec<Remembered> {
    let Ok(text) = fs::read_to_string(steam.join("config").join("loginusers.vdf")) else {
        return Vec::new();
    };
    let mut doc = KvDocument::new();
    if doc.load_from_string(&text).is_err() {
        return Vec::new();
    }
    let users = doc.get_data().iter().find_map(|(key, value)| match value {
        KeyValuesValue::Object(users) if key.eq_ignore_ascii_case("users") => Some(users),
        _ => None,
    });
    let Some(users) = users else {
        return Vec::new();
    };
    users
        .iter()
        .filter_map(|(id, user)| {
            let steam_id64: u64 = id.parse().ok()?;
            let KeyValuesValue::Object(fields) = user else {
                return None;
            };
            let field = |name: &str| {
                fields
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .and_then(|(_, v)| kv_text(v))
            };
            Some(Remembered {
                // The low 32 bits of a SteamID64 are the account id.
                account_id: u32::try_from(steam_id64 & 0xFFFF_FFFF).ok()?,
                most_recent: field("MostRecent").as_deref() == Some("1"),
                auto_login: field("AutoLogin").as_deref() == Some("1"),
                timestamp: field("Timestamp").and_then(|t| t.parse().ok()).unwrap_or(0),
            })
        })
        .collect()
}

/// `userdata/<account>/1422450` for the account signed in to Steam, chosen
/// like the old app's `steam_user::rank_accounts`: among the accounts in
/// `loginusers.vdf`, the one Steam touched last (the newer of its
/// `localconfig.vdf` mtime and its login timestamp), then the `MostRecent`
/// and `AutoLogin` flags, then having Deadlock data, then the lowest id.
///
/// When `loginusers.vdf` is missing or lists nobody, the account whose
/// Deadlock settings file changed last is used.
pub fn user_dir(steam: &Path) -> Result<PathBuf, CrosshairError> {
    let userdata = steam.join("userdata");
    let app_dir = |id: u32| userdata.join(id.to_string()).join(DEADLOCK_APP_ID);
    let ranked = remembered_accounts(steam)
        .into_iter()
        .map(|a| {
            let session = mtime_secs(
                &userdata
                    .join(a.account_id.to_string())
                    .join("config")
                    .join("localconfig.vdf"),
            );
            let last_seen = session
                .into_iter()
                .chain((a.timestamp > 0).then_some(a.timestamp))
                .max();
            let key = (
                last_seen,
                a.most_recent,
                a.auto_login,
                app_dir(a.account_id).is_dir(),
                std::cmp::Reverse(a.account_id),
            );
            (key, a.account_id)
        })
        .max_by_key(|(key, _)| *key)
        .map(|(_, id)| id);
    if let Some(id) = ranked {
        return Ok(app_dir(id));
    }
    let entries = fs::read_dir(&userdata).map_err(|_| CrosshairError::NoSteamAccount)?;
    entries
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&id| app_dir(id).is_dir())
        .max_by_key(|&id| {
            (
                mtime_secs(&user_settings_path(&app_dir(id))),
                std::cmp::Reverse(id),
            )
        })
        .map(app_dir)
        .ok_or(CrosshairError::NoSteamAccount)
}

/// The settings file in a `userdata/<account>/1422450` folder.
pub fn user_settings_path(user_dir: &Path) -> PathBuf {
    user_dir.join("remote").join("cfg").join(SETTINGS_FILE)
}

/// The settings file in the game's `cfg` folder.
pub fn game_settings_path(paths: &GamePaths) -> PathBuf {
    paths
        .root
        .join("game")
        .join("citadel")
        .join("cfg")
        .join(SETTINGS_FILE)
}

/// Where the Steam copy's backup goes: `local`, beside the cloud-synced
/// `remote` folder, so Steam Cloud does not pick it up.
fn user_backup_path(user_dir: &Path) -> PathBuf {
    user_dir
        .join("local")
        .join(format!("{SETTINGS_FILE}{BACKUP_SUFFIX}"))
}

fn sibling_backup(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Files

/// A symlinked file is read and written through to its target.
fn resolve(path: &Path) -> Result<PathBuf, CrosshairError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(io_err("resolve", path))
        }
        _ => Ok(path.to_path_buf()),
    }
}

/// Read `path`; a missing file is `None`. Non-UTF-8 content is refused so a
/// rewrite can never mangle it.
fn read_text(path: &Path) -> Result<Option<String>, CrosshairError> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| CrosshairError::NotText(path.to_path_buf())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err("read", path)(e)),
    }
}

/// Write `contents` to `path` through a temp file in the same folder, so the
/// game never reads a half-written file. An existing file's permissions are
/// carried over.
fn write_atomic(path: &Path, contents: &str) -> Result<(), CrosshairError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err("create", dir))?;
    let name = path
        .file_name()
        .map_or_else(|| "settings".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.shiv-{}.tmp", std::process::id()));
    let permissions = fs::metadata(path).ok().map(|m| m.permissions());
    let written = fs::File::create(&tmp)
        .and_then(|mut f| {
            f.write_all(contents.as_bytes())?;
            f.sync_all()
        })
        .and_then(|()| match permissions {
            Some(p) => fs::set_permissions(&tmp, p),
            None => Ok(()),
        })
        .map_err(io_err("write", &tmp))
        .and_then(|()| fs::rename(&tmp, path).map_err(io_err("replace", path)));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// A settings file as read and the text to parse: a missing or blank file
/// reads as `EMPTY_SETTINGS`.
fn read_settings(path: &Path) -> Result<(Option<String>, String), CrosshairError> {
    let original = read_text(path)?;
    let text = match original.as_deref() {
        Some(t) if !t.trim().is_empty() => t.to_string(),
        _ => EMPTY_SETTINGS.to_string(),
    };
    Ok((original, text))
}

/// The crosshair in the signed-in account's settings (`user_dir` is its
/// `userdata/<account>/1422450`), or in the game's copy when Steam has none.
/// `None` when the file has no crosshair keys.
pub fn load(paths: &GamePaths, user_dir: &Path) -> Result<Option<CrosshairConfig>, CrosshairError> {
    let mut path = resolve(&user_settings_path(user_dir))?;
    if !path.exists() {
        path = resolve(&game_settings_path(paths))?;
    }
    let (_, text) = read_settings(&path)?;
    HeroSettings::parse(&text)
        .ok()
        .and_then(|s| read_crosshair(&s).ok())
        .ok_or(CrosshairError::InvalidSettings(path))
}

/// One file `apply` rewrites.
struct Change {
    path: PathBuf,
    backup: PathBuf,
    original: Option<String>,
    updated: String,
}

fn settings_change(
    path: &Path,
    backup: PathBuf,
    c: &CrosshairConfig,
) -> Result<Option<Change>, CrosshairError> {
    let path = resolve(path)?;
    let (original, text) = read_settings(&path)?;
    let Ok(updated) = HeroSettings::parse(&text).and_then(|s| s.update(&settings(c))) else {
        return Err(CrosshairError::InvalidSettings(path));
    };
    Ok(
        (original.as_deref() != Some(updated.as_str())).then_some(Change {
            path,
            backup,
            original,
            updated,
        }),
    )
}

/// `line` without its terminator, surrounding space, or a leading BOM.
fn marker_text(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

/// `text` without the complete crosshair blocks of `AUTOEXEC_BLOCKS`, or
/// `None` when it has none. A block is a start marker followed by its own end
/// marker with no other start marker in between; a lone start marker is left
/// alone. Every other line keeps its bytes and line ending.
fn strip_autoexec_blocks(text: &str) -> Option<String> {
    let segments: Vec<&str> = text.split_inclusive('\n').collect();
    let start_of = |line: &str| {
        AUTOEXEC_BLOCKS
            .iter()
            .position(|(start, _)| marker_text(line) == *start)
    };
    let mut out = String::new();
    let mut removed = false;
    let mut i = 0;
    while i < segments.len() {
        let close = start_of(segments[i]).and_then(|block| {
            let end = AUTOEXEC_BLOCKS[block].1;
            segments[i + 1..]
                .iter()
                .position(|l| start_of(l).is_some() || marker_text(l) == end)
                .map(|k| i + 1 + k)
                .filter(|&j| marker_text(segments[j]) == end)
        });
        match close {
            Some(j) => {
                if i == 0 && text.starts_with('\u{feff}') {
                    out.push('\u{feff}');
                }
                removed = true;
                i = j + 1;
            }
            None => {
                out.push_str(segments[i]);
                i += 1;
            }
        }
    }
    removed.then_some(out)
}

fn autoexec_change(paths: &GamePaths) -> Result<Option<Change>, CrosshairError> {
    let path = resolve(&paths.autoexec)?;
    // A file that is not text cannot hold a block Shiv wrote; leave it.
    let original = match read_text(&path) {
        Ok(Some(text)) => text,
        Ok(None) | Err(CrosshairError::NotText(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(strip_autoexec_blocks(&original).map(|updated| Change {
        backup: sibling_backup(&path),
        path,
        original: Some(original),
        updated,
    }))
}

/// Put `change.path` back as it was before `apply`.
fn restore(change: &Change) -> Result<(), CrosshairError> {
    match &change.original {
        Some(text) => write_atomic(&change.path, text),
        None => match fs::remove_file(&change.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(io_err("remove", &change.path)(e)),
            _ => Ok(()),
        },
    }
}

/// Write `c` to the signed-in account's settings (`user_dir`) and to the
/// game's copy, and drop old crosshair blocks from `autoexec.cfg`.
///
/// Everything is read and checked before anything is written: a settings
/// file Shiv cannot parse, or one that is not text, stops the apply with no
/// file changed. Each changed file is first copied to a `.bak` (the Steam
/// copy's goes to `local/`, outside Steam Cloud's `remote/`), then replaced
/// atomically with its permissions kept. Only the crosshair values change;
/// all other bytes stay. If a later write fails, the files already written
/// are put back.
///
/// This does not check for a running game: the caller decides (the UI
/// refuses while Deadlock runs, as the old app did).
pub fn apply(
    paths: &GamePaths,
    user_dir: &Path,
    c: &CrosshairConfig,
) -> Result<(), CrosshairError> {
    let game_file = game_settings_path(paths);
    let changes: Vec<Change> = [
        settings_change(&game_file, sibling_backup(&game_file), c)?,
        settings_change(&user_settings_path(user_dir), user_backup_path(user_dir), c)?,
        autoexec_change(paths)?,
    ]
    .into_iter()
    .flatten()
    .collect();
    for (i, change) in changes.iter().enumerate() {
        let written = change
            .original
            .as_deref()
            .map_or(Ok(()), |text| write_atomic(&change.backup, text))
            .and_then(|()| write_atomic(&change.path, &change.updated));
        if let Err(error) = written {
            for done in changes[..i].iter().rev() {
                if restore(done).is_err() {
                    return Err(CrosshairError::NotRestored {
                        error: Box::new(error),
                        path: done.path.clone(),
                    });
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_follows_the_default_hero_calculator() {
        let mut c = CrosshairConfig::default();
        assert_eq!(c.gap_px(), 11.0);
        c.gap = 3.0;
        assert_eq!(c.gap_px(), 17.0);
        c.gap = -3.0;
        assert_eq!(c.gap_px(), 8.0);
        c.pip_gap_static = true;
        assert_eq!(c.gap_px(), 1.0);
    }

    #[test]
    fn channels_parse_like_parse_int() {
        assert_eq!(channel("12.7"), Some(12));
        assert_eq!(channel("300"), Some(255));
        assert_eq!(channel("-4"), Some(0));
        assert_eq!(channel("x"), None);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let c = CrosshairConfig {
            color: Rgb { r: 1, g: 2, b: 3 },
            pip_border: true,
            pip_gap_static: true,
            pip_opacity: 0.3,
            width: 2.5,
            height: 0.0,
            gap: -7.0,
            dot_opacity: 0.0,
            dot_outline_opacity: 0.9,
        };
        let text = HeroSettings::parse(EMPTY_SETTINGS)
            .and_then(|s| s.update(&settings(&c)))
            .unwrap();
        let parsed = HeroSettings::parse(&text).unwrap();
        assert_eq!(read_crosshair(&parsed), Ok(Some(c)));
    }

    #[test]
    fn autoexec_blocks_are_removed_only_when_complete() {
        assert_eq!(strip_autoexec_blocks("bind x jump\n"), None);
        let (start, end) = AUTOEXEC_BLOCKS[0];
        let orphan = format!("{start}\nbind x jump\n");
        assert_eq!(strip_autoexec_blocks(&orphan), None);
        // An orphan before a real block does not claim the lines between.
        let text = format!("{start}\nmine\n{start}\nx\n{end}\nafter");
        assert_eq!(
            strip_autoexec_blocks(&text).as_deref(),
            Some(format!("{start}\nmine\nafter").as_str())
        );
        let bom = format!("\u{feff}{start}\nx\n{end}\nmine\n");
        assert_eq!(
            strip_autoexec_blocks(&bom).as_deref(),
            Some("\u{feff}mine\n")
        );
    }
}
