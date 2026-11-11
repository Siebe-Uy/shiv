//! Contract check against the real Deadlock Mods API. Ignored by default (it
//! needs the network); the `API contract` workflow runs it daily:
//! `cargo test -p shiv-api --test live_api -- --ignored`.
//!
//! It fails when the API changes shape in a way that would leave Shiv with no
//! mods, no downloads or no performance catalog.

use shiv_api::Client;

const BASE: &str = "https://api.deadlockmods.app/api";

fn raw(path: &str) -> String {
    ureq::get(format!("{BASE}{path}"))
        .call()
        .unwrap_or_else(|e| panic!("GET {path}: {e}"))
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_string()
        .unwrap_or_else(|e| panic!("reading {path}: {e}"))
}

#[test]
#[ignore = "needs the network"]
fn catalog_still_parses() {
    let rows = serde_json::from_str::<serde_json::Value>(&raw("/v2/mods"))
        .expect("/v2/mods is JSON")
        .as_array()
        .expect("/v2/mods is an array")
        .len();
    let mods = Client::new(BASE).list_mods().expect("list_mods");
    assert!(mods.len() >= 1000, "only {} mods parsed", mods.len());
    // Rows that no longer deserialize are skipped silently, so compare counts.
    assert!(
        mods.len() * 100 >= rows * 98,
        "{} of {rows} rows parsed: the row shape changed",
        mods.len()
    );
}

#[test]
#[ignore = "needs the network"]
fn downloads_still_parse() {
    let client = Client::new(BASE);
    let mods = client.list_mods().expect("list_mods");
    let mut checked = 0;
    for m in mods.iter().filter(|m| m.downloadable).take(5) {
        let files = client.downloads(&m.remote_id).expect("downloads");
        assert!(
            files.iter().all(|f| f.url.starts_with("http")),
            "{}: download without a URL",
            m.name
        );
        checked += usize::from(!files.is_empty());
    }
    assert!(checked > 0, "none of 5 downloadable mods had files");
}

#[test]
#[ignore = "needs the network"]
fn perf_catalog_is_served() {
    let (_, body) = Client::new(BASE)
        .perf_catalog(None)
        .expect("perf_catalog")
        .expect("a fresh request is never 304");
    assert!(!body.is_empty(), "empty performance catalog");
    serde_json::from_slice::<serde_json::Value>(&body).expect("performance catalog is JSON");
}
