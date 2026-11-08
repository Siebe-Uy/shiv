//! Wire types for the Deadlock Mods API. Only fields the Browse/Install
//! screens need are modelled; everything non-essential is optional so an API
//! addition or a null never breaks parsing. Unknown fields are ignored.

use serde::Deserialize;

/// One mod from `GET /v2/mods`. `id`, `remote_id` and `name` are required: a row
/// missing any of them is invalid. A null in a non-Option field also makes the
/// row invalid (`list_mods` skips such rows). Dates are kept as the ISO-8601 strings the API
/// sends: they sort correctly as text and avoid a date dependency.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModDto {
    /// Remote (GameBanana) id; this is what `/v2/mods/{id}/downloads` takes.
    pub remote_id: String,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub likes: u64,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub downloadable: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub thumbnail_url: Option<String>,
    #[serde(default)]
    pub hero: Option<String>,
    #[serde(default)]
    pub is_audio: bool,
    #[serde(default)]
    pub is_map: bool,
    #[serde(default, rename = "isNSFW")]
    pub is_nsfw: bool,
    #[serde(default)]
    pub remote_added_at: Option<String>,
    #[serde(default)]
    pub remote_updated_at: Option<String>,
    #[serde(default)]
    pub files_updated_at: Option<String>,
}

/// One downloadable file from `GET /v2/mods/{id}/downloads`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadFile {
    pub url: String,
    #[serde(default)]
    pub name: String,
    /// Bytes; the API reports 0 for override downloads of unknown size.
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DownloadsResponse {
    pub downloads: Vec<DownloadFile>,
}

/// Row wrapper so one malformed mod is skipped instead of failing the catalog.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum Row {
    Ok(Box<ModDto>),
    Skip(serde::de::IgnoredAny),
}
