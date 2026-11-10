//! GameBanana reads the Deadlock Mods API does not offer: a mod's files,
//! changelog and comments (`https://gamebanana.com/apiv11`).
//!
//! Every field is optional on the wire and parsed leniently, so a change on
//! GameBanana's side degrades one tab instead of breaking the page. Text
//! (`text`) is the HTML GameBanana sends; the UI converts it to plain text.

use std::time::Duration;

use serde::Deserialize;
use ureq::Agent;

use crate::error::ApiError;

const DEFAULT_BASE: &str = "https://gamebanana.com/apiv11";
const PER_PAGE: u32 = 15;
const BODY_LIMIT: u64 = 8 * 1024 * 1024;

pub struct GameBananaClient {
    base: String,
    agent: Agent,
}

impl Default for GameBananaClient {
    fn default() -> Self {
        Self::new(DEFAULT_BASE)
    }
}

/// A downloadable file of a mod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModFile {
    pub name: String,
    pub size: u64,
    /// Unix seconds.
    pub added: Option<i64>,
    pub downloads: u64,
}

/// One entry of a mod's changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub title: String,
    pub version: Option<String>,
    pub added: Option<i64>,
    /// `(category, text)` pairs such as `("Bugfix", "Fixed a crash")`.
    pub changes: Vec<(String, String)>,
    /// Free text (HTML) the author wrote besides the change list.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub author: String,
    pub added: Option<i64>,
    pub replies: u64,
    pub pinned: bool,
    /// HTML.
    pub text: String,
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    /// No further pages after this one.
    pub complete: bool,
}

/// Comments can be switched off by the author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comments {
    Hidden,
    Page(Page<Comment>),
}

// ---- wire types ----

#[derive(Deserialize)]
struct Envelope<T> {
    #[serde(rename = "_aMetadata", default)]
    meta: Meta,
    #[serde(rename = "_aRecords", default = "Vec::new")]
    records: Vec<T>,
    #[serde(rename = "_sErrorCode", default)]
    error: Option<String>,
}

#[derive(Deserialize, Default)]
struct Meta {
    #[serde(rename = "_nRecordCount", default)]
    count: u64,
    #[serde(rename = "_bIsComplete", default)]
    complete: bool,
}

#[derive(Deserialize)]
struct ProfileWire {
    #[serde(rename = "_aFiles", default)]
    files: Option<Vec<FileWire>>,
}

#[derive(Deserialize)]
struct FileWire {
    #[serde(rename = "_sFile", default)]
    name: Option<String>,
    #[serde(rename = "_nFilesize", default)]
    size: Option<u64>,
    #[serde(rename = "_tsDateAdded", default)]
    added: Option<i64>,
    #[serde(rename = "_nDownloadCount", default)]
    downloads: Option<u64>,
    #[serde(rename = "_bIsArchived", default)]
    archived: bool,
}

#[derive(Deserialize)]
struct UpdateWire {
    #[serde(rename = "_sName", default)]
    title: Option<String>,
    #[serde(rename = "_sVersion", default)]
    version: Option<String>,
    #[serde(rename = "_tsDateAdded", default)]
    added: Option<i64>,
    #[serde(rename = "_sText", default)]
    text: Option<String>,
    #[serde(rename = "_aChangeLog", default)]
    changes: Option<Vec<ChangeWire>>,
    #[serde(rename = "_bIsTrashed", default)]
    trashed: bool,
    #[serde(rename = "_bIsPrivate", default)]
    private: bool,
}

#[derive(Deserialize)]
struct ChangeWire {
    #[serde(default)]
    cat: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct CommentWire {
    #[serde(rename = "_nStatus", default)]
    status: Option<serde_json::Value>,
    #[serde(rename = "_tsDateAdded", default)]
    added: Option<i64>,
    #[serde(rename = "_nReplyCount", default)]
    replies: Option<u64>,
    #[serde(rename = "_iPinLevel", default)]
    pin: Option<i64>,
    #[serde(rename = "_sText", default)]
    text: Option<String>,
    #[serde(rename = "_aPoster", default)]
    poster: Option<PosterWire>,
}

#[derive(Deserialize)]
struct PosterWire {
    #[serde(rename = "_sName", default)]
    name: Option<String>,
}

/// GameBanana status 0 is a visible post; anything else is trashed or hidden.
fn visible(status: &Option<serde_json::Value>) -> bool {
    match status {
        None => true,
        Some(serde_json::Value::String(s)) => s == "0",
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(0),
        Some(_) => false,
    }
}

impl GameBananaClient {
    pub fn new(base_url: &str) -> Self {
        let agent: Agent = Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Self {
            base: base_url.trim_end_matches('/').to_owned(),
            agent,
        }
    }

    /// `Mod` or `Sound`, and a validated numeric id: the id goes into a URL
    /// path, so anything but digits is refused.
    fn item(&self, remote_id: &str, audio: bool) -> Result<String, ApiError> {
        if remote_id.is_empty() || !remote_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ApiError::Parse(format!("invalid mod id {remote_id:?}")));
        }
        let model = if audio { "Sound" } else { "Mod" };
        Ok(format!("{}/{model}/{remote_id}", self.base))
    }

    fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, ApiError> {
        let mut resp = self.agent.get(url).call().map_err(|e| match e {
            ureq::Error::StatusCode(status) => ApiError::Http {
                status,
                url: url.to_owned(),
            },
            other => ApiError::Network(other.to_string()),
        })?;
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()
            .map_err(|e| ApiError::Network(e.to_string()))?;
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Parse(e.to_string()))
    }

    /// The mod's current files, newest first.
    pub fn files(&self, remote_id: &str, audio: bool) -> Result<Vec<ModFile>, ApiError> {
        let url = format!("{}/ProfilePage", self.item(remote_id, audio)?);
        let profile: ProfileWire = self.get(&url)?;
        let mut files: Vec<ModFile> = profile
            .files
            .unwrap_or_default()
            .into_iter()
            .filter(|f| !f.archived)
            .filter_map(|f| {
                Some(ModFile {
                    name: f.name.filter(|n| !n.is_empty())?,
                    size: f.size.unwrap_or(0),
                    added: f.added,
                    downloads: f.downloads.unwrap_or(0),
                })
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.added));
        Ok(files)
    }

    /// One page (1-based) of the changelog, newest first.
    pub fn changelog(
        &self,
        remote_id: &str,
        audio: bool,
        page: u32,
    ) -> Result<Page<ChangelogEntry>, ApiError> {
        let url = format!(
            "{}/Updates?_nPage={}&_nPerpage={PER_PAGE}",
            self.item(remote_id, audio)?,
            page.max(1)
        );
        let env: Envelope<UpdateWire> = self.get(&url)?;
        let items = env
            .records
            .into_iter()
            .filter(|u| !u.trashed && !u.private)
            .map(|u| ChangelogEntry {
                title: u.title.unwrap_or_default(),
                version: u.version.filter(|v| !v.trim().is_empty()),
                added: u.added,
                changes: u
                    .changes
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|c| {
                        Some((c.cat.unwrap_or_default(), c.text.filter(|t| !t.is_empty())?))
                    })
                    .collect(),
                text: u.text.unwrap_or_default(),
            })
            .collect();
        Ok(Page {
            items,
            total: env.meta.count,
            complete: env.meta.complete,
        })
    }

    /// One page (1-based) of comments, or [`Comments::Hidden`] when the
    /// author turned them off.
    pub fn comments(&self, remote_id: &str, audio: bool, page: u32) -> Result<Comments, ApiError> {
        let url = format!(
            "{}/Posts?_nPage={}&_nPerpage={PER_PAGE}",
            self.item(remote_id, audio)?,
            page.max(1)
        );
        let env: Envelope<CommentWire> = self.get(&url)?;
        if env.error.as_deref() == Some("COMMENT_MODE_HIDDEN") {
            return Ok(Comments::Hidden);
        }
        let items = env
            .records
            .into_iter()
            .filter(|c| visible(&c.status))
            .filter_map(|c| {
                Some(Comment {
                    author: c.poster.and_then(|p| p.name).unwrap_or_default(),
                    added: c.added,
                    replies: c.replies.unwrap_or(0),
                    pinned: c.pin.unwrap_or(0) > 0,
                    text: c.text.filter(|t| !t.trim().is_empty())?,
                })
            })
            .collect();
        Ok(Comments::Page(Page {
            items,
            total: env.meta.count,
            complete: env.meta.complete,
        }))
    }
}
