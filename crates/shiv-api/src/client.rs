use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ureq::Agent;

use crate::dto::{DownloadFile, DownloadsResponse, ModDto};
use crate::error::ApiError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// ureq has no per-read timeout, only a limit on the whole body. API JSON is
/// small, so it gets the 30 s read budget; file downloads get a generous total
/// cap so a stalled connection still ends.
const DOWNLOAD_BODY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Upper bound for buffered JSON bodies (catalog is a few MB).
const JSON_LIMIT: u64 = 64 * 1024 * 1024;

pub struct Client {
    base: String,
    api: Agent,
    download: Agent,
}

fn agent(body_timeout: Duration) -> Agent {
    Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        .timeout_recv_body(Some(body_timeout))
        .build()
        .into()
}

fn map_err(e: ureq::Error, url: &str) -> ApiError {
    match e {
        ureq::Error::StatusCode(status) => ApiError::Http {
            status,
            url: url.to_string(),
        },
        ureq::Error::Io(e) => ApiError::Io(e),
        other => ApiError::Network(other.to_string()),
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

impl Client {
    pub fn new(base_url: &str) -> Client {
        Client {
            base: base_url.trim_end_matches('/').to_string(),
            api: agent(READ_TIMEOUT),
            download: agent(DOWNLOAD_BODY_TIMEOUT),
        }
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, ApiError> {
        let mut resp = self.api.get(url).call().map_err(|e| map_err(e, url))?;
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(JSON_LIMIT)
            .read_to_vec()
            .map_err(|e| map_err(e, url))?;
        serde_json::from_slice(&bytes).map_err(|e| ApiError::Parse(e.to_string()))
    }

    /// The whole catalog (`GET /v2/mods` takes no parameters). Use
    /// [`crate::apply_query`] for search, sort and paging.
    pub fn list_mods(&self) -> Result<Vec<ModDto>, ApiError> {
        self.get_json(&format!("{}/v2/mods", self.base))
    }

    /// Download files for a mod, with URLs exactly as the API returns them
    /// (possibly mirrored).
    pub fn downloads(&self, id: &str) -> Result<Vec<DownloadFile>, ApiError> {
        let url = format!("{}/v2/mods/{}/downloads", self.base, encode_segment(id));
        Ok(self.get_json::<DownloadsResponse>(&url)?.downloads)
    }

    /// `Ok(None)` when the server answers 304 for `etag`; otherwise the new
    /// ETag (empty if the server sent none) and the raw body.
    pub fn perf_catalog(&self, etag: Option<&str>) -> Result<Option<(String, Vec<u8>)>, ApiError> {
        let url = format!("{}/v2/perf-catalog", self.base);
        let mut req = self.api.get(&url);
        if let Some(etag) = etag {
            req = req.header("If-None-Match", etag);
        }
        let mut resp = req.call().map_err(|e| map_err(e, &url))?;
        if resp.status().as_u16() == 304 {
            return Ok(None);
        }
        let new_etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = resp
            .body_mut()
            .with_config()
            .limit(JSON_LIMIT)
            .read_to_vec()
            .map_err(|e| map_err(e, &url))?;
        Ok(Some((new_etag, body)))
    }

    /// Stream `url` to `dest` via `<dest>.part`, renaming on success. The
    /// partial file is removed on any error. `progress(done, total)` is called
    /// after each chunk; `total` is the Content-Length when known.
    pub fn fetch(
        &self,
        url: &str,
        dest: &Path,
        progress: impl FnMut(u64, Option<u64>),
    ) -> Result<(), ApiError> {
        let part = part_path(dest);
        let result = self
            .fetch_to_part(url, &part, progress)
            .and_then(|()| fs::rename(&part, dest).map_err(ApiError::from));
        if result.is_err() {
            let _ = fs::remove_file(&part);
        }
        result
    }

    fn fetch_to_part(
        &self,
        url: &str,
        part: &Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<(), ApiError> {
        let mut resp = self.download.get(url).call().map_err(|e| map_err(e, url))?;
        let total = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let mut reader = resp.body_mut().as_reader();
        let mut file = File::create(part)?;
        let mut buf = [0u8; 64 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => return Err(ApiError::Network(e.to_string())),
            };
            file.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total);
        }
        file.sync_all()?;
        if done == 0 {
            return Err(ApiError::EmptyDownload);
        }
        match total {
            Some(expected) if expected != done => Err(ApiError::Incomplete {
                received: done,
                expected,
            }),
            _ => Ok(()),
        }
    }
}
