use std::io::Read;
use std::path::Path;
use std::thread;

use shiv_api::{ApiError, Client};
use tiny_http::{Header, Response, Server};

/// Serves `n` requests with `handler`, returning the base URL.
fn stub<F>(n: usize, handler: F) -> String
where
    F: Fn(&tiny_http::Request) -> Response<std::io::Cursor<Vec<u8>>> + Send + 'static,
{
    let server = Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    thread::spawn(move || {
        for _ in 0..n {
            let Ok(req) = server.recv() else { return };
            let resp = handler(&req);
            let _ = req.respond(resp);
        }
    });
    url
}

/// Announces 1000 bytes, sends 10, then closes the connection.
fn truncated_server() -> std::net::SocketAddr {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n0123456789");
        }
    });
    addr
}

fn json(body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

#[test]
fn list_mods_parses_array() {
    let url = stub(1, |req| {
        assert_eq!(req.url(), "/v2/mods");
        json(
            r#"[{"id":"a","remoteId":"123","name":"Mod A","description":null,"category":"Skins",
            "author":"Amy","likes":3,"downloadCount":9,"tags":["x"],"images":["i.png"],
            "thumbnailUrl":null,"isNSFW":false,"remoteAddedAt":"2025-01-01T00:00:00.000Z",
            "someFutureField":{"a":1}},
            {"id":"b","remoteId":"124","name":"Mod B"}]"#,
        )
    });
    let mods = Client::new(&url).list_mods().unwrap();
    assert_eq!(mods.len(), 2);
    assert_eq!(mods[0].remote_id, "123");
    assert_eq!(mods[0].download_count, 9);
    assert_eq!(mods[0].category.as_deref(), Some("Skins"));
    assert_eq!(mods[1].name, "Mod B");
    assert!(mods[1].description.is_none());
}

#[test]
fn downloads_parses_files() {
    let url = stub(1, |req| {
        assert_eq!(req.url(), "/v2/mods/123/downloads");
        json(
            r#"{"count":1,"downloads":[{"url":"https://m/x.zip","size":42,"name":"x.zip",
            "description":null,"createdAt":null,"updatedAt":null,"md5Checksum":null}]}"#,
        )
    });
    let files = Client::new(&url).downloads("123").unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].url, "https://m/x.zip");
    assert_eq!(files[0].name, "x.zip");
    assert_eq!(files[0].size, Some(42));
}

#[test]
fn perf_catalog_304_returns_none() {
    let url = stub(1, |req| {
        let inm = req
            .headers()
            .iter()
            .find(|h| h.field.equiv("If-None-Match"))
            .map(|h| h.value.to_string());
        assert_eq!(inm.as_deref(), Some("\"v1\""));
        Response::from_string("").with_status_code(304)
    });
    assert!(
        Client::new(&url)
            .perf_catalog(Some("\"v1\""))
            .unwrap()
            .is_none()
    );
}

#[test]
fn perf_catalog_200_returns_body_and_etag() {
    let url = stub(1, |req| {
        assert_eq!(req.url(), "/v2/perf-catalog");
        json(r#"{"version":"v2"}"#).with_header(Header::from_bytes("ETag", "\"v2\"").unwrap())
    });
    let (etag, body) = Client::new(&url).perf_catalog(None).unwrap().unwrap();
    assert_eq!(etag, "\"v2\"");
    assert_eq!(body, br#"{"version":"v2"}"#);
}

#[test]
fn fetch_writes_file_and_reports_progress() {
    let payload = vec![7u8; 20_000];
    let p = payload.clone();
    let url = stub(1, move |_| Response::from_data(p.clone()));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let mut last = (0, None);
    let mut calls = 0;
    Client::new(&url)
        .fetch(&format!("{url}/f"), &dest, |d, t| {
            last = (d, t);
            calls += 1;
        })
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), payload);
    assert!(calls >= 1);
    assert_eq!(last, (20_000, Some(20_000)));
    assert!(!dir.path().join("m.zip.part").exists());
}

#[test]
fn fetch_removes_partial_file_on_error() {
    // Server announces 1000 bytes, sends 10, then closes: error after bytes were written.
    let addr = truncated_server();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let r = Client::new("http://unused").fetch(&format!("http://{addr}/f"), &dest, |_, _| {});
    assert!(r.is_err());
    assert!(!dest.exists());
    assert!(!dir.path().join("m.zip.part").exists());
}

#[test]
fn fetch_cancellable_stops_and_removes_partial_file() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let payload = vec![7u8; 1_000_000];
    let url = stub(1, move |_| Response::from_data(payload.clone()));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    let r = Client::new(&url).fetch_cancellable(&format!("{url}/f"), &dest, &cancel, |_, _| {
        calls += 1;
        cancel.store(true, Ordering::Relaxed);
    });
    assert!(matches!(r, Err(ApiError::Cancelled)), "{r:?}");
    assert_eq!(calls, 1);
    assert!(!dest.exists());
    assert!(!dir.path().join("m.zip.part").exists());
}

#[test]
fn fetch_cancellable_set_before_start_does_not_request() {
    use std::sync::atomic::AtomicBool;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let r = Client::new("http://unused").fetch_cancellable(
        "http://127.0.0.1:9/f",
        &dest,
        &AtomicBool::new(true),
        |_, _| {},
    );
    assert!(matches!(r, Err(ApiError::Cancelled)), "{r:?}");
    assert!(!dir.path().join("m.zip.part").exists());
}

#[test]
fn fetch_rejects_empty_body() {
    let url = stub(1, |_| Response::from_data(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let r = Client::new(&url).fetch(&format!("{url}/f"), &dest, |_, _| {});
    assert!(matches!(r, Err(ApiError::EmptyDownload)), "{r:?}");
    assert!(!dest.exists());
    assert!(!dir.path().join("m.zip.part").exists());
}

#[test]
fn fetch_rejects_short_body() {
    let addr = truncated_server();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("m.zip");
    let r = Client::new("http://unused").fetch(&format!("http://{addr}/f"), &dest, |_, _| {});
    assert!(
        matches!(
            r,
            Err(ApiError::Incomplete { .. } | ApiError::Io(_) | ApiError::Network(_))
        ),
        "{r:?}"
    );
    assert!(!Path::new(&dest).exists());
    let mut leftovers = String::new();
    for e in std::fs::read_dir(dir.path()).unwrap() {
        leftovers.push_str(&e.unwrap().file_name().to_string_lossy());
    }
    assert!(leftovers.is_empty(), "{leftovers}");
}

#[test]
fn http_error_status_is_reported() {
    let url = stub(1, |_| Response::from_string("nope").with_status_code(404));
    let r = Client::new(&url).downloads("1");
    assert!(
        matches!(r, Err(ApiError::Http { status: 404, .. })),
        "{r:?}"
    );
}

#[test]
fn list_mods_skips_rows_missing_remote_id() {
    let url = stub(1, |_| {
        json(
            r#"[{"id":"a","remoteId":"1","name":"Good"},
            {"id":"b","name":"No remote id"},
            {"id":"c","remoteId":"3","name":"Null likes","likes":null},
            {"id":"d","remoteId":"4","name":"Also good","likes":2}]"#,
        )
    });
    let mods = Client::new(&url).list_mods().unwrap();
    let names: Vec<&str> = mods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["Good", "Also good"]);
}

#[test]
fn list_mods_all_valid_unchanged() {
    let url = stub(1, |_| {
        json(
            r#"[{"id":"a","remoteId":"1","name":"A","likes":1,"downloadCount":5},
            {"id":"b","remoteId":"2","name":"B","description":null}]"#,
        )
    });
    let mods = Client::new(&url).list_mods().unwrap();
    assert_eq!(mods.len(), 2);
    assert_eq!(
        (mods[0].id.as_str(), mods[0].likes, mods[0].download_count),
        ("a", 1, 5)
    );
    assert_eq!(mods[1].remote_id, "2");
}
