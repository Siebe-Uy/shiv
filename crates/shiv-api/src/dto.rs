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
    /// The author's catalog id (`/v2/mod-authors/{id}`); groups a mod's author's mods.
    #[serde(default)]
    pub mod_author_id: Option<String>,
    #[serde(default)]
    pub metadata: Option<ModMetadata>,
}

/// Extra per-mod data. Every part is optional and lenient: a bad entry here
/// must never invalidate the mod row.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModMetadata {
    #[serde(default)]
    pub donation_links: Option<Vec<DonationLink>>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct DonationLink {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
}

/// Hosts a donation link may point at. The catalog is third-party data, so
/// only well-known support pages are offered.
const DONATION_HOSTS: &[&str] = &[
    "ko-fi.com",
    "patreon.com",
    "buymeacoffee.com",
    "paypal.com",
    "paypal.me",
    "liberapay.com",
];

/// True for an `https` URL on a known donation host (a `www.` prefix is
/// fine). Ports and credentials in the URL are refused.
pub fn is_donation_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.is_empty() || host.contains(['@', ':', '\\']) {
        return false;
    }
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    DONATION_HOSTS.contains(&host)
}

impl ModDto {
    /// The first donation link the author offers on a known host, as
    /// `(platform, url)`. The platform falls back to the host's name.
    pub fn donation_link(&self) -> Option<(&str, &str)> {
        let links = self.metadata.as_ref()?.donation_links.as_ref()?;
        links.iter().find_map(|l| {
            let url = l.url.as_deref()?.trim();
            is_donation_url(url).then(|| {
                let platform = l
                    .platform
                    .as_deref()
                    .map(str::trim)
                    .filter(|p| !p.is_empty());
                (platform.unwrap_or("Support the author"), url)
            })
        })
    }

    /// When the mod last changed: the GameBanana update date, else the date
    /// its files changed. Empty strings count as missing.
    pub fn last_updated(&self) -> Option<&str> {
        fn non_empty(s: &Option<String>) -> Option<&str> {
            s.as_deref().filter(|s| !s.is_empty())
        }
        non_empty(&self.remote_updated_at).or_else(|| non_empty(&self.files_updated_at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn donation_urls_need_https_and_a_known_host() {
        assert!(is_donation_url("https://ko-fi.com/lilotty"));
        assert!(is_donation_url("https://www.patreon.com/x"));
        assert!(is_donation_url("https://PayPal.me/x"));
        assert!(!is_donation_url("http://ko-fi.com/x"));
        assert!(!is_donation_url("https://evil.test/ko-fi.com"));
        assert!(!is_donation_url("https://ko-fi.com@evil.test/x"));
        assert!(!is_donation_url("https://ko-fi.com.evil.test/x"));
        assert!(!is_donation_url("https://ko-fi.com:444/x"));
        assert!(!is_donation_url("javascript:alert(1)"));
        assert!(!is_donation_url(""));
    }

    #[test]
    fn a_mod_offers_its_first_safe_donation_link() {
        let m: ModDto = serde_json::from_str(
            r#"{"id":"a","remoteId":"1","name":"n","modAuthorId":"auth",
            "metadata":{"donationLinks":[{"url":"https://evil.test/x","platform":"Ko-fi"},
            {"platform":"Patreon"},{"url":"https://ko-fi.com/me","platform":"Ko-fi"}]}}"#,
        )
        .unwrap();
        assert_eq!(m.donation_link(), Some(("Ko-fi", "https://ko-fi.com/me")));
        assert_eq!(m.mod_author_id.as_deref(), Some("auth"));
        let none: ModDto = serde_json::from_str(
            r#"{"id":"a","remoteId":"1","name":"n","metadata":{"donationLinks":null}}"#,
        )
        .unwrap();
        assert_eq!(none.donation_link(), None);
    }
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
