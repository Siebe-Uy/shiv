//! Performance configs: the API's perf catalog (presets and convar metadata)
//! and source-preserving edits to the game's `gameinfo.gi`.
//!
//! The catalog shape follows the old app's `packages/perf-catalog/src/schema.ts`
//! and `perf_config/catalog.rs`; only the parts Shiv uses are read (stock
//! files and community configs are skipped). A catalog with a different
//! `schemaVersion` is refused.
//!
//! Edits work on the text, the way the old app's `perf_config/patch.rs`
//! does: `kv_parser::SourceDocument` locates every key and value token, and
//! only the value tokens of the listed paths change. Every other byte
//! (comments, spacing, quoting, line endings, a BOM) stays as it was. A key
//! the file already has is edited at every occurrence (the engine reads the
//! last one). A new key goes on its own line before its block's closing
//! brace. `value: None` comments a key out as `// shiv off: <line>`, and
//! setting the key again later restores that line instead of adding another.
//! The edited text is parsed again and checked before anything is written.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use kv_parser::{ParseOptions, SourceDocument, SourceKind, SourceSite};
use serde::{Deserialize, Serialize};

use crate::game::GamePaths;
use crate::platform;

/// The catalog schema this build understands (`SCHEMA_VERSION` in schema.ts).
pub const SCHEMA_VERSION: u32 = 1;

const BACKUP_SUFFIX: &str = ".bak";
/// Prefix of a line Shiv commented out (`value: None`).
const OFF_TAG: &str = "// shiv off: ";
const CACHE_FILE: &str = "perf-catalog.json";
const ETAG_FILE: &str = "perf-catalog.etag";

#[derive(Debug, thiserror::Error)]
pub enum PerfError {
    #[error("the performance catalog is not valid: {0}")]
    InvalidCatalog(String),
    #[error(
        "the performance catalog uses schema version {found}, but this version of Shiv reads version {expected}. Update Shiv to use it"
    )]
    SchemaVersion { found: u32, expected: u32 },
    #[error("{} is read-only, so Shiv left it unchanged", .0.display())]
    ReadOnly(PathBuf),
    #[error("{} is not plain text, so Shiv left it unchanged", .0.display())]
    NotText(PathBuf),
    #[error("{} could not be read as a game config ({reason}), so Shiv left it unchanged", path.display())]
    Unparsable { path: PathBuf, reason: String },
    #[error("{setting} can't be changed: {reason}")]
    Entry { setting: String, reason: String },
    #[error("could not {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> PerfError {
    let path = path.to_path_buf();
    move |source| PerfError::Io {
        action,
        path,
        source,
    }
}

// ---------------------------------------------------------------------------
// Catalog

/// One setting: a path below the `GameInfo` root (`["ConVars", "r_ssao"]`)
/// and its raw value without quotes. `None` comments the key out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigEntry {
    pub path: Vec<String>,
    pub value: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Pretty,
    Balanced,
    Lean,
    Sweaty,
    Potato,
}

impl Tier {
    /// Prettiest to fastest.
    pub const ALL: [Tier; 5] = [
        Tier::Pretty,
        Tier::Balanced,
        Tier::Lean,
        Tier::Sweaty,
        Tier::Potato,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tier::Pretty => "Pretty",
            Tier::Balanced => "Balanced",
            Tier::Lean => "Lean",
            Tier::Sweaty => "Sweaty",
            Tier::Potato => "Potato",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConvarKind {
    Bool,
    Int,
    Float,
    String,
    Enum,
    Color,
    Vector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConvarStatus {
    #[default]
    Active,
    /// The game ignores it from gameinfo.gi.
    Blocked,
    /// No longer in the game's convar list.
    Removed,
    /// A console command, not a convar.
    NotConvar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnumValue {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConvarMeta {
    pub name: String,
    pub kind: ConvarKind,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub enum_values: Vec<EnumValue>,
    #[serde(default)]
    pub help: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub category: String,
    #[serde(default)]
    pub side_effects: Option<String>,
    #[serde(default)]
    pub status: ConvarStatus,
}

impl ConvarMeta {
    /// Help text: the catalog's `help`, else its `description`.
    pub fn help_text(&self) -> Option<&str> {
        self.help
            .as_deref()
            .or(self.description.as_deref())
            .filter(|t| !t.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Category {
    pub id: String,
    pub label: String,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub author: String,
    pub tier: Tier,
    pub blurb: String,
    #[serde(default)]
    pub highlights: Vec<String>,
    #[serde(default)]
    pub recommended: bool,
    /// The author's changes relative to their base build, not a whole file.
    pub entries: Vec<ConfigEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeniedRule {
    #[serde(default)]
    pub path: Option<Vec<String>>,
    /// Regex matched against a ConVars key.
    #[serde(default)]
    pub pattern: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rules {
    /// `/`-joined sections never written (`FileSystem`).
    pub excluded_sections: Vec<String>,
    pub guarded_sections: Vec<String>,
    #[serde(default)]
    pub denied: Vec<DeniedRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    version: String,
    #[serde(default)]
    generated_at: String,
    categories: Vec<Category>,
    convars: Vec<ConvarMeta>,
    #[serde(default)]
    presets: Vec<Preset>,
    rules: Rules,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchemaProbe {
    schema_version: u32,
}

/// A parsed catalog with lookup indexes.
#[derive(Debug)]
pub struct Catalog {
    version: String,
    generated_at: String,
    categories: Vec<Category>,
    convars: Vec<ConvarMeta>,
    presets: Vec<Preset>,
    rules: Rules,
    index: HashMap<String, usize>,
    denied_patterns: Vec<(regex::Regex, String)>,
}

impl Catalog {
    /// Parse the API's catalog JSON. The schema version is checked before the
    /// rest, so a newer catalog is reported as such rather than as malformed.
    pub fn parse(bytes: &[u8]) -> Result<Catalog, PerfError> {
        let invalid = |e: serde_json::Error| PerfError::InvalidCatalog(e.to_string());
        let probe: SchemaProbe = serde_json::from_slice(bytes).map_err(invalid)?;
        if probe.schema_version != SCHEMA_VERSION {
            return Err(PerfError::SchemaVersion {
                found: probe.schema_version,
                expected: SCHEMA_VERSION,
            });
        }
        let file: CatalogFile = serde_json::from_slice(bytes).map_err(invalid)?;
        let index = file
            .convars
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.to_ascii_lowercase(), i))
            .collect();
        // An invalid pattern denies nothing rather than failing the catalog.
        let denied_patterns = file
            .rules
            .denied
            .iter()
            .filter_map(|rule| {
                let regex = regex::RegexBuilder::new(rule.pattern.as_deref()?)
                    .case_insensitive(true)
                    .build()
                    .ok()?;
                Some((regex, rule.reason.clone()))
            })
            .collect();
        Ok(Catalog {
            version: file.version,
            generated_at: file.generated_at,
            categories: file.categories,
            convars: file.convars,
            presets: file.presets,
            rules: file.rules,
            index,
            denied_patterns,
        })
    }

    /// Sortable revision, e.g. `2026.11.07-1`.
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn generated_at(&self) -> &str {
        &self.generated_at
    }

    pub fn categories(&self) -> &[Category] {
        &self.categories
    }

    pub fn category_label<'a>(&'a self, id: &'a str) -> &'a str {
        self.categories
            .iter()
            .find(|c| c.id == id)
            .map_or(id, |c| c.label.as_str())
    }

    pub fn convars(&self) -> &[ConvarMeta] {
        &self.convars
    }

    /// Case-insensitive lookup by convar name.
    pub fn convar(&self, name: &str) -> Option<&ConvarMeta> {
        self.index
            .get(&name.to_ascii_lowercase())
            .map(|&i| &self.convars[i])
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// The preset shown for `tier`: the recommended one, else the first.
    pub fn preset_info(&self, tier: Tier) -> Option<&Preset> {
        let mut of_tier = self.presets.iter().filter(|p| p.tier == tier);
        let first = of_tier.next()?;
        Some(
            std::iter::once(first)
                .chain(of_tier)
                .find(|p| p.recommended)
                .unwrap_or(first),
        )
    }

    /// The entries Shiv writes for `tier`'s preset: its ConVars edits, minus
    /// what the catalog rules deny. Edits in engine sections (SceneSystem,
    /// RenderSystem, ...) are left out, as the old app leaves them out by
    /// default: those are the sections Valve's matchmaking warning lists.
    /// Empty when the catalog has no preset for the tier.
    pub fn preset(&self, tier: Tier) -> Vec<ConfigEntry> {
        let Some(preset) = self.preset_info(tier) else {
            return Vec::new();
        };
        let mut out: Vec<ConfigEntry> = Vec::new();
        for entry in &preset.entries {
            let is_convar = entry.path.len() == 2 && entry.path[0].eq_ignore_ascii_case("ConVars");
            if !is_convar || self.writable(&entry.path).is_err() {
                continue;
            }
            // The last entry for a path wins, as in a file.
            match out.iter_mut().find(|e| path_eq(&e.path, &entry.path)) {
                Some(existing) => existing.value.clone_from(&entry.value),
                None => out.push(entry.clone()),
            }
        }
        out
    }

    /// Why the catalog rules refuse to write `path`, if they do.
    pub fn denied_reason(&self, path: &[String]) -> Option<&str> {
        for rule in &self.rules.denied {
            if rule.path.as_deref().is_some_and(|p| path_eq(p, path)) {
                return Some(&rule.reason);
            }
        }
        if path.len() == 2 && path[0].eq_ignore_ascii_case("ConVars") {
            for (regex, reason) in &self.denied_patterns {
                if regex.is_match(&path[1]) {
                    return Some(reason);
                }
            }
        }
        None
    }

    /// `Err(reason)` when the rules forbid writing `path`.
    pub fn writable(&self, path: &[String]) -> Result<(), String> {
        if let Some(section) = self
            .rules
            .excluded_sections
            .iter()
            .find(|s| has_prefix(path, s))
        {
            return Err(format!("{section} is never edited"));
        }
        match self.denied_reason(path) {
            Some(reason) => Err(reason.to_string()),
            None => Ok(()),
        }
    }
}

fn has_prefix(path: &[String], prefix: &str) -> bool {
    let parts: Vec<&str> = prefix.split('/').filter(|p| !p.is_empty()).collect();
    !parts.is_empty()
        && parts.len() <= path.len()
        && parts
            .iter()
            .zip(path)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn path_eq(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn path_key(path: &[String]) -> String {
    path.iter()
        .map(|p| p.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether two values read the same to the engine: booleans as `1`/`0`,
/// numbers by value, anything else exactly.
pub fn values_equal(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

fn normalize(value: &str) -> String {
    let v = value.trim();
    if v.eq_ignore_ascii_case("true") {
        return "1".into();
    }
    if v.eq_ignore_ascii_case("false") {
        return "0".into();
    }
    match v.parse::<f64>() {
        Ok(n) if n.is_finite() => n.to_string(),
        _ => v.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Catalog cache

/// The last good catalog from the API and its ETag, on disk.
#[derive(Debug, Clone)]
pub struct CatalogCache {
    dir: PathBuf,
}

impl CatalogCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<user cache dir>/shiv`, if the platform has one.
    pub fn default_dir() -> Option<PathBuf> {
        platform::cache_dir().map(|d| d.join("shiv"))
    }

    /// The cached catalog and its ETag; `None` when missing or no longer
    /// valid (a cache written by another version is ignored).
    pub fn load(&self) -> Option<(Option<String>, Catalog)> {
        let bytes = fs::read(self.dir.join(CACHE_FILE)).ok()?;
        let catalog = Catalog::parse(&bytes).ok()?;
        let etag = fs::read_to_string(self.dir.join(ETAG_FILE))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Some((etag, catalog))
    }

    /// Parse `bytes` and, only when valid, keep them with `etag`.
    pub fn store(&self, etag: &str, bytes: &[u8]) -> Result<Catalog, PerfError> {
        let catalog = Catalog::parse(bytes)?;
        self.save(etag, bytes)?;
        Ok(catalog)
    }

    /// Keep `bytes` with `etag`; the caller has already parsed them.
    pub fn save(&self, etag: &str, bytes: &[u8]) -> Result<(), PerfError> {
        fs::create_dir_all(&self.dir).map_err(io_err("create", &self.dir))?;
        // Drop the old ETag first: a stale one beside a new body would be
        // worse than none.
        let etag_path = self.dir.join(ETAG_FILE);
        match fs::remove_file(&etag_path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(io_err("remove", &etag_path)(e));
            }
            _ => {}
        }
        write_atomic(&self.dir.join(CACHE_FILE), bytes, None)?;
        if !etag.is_empty() {
            write_atomic(&etag_path, etag.as_bytes(), None)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// gameinfo.gi

fn parse_options() -> ParseOptions {
    // Source 2 reads backslashes literally in gameinfo.gi.
    ParseOptions {
        allow_escape_sequences: false,
        ..ParseOptions::default()
    }
}

/// A symlinked file is read and written through to its target.
fn resolve(path: &Path) -> Result<PathBuf, PerfError> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(io_err("resolve", path))
        }
        _ => Ok(path.to_path_buf()),
    }
}

fn read_text(path: &Path) -> Result<String, PerfError> {
    let bytes = fs::read(path).map_err(io_err("read", path))?;
    String::from_utf8(bytes).map_err(|_| PerfError::NotText(path.to_path_buf()))
}

fn parse_doc(path: &Path, text: &str) -> Result<SourceDocument, PerfError> {
    SourceDocument::parse(text, parse_options()).map_err(|e| PerfError::Unparsable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// The site's path below the root key, or `None` for the root itself.
fn rel(site: &SourceSite) -> Option<&[String]> {
    site.path.split_first().map(|(_, rest)| rest)
}

/// Every value set in `gameinfo.gi`, as the file is now (a game update
/// rewrites it). Paths are below the `GameInfo` root; a key set more than
/// once reads as its last value, at its first position. `FileSystem` is not
/// a setting and is left out.
pub fn read_current(paths: &GamePaths) -> Result<Vec<ConfigEntry>, PerfError> {
    let path = resolve(&paths.gameinfo)?;
    let text = read_text(&path)?;
    let doc = parse_doc(&path, &text)?;
    let mut out: Vec<ConfigEntry> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for site in &doc.sites {
        let SourceKind::Scalar { value, .. } = &site.kind else {
            continue;
        };
        let Some(rel) = rel(site).filter(|r| !r.is_empty()) else {
            continue;
        };
        if rel.len() < 2 || rel[0].eq_ignore_ascii_case("FileSystem") {
            continue;
        }
        let key = path_key(rel);
        match seen.get(&key) {
            Some(&i) => out[i].value = Some(value.clone()),
            None => {
                seen.insert(key, out.len());
                out.push(ConfigEntry {
                    path: rel.to_vec(),
                    value: Some(value.clone()),
                });
            }
        }
    }
    Ok(out)
}

/// Write `entries` into `gameinfo.gi`, changing only those paths. The file
/// as it was is copied to `gameinfo.gi.bak` first, and the new text replaces
/// the file through a temp file, keeping its permissions. A read-only file,
/// a file Shiv can't parse, or an entry it can't write safely fails without
/// touching anything. Applying values the file already has writes nothing.
pub fn apply(paths: &GamePaths, entries: &[ConfigEntry]) -> Result<(), PerfError> {
    let path = resolve(&paths.gameinfo)?;
    let text = read_text(&path)?;
    let edited = edit(&path, &text, entries)?;
    if edited == text {
        return Ok(());
    }
    let meta = fs::metadata(&path).map_err(io_err("read", &path))?;
    // A rename would replace a read-only file anyway, so check first. The
    // open (without truncating) also catches a file owned by someone else.
    if meta.permissions().readonly() {
        return Err(PerfError::ReadOnly(path));
    }
    if let Err(e) = fs::OpenOptions::new().append(true).open(&path) {
        return Err(if e.kind() == io::ErrorKind::PermissionDenied {
            PerfError::ReadOnly(path)
        } else {
            io_err("open", &path)(e)
        });
    }
    let mut backup = path.file_name().unwrap_or_default().to_os_string();
    backup.push(BACKUP_SUFFIX);
    let backup = path.with_file_name(backup);
    write_atomic(&backup, text.as_bytes(), Some(meta.permissions()))?;
    write_atomic(&path, edited.as_bytes(), Some(meta.permissions()))
}

/// Write `contents` to `path` through a temp file in the same folder.
fn write_atomic(
    path: &Path,
    contents: &[u8],
    permissions: Option<fs::Permissions>,
) -> Result<(), PerfError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.shiv-{}.tmp", std::process::id()));
    let permissions = permissions.or_else(|| fs::metadata(path).ok().map(|m| m.permissions()));
    let written = fs::File::create(&tmp)
        .and_then(|mut f| {
            f.write_all(contents)?;
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

/// One line of the text with its own ending.
struct Line<'a> {
    start: usize,
    content: &'a str,
    eol: &'a str,
}

fn split_lines(text: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for segment in text.split_inclusive('\n') {
        let content = segment
            .strip_suffix('\n')
            .map_or(segment, |s| s.strip_suffix('\r').unwrap_or(s));
        lines.push(Line {
            start,
            content,
            eol: &segment[content.len()..],
        });
        start += segment.len();
    }
    lines
}

fn line_index(lines: &[Line], offset: usize) -> usize {
    lines
        .partition_point(|l| l.start <= offset)
        .saturating_sub(1)
}

fn indent_of(content: &str) -> &str {
    &content[..content.len() - content.trim_start_matches([' ', '\t']).len()]
}

/// The token starting at byte `start`: its end and text without quotes.
/// Same rules as kv-parser with escape sequences off.
fn read_token(content: &str, start: usize) -> Option<(usize, &str)> {
    let rest = content.get(start..)?;
    if let Some(quoted) = rest.strip_prefix('"') {
        let close = quoted.find('"')?;
        return Some((start + close + 2, &quoted[..close]));
    }
    if rest.starts_with(['[', '#', '{', '}']) {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut end = 0;
    while end < bytes.len() {
        match bytes[end] {
            b' ' | b'\t' | b'\r' | b'\n' | b'"' | b'{' | b'}' => break,
            b'/' if matches!(bytes.get(end + 1), Some(b'/' | b'*')) => break,
            _ => end += 1,
        }
    }
    (end > 0).then(|| (start + end, &rest[..end]))
}

/// A `key value` line: the key, and the value token's range within the line.
fn parse_entry_line(content: &str) -> Option<(&str, Range<usize>)> {
    let key_start = indent_of(content).len();
    let (key_end, key) = read_token(content, key_start)?;
    let value_start = key_end + indent_of(&content[key_end..]).len();
    let (value_end, _) = read_token(content, value_start)?;
    let rest = content[value_end..].trim_start_matches([' ', '\t']);
    let alone = rest.is_empty() || rest.starts_with("//") || rest.starts_with('[');
    alone.then_some((key, value_start..value_end))
}

/// Whether `value` reads back as one token without quotes.
fn is_bare_token(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('#')
        && !value.contains("//")
        && !value.contains("/*")
        && !value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '{' | '}' | '[' | ']'))
}

fn is_writable_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Strings other code finds anywhere in gameinfo.gi (the addons mount).
fn is_reserved(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("citadel/addons") || lower.contains("searchpaths")
}

/// Whether `value` can be written between quotes without changing how
/// gameinfo.gi parses.
pub fn is_writable_value(value: &str) -> bool {
    !value
        .chars()
        .any(|c| c.is_control() || matches!(c, '"' | '\\' | '{' | '}'))
        && !value.contains("//")
        && !value.contains("/*")
        && !is_reserved(value)
}

/// `line` with the value token at `range` set to `value`, keeping its quoting.
fn with_value(line: &str, range: Range<usize>, value: &str) -> String {
    let original = &line[range.clone()];
    let token = if original.starts_with('"') || !is_bare_token(value) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    };
    format!("{}{token}{}", &line[..range.start], &line[range.end..])
}

fn entry_err(entry: &ConfigEntry, reason: impl Into<String>) -> PerfError {
    PerfError::Entry {
        setting: entry.path.join("/"),
        reason: reason.into(),
    }
}

fn check_entry(entry: &ConfigEntry) -> Result<(), PerfError> {
    if entry.path.len() < 2 {
        return Err(entry_err(entry, "it is not inside a block"));
    }
    if entry.path[0].eq_ignore_ascii_case("FileSystem") {
        return Err(entry_err(entry, "FileSystem is never edited"));
    }
    if !entry.path.iter().all(|k| is_writable_key(k)) || entry.path.iter().any(|k| is_reserved(k)) {
        return Err(entry_err(entry, "it is not a key gameinfo.gi can hold"));
    }
    if entry
        .value
        .as_deref()
        .is_some_and(|v| !is_writable_value(v))
    {
        return Err(entry_err(
            entry,
            "the value has characters gameinfo.gi can't hold",
        ));
    }
    Ok(())
}

/// The edited text, or why it can't be edited safely.
fn edit(path: &Path, text: &str, entries: &[ConfigEntry]) -> Result<String, PerfError> {
    let doc = parse_doc(path, text)?;
    let lines = split_lines(text);
    let fallback_eol = if text.contains("\r\n") { "\r\n" } else { "\n" };

    // The last entry for a path wins.
    let mut planned: Vec<&ConfigEntry> = Vec::new();
    for entry in entries {
        check_entry(entry)?;
        match planned.iter_mut().find(|e| path_eq(&e.path, &entry.path)) {
            Some(slot) => *slot = entry,
            None => planned.push(entry),
        }
    }

    let objects: Vec<(&[String], usize, usize)> = doc
        .sites
        .iter()
        .filter_map(|site| match site.kind {
            SourceKind::Object { open, close } => rel(site).map(|r| (r, open, close)),
            SourceKind::Scalar { .. } => None,
        })
        .collect();

    // Replacement content per line index, and new lines before a line.
    let mut replaced: HashMap<usize, String> = HashMap::new();
    let mut inserted: HashMap<usize, Vec<String>> = HashMap::new();

    for entry in &planned {
        let scalars: Vec<(usize, &Range<usize>)> = doc
            .sites
            .iter()
            .filter_map(|site| match &site.kind {
                SourceKind::Scalar { value_range, .. }
                    if rel(site).is_some_and(|r| path_eq(r, &entry.path)) =>
                {
                    Some((site.key_start, value_range))
                }
                _ => None,
            })
            .collect();

        if !scalars.is_empty() {
            for (key_start, range) in scalars {
                let i = line_index(&lines, key_start);
                let line = &lines[i];
                let local = range.start - line.start..range.end - line.start;
                if parse_entry_line(line.content).map(|(_, r)| r) != Some(local.clone()) {
                    return Err(entry_err(entry, "it shares its line with other settings"));
                }
                let new = match entry.value.as_deref() {
                    Some(value) => with_value(line.content, local, value),
                    None => {
                        let indent = indent_of(line.content);
                        format!("{indent}{OFF_TAG}{}", &line.content[indent.len()..])
                    }
                };
                replaced.insert(i, new);
            }
            continue;
        }

        let Some(value) = entry.value.as_deref() else {
            continue;
        };
        if objects.iter().any(|(r, ..)| path_eq(r, &entry.path)) {
            return Err(entry_err(
                entry,
                "it is a block in gameinfo.gi, not a value",
            ));
        }
        let (parent, key) = entry.path.split_at(entry.path.len() - 1);
        let key = &key[0];
        let Some(&(_, open, close)) = objects.iter().rev().find(|(r, ..)| path_eq(r, parent))
        else {
            return Err(entry_err(
                entry,
                format!("gameinfo.gi has no {} block", parent.join("/")),
            ));
        };
        let (open_line, close_line) = (line_index(&lines, open), line_index(&lines, close));

        // A line Shiv commented out earlier comes back.
        let nested: Vec<(usize, usize)> = objects
            .iter()
            .filter(|(r, o, c)| r.len() == parent.len() + 1 && *o > open && *c < close)
            .map(|&(_, o, c)| (o, c))
            .collect();
        let restored = (open_line + 1..close_line).find_map(|i| {
            let line = &lines[i];
            if nested
                .iter()
                .any(|&(o, c)| line.start > o && line.start < c)
            {
                return None;
            }
            let indent = indent_of(line.content);
            let rest = line.content[indent.len()..].strip_prefix(OFF_TAG)?;
            let (k, range) = parse_entry_line(rest)?;
            k.eq_ignore_ascii_case(key)
                .then(|| (i, format!("{indent}{}", with_value(rest, range, value))))
        });
        if let Some((i, new)) = restored {
            replaced.insert(i, new);
            continue;
        }

        let before_close = &lines[close_line].content[..close - lines[close_line].start];
        if open_line == close_line || !before_close.trim().is_empty() {
            return Err(entry_err(
                entry,
                format!("the {} block closes on a shared line", parent.join("/")),
            ));
        }
        // Indent like the block's first entry, else one tab deeper than the brace.
        let indent = doc
            .sites
            .iter()
            .filter(|s| s.key_start > open && s.key_start < close)
            .find(|s| rel(s).is_some_and(|r| r.len() == parent.len() + 1))
            .map(|s| {
                let line = &lines[line_index(&lines, s.key_start)];
                indent_of(line.content).to_string()
            })
            .unwrap_or_else(|| format!("{}\t", indent_of(lines[close_line].content)));
        inserted
            .entry(close_line)
            .or_default()
            .push(format!("{indent}\"{key}\"\t\t\"{value}\""));
    }

    let mut out = String::with_capacity(text.len() + 64 * inserted.len());
    for (i, line) in lines.iter().enumerate() {
        let eol = if line.eol.is_empty() {
            fallback_eol
        } else {
            line.eol
        };
        for new in inserted.get(&i).into_iter().flatten() {
            out.push_str(new);
            out.push_str(eol);
        }
        out.push_str(replaced.get(&i).map_or(line.content, String::as_str));
        out.push_str(line.eol);
    }
    verify(path, &doc, &out, &planned)?;
    Ok(out)
}

/// The edited text parses, keeps its blocks and every value not planned, and
/// holds exactly the planned values.
fn verify(
    path: &Path,
    before: &SourceDocument,
    text: &str,
    planned: &[&ConfigEntry],
) -> Result<(), PerfError> {
    let failed = |reason: &str| PerfError::Unparsable {
        path: path.to_path_buf(),
        reason: format!("the edit {reason}"),
    };
    let after =
        SourceDocument::parse(text, parse_options()).map_err(|_| failed("would not parse back"))?;
    let planned_keys: HashSet<String> = planned.iter().map(|e| path_key(&e.path)).collect();
    let others = |doc: &SourceDocument| -> Vec<(String, Option<String>)> {
        doc.sites
            .iter()
            .filter(|s| !rel(s).is_some_and(|r| planned_keys.contains(&path_key(r))))
            .map(|s| {
                let value = match &s.kind {
                    SourceKind::Scalar { value, .. } => Some(value.clone()),
                    SourceKind::Object { .. } => None,
                };
                (path_key(&s.path), value)
            })
            .collect()
    };
    if others(before) != others(&after) {
        return Err(failed("would change other settings"));
    }
    for entry in planned {
        let values: Vec<&str> = after
            .sites
            .iter()
            .filter(|s| rel(s).is_some_and(|r| path_eq(r, &entry.path)))
            .filter_map(|s| match &s.kind {
                SourceKind::Scalar { value, .. } => Some(value.as_str()),
                SourceKind::Object { .. } => None,
            })
            .collect();
        let ok = match entry.value.as_deref() {
            Some(v) => !values.is_empty() && values.iter().all(|x| *x == v),
            None => values.is_empty(),
        };
        if !ok {
            return Err(failed(&format!("would not set {}", entry.path.join("/"))));
        }
    }
    Ok(())
}
