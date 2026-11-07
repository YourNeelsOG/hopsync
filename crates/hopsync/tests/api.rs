//! The HTTP API and the CLI socket over real listeners (Phase 2 steps 7 and 8).

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hopsync::client::{Client, Endpoint};
use hopsync::daemon;
use hopsync::service::{Service, ServiceConfig};
use hopsync::ssh::SshOptions;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "correct horse battery staple";

struct Daemon {
    svc: Arc<Service>,
    addr: SocketAddr,
    socket: std::path::PathBuf,
    local: tempfile::TempDir,
    _data: tempfile::TempDir,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

async fn daemon() -> Daemon {
    let data = tempfile::tempdir().expect("tempdir");
    let local = tempfile::tempdir().expect("tempdir");
    let svc = Service::open(ServiceConfig {
        data_dir: data.path().join("data"),
        local_root: local.path().to_owned(),
        concurrency: 2,
        ssh: SshOptions::default(),
    })
    .expect("service");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("addr");
    let socket = data.path().join("run/hopsync.sock");
    let sock = daemon::bind_socket(&socket).await.expect("socket");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let s = svc.clone();
    tokio::spawn(async move {
        let shutdown = async {
            let _ = stopped.await;
        };
        daemon::serve(s, tcp, sock, false, None, shutdown, addr)
            .await
            .expect("serve");
    });
    Daemon {
        svc,
        addr,
        socket,
        local,
        _data: data,
        stop: Some(stop),
    }
}

/// One raw HTTP/1.1 request, for headers the client never sends. Returns the status and the
/// whole response text.
async fn raw(addr: SocketAddr, request: &str) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    s.write_all(request.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, text)
}

/// The script and stylesheet paths named by the UI page.
fn shipped_paths(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        for part in page.split(attr).skip(1) {
            let path = part.split('"').next().unwrap_or_default();
            if path.ends_with(".js") || path.ends_with(".css") {
                out.push(path.to_owned());
            }
        }
    }
    out
}

async fn logged_in(d: &Daemon) -> Client {
    let mut c = Client::new(Endpoint::Tcp(d.addr));
    let token = d.svc.auth.setup_token().expect("setup token");
    let r = c
        .post(
            "/api/setup",
            json!({ "token": token, "password": PASSWORD }),
        )
        .await
        .expect("setup");
    assert_eq!(r.status, 200, "{:?}", r.body);
    c.cookie = r.set_cookie;
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_works_before_setup_and_everything_needs_a_session() {
    let d = daemon().await;
    let c = Client::new(Endpoint::Tcp(d.addr));
    let health = c
        .get("/api/health")
        .await
        .expect("health")
        .ok()
        .expect("ok");
    assert_eq!(health["setup_required"], true);
    assert!(
        !health
            .to_string()
            .contains(&d.svc.auth.setup_token().expect("token")),
        "the setup token is never sent"
    );
    assert_eq!(c.get("/api/servers").await.expect("req").status, 403);
    let wrong = c
        .post(
            "/api/setup",
            json!({ "token": "nope", "password": PASSWORD }),
        )
        .await
        .expect("req");
    assert_eq!(wrong.status, 401);

    // The token is in a private file, never in the log; setup removes the file.
    let token_file = d._data.path().join("data/setup-token");
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&token_file)
            .expect("token file")
            .permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(&token_file).expect("read").trim(),
        d.svc.auth.setup_token().expect("token")
    );
    let authed = logged_in(&d).await;
    assert!(!token_file.exists(), "setup removes the token file");
    assert!(
        authed
            .cookie
            .as_deref()
            .is_some_and(|c| c.starts_with("hopsync_session=")),
        "{:?}",
        authed.cookie
    );
    assert_eq!(authed.get("/api/servers").await.expect("req").status, 200);
    for path in [
        "/api/servers",
        "/api/jobs",
        "/api/local/list",
        "/api/audit",
        "/api/events",
    ] {
        assert_eq!(c.get(path).await.expect("req").status, 401, "{path}");
    }
    let bad = Client {
        cookie: Some(format!("hopsync_session={}", "0".repeat(64))),
        ..c.clone()
    };
    assert_eq!(bad.get("/api/jobs").await.expect("req").status, 401);

    let out = authed.post("/api/logout", json!({})).await.expect("logout");
    assert_eq!(out.status, 200);
    assert_eq!(authed.get("/api/jobs").await.expect("req").status, 401);

    let again = c
        .post("/api/login", json!({ "password": PASSWORD }))
        .await
        .expect("login");
    assert_eq!(again.status, 200);
    let denied = c
        .post("/api/login", json!({ "password": "wrong password!!" }))
        .await
        .expect("login");
    assert_eq!(denied.status, 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn cross_origin_and_form_posts_are_refused() {
    let d = daemon().await;
    let c = logged_in(&d).await;
    let cookie = c.cookie.clone().expect("cookie");
    let host = d.addr;
    let body = r#"{"password":"x"}"#;
    let post = |origin: &str, ctype: &str| {
        format!(
            "POST /api/queue/pause HTTP/1.1\r\nHost: {host}\r\n{origin}Cookie: {cookie}\r\n\
             Content-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let (status, _) = raw(
        d.addr,
        &post("Origin: http://evil.example\r\n", "application/json"),
    )
    .await;
    assert_eq!(status, 403, "foreign origin");
    let (status, _) = raw(d.addr, &post("", "application/json")).await;
    assert_eq!(status, 403, "no origin at all");
    let (status, _) = raw(
        d.addr,
        &post(
            "Sec-Fetch-Site: cross-site\r\nOrigin: http://evil.example\r\n",
            "application/json",
        ),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = raw(
        d.addr,
        &post(&format!("Origin: http://{host}\r\n"), "text/plain"),
    )
    .await;
    assert_eq!(status, 415, "a form-style body type");
    let (status, _) = raw(
        d.addr,
        &post(&format!("Origin: http://{host}\r\n"), "application/json"),
    )
    .await;
    assert_eq!(status, 200);
    assert!(d.svc.queue_paused());

    let (_, text) = raw(
        d.addr,
        &format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    let lower = text.to_ascii_lowercase();
    for h in [
        "cache-control: no-store",
        "x-content-type-options: nosniff",
        "x-frame-options: deny",
        "content-security-policy: default-src 'self'",
        "referrer-policy: no-referrer",
    ] {
        assert!(lower.contains(h), "missing {h} in {text}");
    }
    let (_, login) = raw(
        d.addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            json!({ "password": PASSWORD }).to_string().len(),
            json!({ "password": PASSWORD })
        ),
    )
    .await;
    let cookie_line = login
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("cookie");
    assert!(cookie_line.contains("HttpOnly") && cookie_line.contains("SameSite=Strict"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_local_socket_needs_no_login() {
    let d = daemon().await;
    let local = Client::new(Endpoint::Unix(d.socket.clone()));
    // Even before setup: being this user already gives access to the data directory.
    std::fs::write(d.local.path().join("x.txt"), b"x").expect("write");
    let listed = local
        .get("/api/local/list")
        .await
        .expect("list")
        .ok()
        .expect("ok");
    assert_eq!(listed[0]["name"], "x.txt");
    assert_eq!(listed[0]["kind"], "file");
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(&d.socket).expect("stat").permissions(),
    );
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_job_runs_end_to_end_over_the_api_with_live_events() {
    let Some(sshd) = support::TestSshd::start() else {
        return;
    };
    let d = daemon().await;
    let c = logged_in(&d).await;
    let remote = sshd.dir().join("remote");
    std::fs::create_dir(&remote).expect("mkdir");
    std::fs::write(d.local.path().join("movie.mkv"), vec![9u8; 3 << 20]).expect("write");
    let (_pins, _known, target) = sshd.client();

    let server = c
        .post(
            "/api/servers",
            json!({
                "name": "box",
                "host": sshd.addr().ip().to_string(),
                "port": sshd.addr().port(),
                "user": target.user,
                "root": remote.to_str().expect("utf8"),
                "key_file": sshd.client_key().to_str().expect("utf8"),
            }),
        )
        .await
        .expect("add")
        .ok()
        .expect("created");
    let id = server["id"].as_i64().expect("id");
    assert_eq!(server["blocked"], Value::Null);
    let probe = c
        .post(&format!("/api/servers/{id}/probe"), json!({}))
        .await
        .expect("probe")
        .ok()
        .expect("ok");
    assert_eq!(probe["status"], "unknown");
    c.post(
        &format!("/api/servers/{id}/trust"),
        json!({ "fingerprint": probe["fingerprint"] }),
    )
    .await
    .expect("trust")
    .ok()
    .expect("trusted");

    // Live events: the first snapshot comes at once.
    let mut sse = tokio::net::TcpStream::connect(d.addr)
        .await
        .expect("connect");
    sse.write_all(
        format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
            d.addr,
            c.cookie.clone().expect("cookie")
        )
        .as_bytes(),
    )
    .await
    .expect("write");

    let plan = c
        .post(
            "/api/jobs/dry-run",
            json!({ "direction": "push", "server": id, "src": "movie.mkv", "dst": "movie.mkv" }),
        )
        .await
        .expect("dry run")
        .ok()
        .expect("ok");
    assert_eq!(plan["bytes"], 3 << 20);
    assert_eq!(plan["items"][0]["outcome"], "create");

    let job = c
        .post(
            "/api/jobs",
            json!({ "direction": "push", "server": id, "src": "movie.mkv", "dst": "movie.mkv", "verify": true }),
        )
        .await
        .expect("queue")
        .ok()
        .expect("created");
    let jid = job["id"].as_i64().expect("id");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let done = loop {
        let j = c
            .get(&format!("/api/jobs/{jid}"))
            .await
            .expect("job")
            .ok()
            .expect("ok");
        if j["state"] == "done" || j["state"] == "failed" {
            break j;
        }
        assert!(tokio::time::Instant::now() < deadline, "stuck: {j}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["direction"], "push");
    assert_eq!(
        std::fs::metadata(remote.join("movie.mkv"))
            .expect("stat")
            .len(),
        3 << 20
    );
    let items = c
        .get(&format!("/api/jobs/{jid}/items"))
        .await
        .expect("items")
        .ok()
        .expect("ok");
    assert_eq!(items[0]["outcome"], "sent");
    let events = c
        .get(&format!("/api/jobs/{jid}/events"))
        .await
        .expect("events")
        .ok()
        .expect("ok");
    let texts: Vec<&str> = events
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|e| e["text"].as_str())
        .collect();
    assert_eq!(texts, ["queued", "started", "done"]);

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(5), sse.read(&mut buf))
        .await
        .expect("sse in time")
        .expect("read");
    let text = String::from_utf8_lossy(&buf[..n]);
    assert!(text.contains("text/event-stream"), "{text}");
    assert!(text.contains("event: snapshot"), "{text}");

    let remote_list = c
        .get(&format!("/api/servers/{id}/list?path="))
        .await
        .expect("list")
        .ok()
        .expect("ok");
    assert_eq!(remote_list[0]["name"], "movie.mkv");
    let traversal = c
        .get(&format!("/api/servers/{id}/list?path=../.."))
        .await
        .expect("list");
    assert_eq!(traversal.status, 400);
    let audit = c.get("/api/audit").await.expect("audit").ok().expect("ok");
    let kinds: Vec<&str> = audit
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|e| e["event"].as_str())
        .collect();
    assert!(kinds.contains(&"host-key-trusted") && kinds.contains(&"server-added"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebound_hostname_is_refused_on_a_loopback_listener() {
    let d = daemon().await;
    let port = d.addr.port();
    // A page on evil.example whose DNS now points at 127.0.0.1: Origin and Host agree with each
    // other, so only the Host check stops it.
    let body = json!({ "password": PASSWORD }).to_string();
    let (status, _) = raw(
        d.addr,
        &format!(
            "POST /api/login HTTP/1.1\r\nHost: evil.example:{port}\r\nOrigin: http://evil.example:{port}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert_eq!(status, 421);
    let (status, _) = raw(
        d.addr,
        &format!(
            "GET /api/health HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n"
        ),
    )
    .await;
    assert_eq!(status, 421);
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        "localhost".to_owned(),
    ] {
        let (status, _) = raw(
            d.addr,
            &format!("GET /api/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 200, "{host}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_ui_is_served_from_the_binary_with_strict_headers() {
    let d = daemon().await;
    let host = d.addr;
    let get =
        |path: &str| format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let (status, page) = raw(d.addr, &get("/")).await;
    assert_eq!(status, 200);
    let lower = page.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: text/html; charset=utf-8"),
        "{page}"
    );
    assert!(lower.contains("content-security-policy: default-src 'self'"));
    assert!(
        !page.contains("<script>") && !page.contains("style=\""),
        "no inline code"
    );
    // A checkout without a built UI serves a page that says how to build it.
    let paths = shipped_paths(&page);
    if paths.is_empty() {
        assert!(page.contains("pnpm"), "{page}");
    }
    assert!(paths.iter().all(|p| p.starts_with("/assets/")), "{paths:?}");
    for path in &paths {
        let (status, text) = raw(d.addr, &get(path)).await;
        let lower = text.to_ascii_lowercase();
        assert_eq!(status, 200, "{path}");
        assert!(lower.contains("content-type: text/"), "{path}: {text:.300}");
        assert!(
            lower.contains("cache-control: public, max-age=31536000, immutable"),
            "hashed files are cached: {text:.300}"
        );
        assert!(!lower.contains("no-store"), "{text:.300}");
        assert!(lower.contains("x-content-type-options: nosniff"));
        assert!(!lower.contains("content-encoding"), "{text:.300}");
        // Gzip only when the browser says it can read it.
        let gzipped = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept-Encoding: gzip, br\r\nConnection: close\r\n\r\n"
        );
        let (status, text) = raw(d.addr, &gzipped).await;
        let lower = text.to_ascii_lowercase();
        assert_eq!(status, 200, "{path}");
        assert!(lower.contains("content-encoding: gzip"), "{text:.300}");
        assert!(lower.contains("vary: accept-encoding"), "{text:.300}");
        let refused = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept-Encoding: gzip;q=0\r\nConnection: close\r\n\r\n"
        );
        let (_, text) = raw(d.addr, &refused).await;
        assert!(
            !text.to_ascii_lowercase().contains("content-encoding"),
            "{text:.300}"
        );
    }
    // The placeholder page has no icon, so `cargo test` passes on a fresh checkout too.
    if !paths.is_empty() {
        let (status, text) = raw(d.addr, &get("/icon.svg")).await;
        assert_eq!(status, 200);
        let lower = text.to_ascii_lowercase();
        assert!(lower.contains("content-type: image/svg+xml"), "{text:.300}");
        assert!(lower.contains("cache-control: no-store"), "{text:.300}");
    }
    let (status, _) = raw(d.addr, &get("/assets/nope.js")).await;
    assert_eq!(status, 404);
    let (status, _) = raw(d.addr, &get("/assets/../../Cargo.toml")).await;
    assert_ne!(status, 200);
    let (status, _) = raw(d.addr, &get("/nope.js")).await;
    assert_eq!(status, 404);
    let (status, _) = raw(d.addr, &get("/../Cargo.toml")).await;
    assert_ne!(status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_are_validated_and_kept() {
    let d = daemon().await;
    let c = logged_in(&d).await;
    let got = c.get("/api/settings").await.expect("get").ok().expect("ok");
    assert_eq!(
        got,
        json!({ "default_existing": "skip", "default_verify": false, "local_min_free": 0 })
    );
    let put = |body: Value| {
        let c = c.clone();
        async move {
            c.request(hopsync::client::Method::PUT, "/api/settings", Some(body))
                .await
                .expect("put")
        }
    };
    assert_eq!(
        put(json!({ "default_existing": "explode" })).await.status,
        400
    );
    assert_eq!(put(json!({ "unknown": 1 })).await.status, 400);
    let r = put(json!({ "default_existing": "update", "local_min_free": 1073741824u64 })).await;
    assert_eq!(r.status, 200, "{:?}", r.body);
    let got = c.get("/api/settings").await.expect("get").ok().expect("ok");
    assert_eq!(
        got,
        json!({ "default_existing": "update", "default_verify": false, "local_min_free": 1073741824u64 })
    );
}

/// Bug found driving the web UI: an open browser tab holds `/api/events` forever, and graceful
/// shutdown waited for it, so SIGTERM never stopped the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_does_not_wait_for_open_event_streams() {
    let data = tempfile::tempdir().expect("tempdir");
    let svc = Service::open(ServiceConfig {
        data_dir: data.path().join("data"),
        local_root: data.path().to_owned(),
        concurrency: 1,
        ssh: SshOptions::default(),
    })
    .expect("service");
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = tcp.local_addr().expect("addr");
    let sock = daemon::bind_socket(&data.path().join("run/hopsync.sock"))
        .await
        .expect("socket");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(daemon::serve(
        svc.clone(),
        tcp,
        sock,
        false,
        None,
        async {
            let _ = stopped.await;
        },
        addr,
    ));

    let token = svc.auth.setup_token().expect("setup token");
    let r = Client::new(Endpoint::Tcp(addr))
        .post(
            "/api/setup",
            json!({ "token": token, "password": PASSWORD }),
        )
        .await
        .expect("setup");
    let cookie = r.set_cookie.expect("cookie");
    let mut sse = tokio::net::TcpStream::connect(addr).await.expect("connect");
    sse.write_all(
        format!("GET /api/events HTTP/1.1\r\nHost: {addr}\r\nCookie: {cookie}\r\n\r\n").as_bytes(),
    )
    .await
    .expect("write");
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), sse.read(&mut buf))
        .await
        .expect("first snapshot in time")
        .expect("read");
    assert!(
        String::from_utf8_lossy(&buf[..n]).contains("text/event-stream"),
        "the stream is open"
    );

    stop.send(()).expect("stop");
    let done = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(
        done.is_ok(),
        "serve must return while an event stream is open"
    );
    // The stream ends instead of being cut mid-event.
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), sse.read_to_end(&mut rest)).await;
}

/// Found in the Phase 3 review: an event stream opened before logout (or a password change, which
/// ends every session) kept delivering snapshots on the revoked session.
#[tokio::test(flavor = "multi_thread")]
async fn an_event_stream_ends_when_its_session_does() {
    let d = daemon().await;
    let c = logged_in(&d).await;
    let mut sse = tokio::net::TcpStream::connect(d.addr)
        .await
        .expect("connect");
    sse.write_all(
        format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
            d.addr,
            c.cookie.clone().expect("cookie")
        )
        .as_bytes(),
    )
    .await
    .expect("write");
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), sse.read(&mut buf))
        .await
        .expect("first snapshot in time")
        .expect("read");
    let first = String::from_utf8_lossy(&buf[..n]).into_owned();
    assert!(first.contains("\"queue_paused\":false"), "{first}");

    c.post("/api/logout", json!({}))
        .await
        .expect("logout")
        .ok()
        .expect("logged out");
    d.svc.pause_queue().expect("hold");

    // The body ends with the last chunk; the connection itself may stay open for keep-alive.
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        while !rest.ends_with(b"0\r\n\r\n") {
            match sse.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    let rest = String::from_utf8_lossy(&rest);
    assert!(
        !rest.contains("\"queue_paused\":true"),
        "no snapshot after logout: {rest}"
    );
    assert!(ended.is_ok(), "the stream must end after logout");
}

/// Keep-alive comments never reach `EventSource`, so a stalled stream looked like an idle one. A
/// named event with data is what the UI's watchdog can see.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_event_stream_sends_heartbeats_the_browser_can_see() {
    let d = daemon().await;
    let c = logged_in(&d).await;
    let mut sse = tokio::net::TcpStream::connect(d.addr)
        .await
        .expect("connect");
    sse.write_all(
        format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
            d.addr,
            c.cookie.clone().expect("cookie")
        )
        .as_bytes(),
    )
    .await
    .expect("write");
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let found = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let n = sse.read(&mut buf).await.expect("read");
            seen.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&seen);
            // Browsers drop an event without data.
            if text.contains("event: heartbeat\ndata: ") {
                break;
            }
            assert!(n > 0, "stream ended: {text}");
        }
    })
    .await;
    assert!(
        found.is_ok(),
        "no heartbeat: {}",
        String::from_utf8_lossy(&seen)
    );
}

/// Found by the browser test: the session was re-checked only when a snapshot changed, so on an idle
/// daemon a revoked session's stream stayed open forever and the page never showed the login.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_event_stream_ends_when_its_session_does() {
    let d = daemon().await;
    let c = logged_in(&d).await;
    let mut sse = tokio::net::TcpStream::connect(d.addr)
        .await
        .expect("connect");
    sse.write_all(
        format!(
            "GET /api/events HTTP/1.1\r\nHost: {}\r\nCookie: {}\r\n\r\n",
            d.addr,
            c.cookie.clone().expect("cookie")
        )
        .as_bytes(),
    )
    .await
    .expect("write");
    let mut buf = [0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), sse.read(&mut buf))
        .await
        .expect("first snapshot in time")
        .expect("read");
    assert!(n > 0);
    c.post("/api/logout", json!({}))
        .await
        .expect("logout")
        .ok()
        .expect("logged out");
    // Nothing changes on the daemon from here on.
    let mut rest = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(8), async {
        while !rest.ends_with(b"0\r\n\r\n") {
            match sse.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => rest.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the stream must end within a heartbeat");
}
