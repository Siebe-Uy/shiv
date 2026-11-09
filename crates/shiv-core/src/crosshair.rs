//! Crosshair settings: the `citadel_crosshair_*` console commands and the
//! managed block Shiv keeps in `autoexec.cfg`.
//!
//! `to_commands` and `parse` follow `generateConfigString` and
//! `parseConfigString` in the old app's `packages/crosshair/src/config-generator.ts`.
//! `apply` writes the commands between Shiv's markers in `autoexec.cfg`,
//! which Deadlock runs at every launch; everything outside the block is the
//! user's and is kept byte for byte.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::game::GamePaths;

/// First line of the block Shiv owns in `autoexec.cfg`.
pub const SECTION_START: &str = "// === Shiv - Crosshair Settings (DO NOT EDIT) ===";
/// Last line of the block Shiv owns in `autoexec.cfg`.
pub const SECTION_END: &str = "// === End Shiv Crosshair Settings ===";

/// The block the old Deadlock Mod Manager wrote. Shiv reads it when it has
/// none of its own and replaces it on apply, so the file keeps one crosshair.
const LEGACY_START: &str = "// === Deadlock Mod Manager - Crosshair Settings (DO NOT EDIT) ===";
const LEGACY_END: &str = "// === End Crosshair Settings ===";

const SECTIONS: [(&str, &str); 2] = [(SECTION_START, SECTION_END), (LEGACY_START, LEGACY_END)];

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
    #[error("{} is not plain text, so Shiv left it unchanged", .0.display())]
    NotText(PathBuf),
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
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

/// Read crosshair commands, starting from the defaults like
/// `parseConfigString`. Returns `None` when no crosshair command is present.
///
/// Beyond the TS version it also splits on newlines, strips quotes around
/// values, accepts `1` for the boolean convars, and keeps the default for an
/// unparsable number instead of storing NaN.
pub fn parse(s: &str) -> Option<CrosshairConfig> {
    let mut c = CrosshairConfig::default();
    let mut found = false;
    for command in s.split([';', '\n', '\r']) {
        let mut parts = command.split_whitespace();
        let (Some(key), Some(raw)) = (parts.next(), parts.next()) else {
            continue;
        };
        let value = raw.trim_matches('"');
        match key {
            "citadel_crosshair_color_r" => set(&mut c.color.r, channel(value)),
            "citadel_crosshair_color_g" => set(&mut c.color.g, channel(value)),
            "citadel_crosshair_color_b" => set(&mut c.color.b, channel(value)),
            "citadel_crosshair_pip_border" => c.pip_border = truthy(value),
            "citadel_crosshair_pip_outline_border" => {
                c.pip_border = number(value).is_some_and(|v| v > 0.0);
            }
            "citadel_crosshair_pip_gap_static" => c.pip_gap_static = truthy(value),
            "citadel_crosshair_pip_opacity" => set(&mut c.pip_opacity, number(value)),
            "citadel_crosshair_pip_width" => set(&mut c.width, number(value)),
            "citadel_crosshair_pip_height" => set(&mut c.height, number(value)),
            "citadel_crosshair_pip_gap" => set(&mut c.gap, number(value)),
            "citadel_crosshair_dot_opacity" => set(&mut c.dot_opacity, number(value)),
            "citadel_crosshair_dot_outline_opacity" => {
                set(&mut c.dot_outline_opacity, number(value));
            }
            _ => continue,
        }
        found = true;
    }
    found.then_some(c)
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

const BOM: char = '\u{feff}';

/// A line's text for marker matching: no terminator, no surrounding space.
fn marker_text(line: &str) -> &str {
    line.trim()
}

/// Which managed section (index into `SECTIONS`) `line` starts, if any.
fn start_of(line: &str) -> Option<usize> {
    SECTIONS
        .iter()
        .position(|(start, _)| marker_text(line) == *start)
}

/// Managed blocks in `lines`, as `(section, first, last)` line indices, and
/// orphan start markers. A block is a start marker followed by its own end
/// marker with no other start marker in between; anything else is an orphan
/// start line, so a lone marker never claims the user's lines after it.
fn find_blocks(lines: &[&str]) -> (Vec<(usize, usize, usize)>, Vec<usize>) {
    let mut blocks = Vec::new();
    let mut orphans = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(section) = start_of(lines[i]) else {
            i += 1;
            continue;
        };
        let end = SECTIONS[section].1;
        let close = lines[i + 1..]
            .iter()
            .enumerate()
            .find(|(_, l)| start_of(l).is_some() || marker_text(l) == end)
            .filter(|(_, l)| marker_text(l) == end)
            .map(|(k, _)| i + 1 + k);
        match close {
            Some(j) => {
                blocks.push((section, i, j));
                i = j + 1;
            }
            None => {
                orphans.push(i);
                i += 1;
            }
        }
    }
    (blocks, orphans)
}

/// The crosshair in `autoexec.cfg`'s managed block (Shiv's, or the old app's
/// when Shiv has not written one). `None` for no file or no complete block.
pub fn load(paths: &GamePaths) -> Result<Option<CrosshairConfig>, CrosshairError> {
    let Some(text) = read_text(&paths.autoexec)? else {
        return Ok(None);
    };
    let text = text.strip_prefix(BOM).unwrap_or(&text);
    let lines: Vec<&str> = text.lines().collect();
    let (blocks, _) = find_blocks(&lines);
    Ok((0..SECTIONS.len())
        .find_map(|s| blocks.iter().find(|b| b.0 == s))
        .and_then(|&(_, first, last)| parse(&lines[first + 1..last].join("\n"))))
}

/// The file's usual line ending: CRLF when most lines end in it, else LF.
fn dominant_eol(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf { "\r\n" } else { "\n" }
}

/// Put one block for `c` in place of the first complete managed block
/// (Shiv's or the old app's), drop the other blocks and orphan start
/// markers, or append the block when there is none. Every other line keeps
/// its bytes and its own line ending; the new block uses the file's dominant
/// ending. A leading BOM is kept.
fn rewrite(text: &str, c: &CrosshairConfig) -> String {
    let (bom, body) = match text.strip_prefix(BOM) {
        Some(rest) => (BOM.to_string(), rest),
        None => (String::new(), text),
    };
    let eol = dominant_eol(body);
    let segments: Vec<&str> = body.split_inclusive('\n').collect();
    let (blocks, orphans) = find_blocks(&segments);
    let block = |last_terminator: &str| {
        format!(
            "{SECTION_START}{eol}{}{eol}{SECTION_END}{last_terminator}",
            to_commands(c)
        )
    };
    let mut out = bom;
    let mut replaced = false;
    let mut i = 0;
    while i < segments.len() {
        if let Some(&(_, _, last)) = blocks.iter().find(|b| b.1 == i) {
            if !replaced {
                let terminator =
                    &segments[last][segments[last].trim_end_matches(['\r', '\n']).len()..];
                out.push_str(&block(terminator));
                replaced = true;
            }
            i = last + 1;
            continue;
        }
        if !orphans.contains(&i) {
            out.push_str(segments[i]);
        }
        i += 1;
    }
    if !replaced {
        if !out.is_empty() && !out.ends_with('\n') && out != "\u{feff}" {
            out.push_str(eol);
        }
        out.push_str(&block(eol));
    }
    out
}

/// Write `contents` to `path` through a temp file in the same folder, so the
/// game never reads a half-written file. An existing file's permissions are
/// carried over.
fn write_atomic(path: &Path, contents: &str) -> Result<(), CrosshairError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).map_err(io_err("create", dir))?;
    let name = path.file_name().map_or_else(
        || "autoexec.cfg".into(),
        |n| n.to_string_lossy().into_owned(),
    );
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

/// Write `c` into the managed block of `autoexec.cfg`, creating the file and
/// its folder when missing. Lines outside the block keep their bytes and
/// line endings (see `rewrite`). A symlinked `autoexec.cfg` is written
/// through to its target.
///
/// This does not check for a running game: the caller decides (the UI
/// refuses while Deadlock runs, as the old app did).
pub fn apply(paths: &GamePaths, c: &CrosshairConfig) -> Result<(), CrosshairError> {
    let target = match fs::symlink_metadata(&paths.autoexec) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(&paths.autoexec).map_err(io_err("resolve", &paths.autoexec))?
        }
        _ => paths.autoexec.clone(),
    };
    let text = read_text(&target)?.unwrap_or_default();
    write_atomic(&target, &rewrite(&text, c))
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
    fn rewrite_of_empty_text_is_just_the_block() {
        let out = rewrite("", &CrosshairConfig::default());
        assert!(out.starts_with(SECTION_START));
        assert_eq!(out.lines().count(), 3);
    }
}
