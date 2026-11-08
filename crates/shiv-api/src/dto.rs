//! Wire types for the Deadlock Mods API. Only fields the Browse/Install
//! screens need are modelled; everything non-essential is optional so an API
//! addition or a null never breaks parsing. Unknown fields are ignored.

use serde::Deserialize;

/// One mod from `GET /v2/mods`. Dates are kept as the ISO-8601 strings the API
/// sends: they sort correctly as text and avoid a date dependency.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModDto {
    /// Remote (GameBanana) id; this is what `/v2/mods/{id}/downloads` takes.
    pub remote_id: String,
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub remote_url: Option<String>,
    pub category: Option<String>,
    pub author: Option<String>,
    pub likes: u64,
    pub download_count: u64,
    pub downloadable: bool,
    pub tags: Vec<String>,
    pub images: Vec<String>,
    pub thumbnail_url: Option<String>,
    pub hero: Option<String>,
    pub is_audio: bool,
    pub is_map: bool,
    #[serde(rename = "isNSFW")]
    pub is_nsfw: bool,
    pub remote_added_at: Option<String>,
    pub remote_updated_at: Option<String>,
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
