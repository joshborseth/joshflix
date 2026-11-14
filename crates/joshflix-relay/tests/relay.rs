//! End-to-end tests: a real relay on 127.0.0.1 talking to a fake Real-Debrid.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode, header},
    routing::get,
};
use joshflix_protocol::{
    ApiError, CreatePairingResponse, PairingPoll, PairingState, PhonePairingStatus,
    SubmitKeyResponse, UPDATE_MANIFEST_PATH, UpdateApk, UpdateManifest,
};
use joshflix_relay::{AppState, Config, ratelimit::Limit, router};
use serde_json::json;
use tower::ServiceExt as _;

const GOOD_KEY: &str = "GOODKEY0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const BAD_KEY: &str = "BADKEY00123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const FLAKY_KEY: &str = "FLAKYKEY0123456789ABCDEFGHIJKLMNOPQRSTUVWX";

/// Fake `GET /user`: GOOD_KEY → user "josh", FLAKY_KEY → 503, anything else → 401.
async fn spawn_fake_rd() -> (String, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let app = Router::new().route(
        "/user",
        get(move |headers: HeaderMap| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let auth = headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok());
                match auth {
                    Some(a) if a == format!("Bearer {GOOD_KEY}") => (
                        StatusCode::OK,
                        Json(json!({"id": 1, "username": "josh", "type": "premium"})),
                    ),
                    Some(a) if a == format!("Bearer {FLAKY_KEY}") => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error": "down"})),
                    ),
                    _ => (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "bad_token", "error_code": 8})),
                    ),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake rd");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake rd") });
    (format!("http://{addr}"), calls)
}

struct TestDir(PathBuf);

impl TestDir {
    fn new(name: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("relay-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("updates")).expect("create test dir");
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(dir: &TestDir, rd_base: &str) -> Config {
    let mut config = Config::new(&dir.0);
    config.rd_api_base = rd_base.to_owned();
    config.rd_timeout = Duration::from_secs(5);
    config
}

struct Relay {
    base: String,
    state: AppState,
    http: reqwest::Client,
}

impl Relay {
    async fn start(config: Config) -> Self {
        let state = AppState::new(config).expect("state");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind relay");
        let addr = listener.local_addr().expect("addr");
        let app = router(state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.expect("relay") });
        // reqwest is built with `rustls-no-provider`; the test client only
        // speaks plain HTTP but still needs a provider to be built.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        Self {
            base: format!("http://{addr}"),
            state,
            http,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn create(&self) -> CreatePairingResponse {
        let r = self
            .http
            .post(self.url("/api/v1/pairings"))
            .send()
            .await
            .expect("create");
        assert_eq!(r.status(), StatusCode::CREATED);
        r.json().await.expect("create json")
    }

    async fn poll_raw(&self, code: &str, secret: &str, wait: u64) -> reqwest::Response {
        self.http
            .get(self.url(&format!("/api/v1/pairings/{code}?wait={wait}")))
            .bearer_auth(secret)
            .send()
            .await
            .expect("poll")
    }

    async fn poll(&self, p: &CreatePairingResponse, wait: u64) -> PairingPoll {
        let r = self.poll_raw(&p.code, &p.tv_secret, wait).await;
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.expect("poll json")
    }

    async fn status(&self, code: &str) -> PhonePairingStatus {
        let r = self
            .http
            .get(self.url(&format!("/api/v1/pairings/{code}/status")))
            .send()
            .await
            .expect("status");
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.expect("status json")
    }

    async fn submit(&self, code: &str, key: &str) -> reqwest::Response {
        self.http
            .post(self.url(&format!("/api/v1/pairings/{code}/key")))
            .json(&json!({ "api_key": key }))
            .send()
            .await
            .expect("submit")
    }
}

async fn api_error(r: reqwest::Response, status: StatusCode) -> ApiError {
    assert_eq!(r.status(), status);
    r.json().await.expect("api error json")
}

#[tokio::test]
async fn full_pairing_flow() {
    let dir = TestDir::new("flow");
    let (rd, calls) = spawn_fake_rd().await;
    let relay = Relay::start(config(&dir, &rd)).await;

    assert_eq!(
        relay
            .http
            .get(relay.url("/healthz"))
            .send()
            .await
            .expect("health")
            .text()
            .await
            .expect("body"),
        "ok"
    );

    let p = relay.create().await;
    assert!(joshflix_protocol::is_valid_pairing_code(&p.code));
    assert_eq!(p.tv_secret.len(), 43);
    assert_eq!(p.phone_url, relay.url(&format!("/p/{}", p.code)));
    assert_eq!(p.expires_in_secs, 600);

    let first = relay.poll(&p, 0).await;
    assert_eq!(first.state, PairingState::Pending);
    assert!(first.api_key.is_none());
    assert!(first.expires_in_secs > 590);

    // Phone opens the page.
    let page = relay.http.get(&p.phone_url).send().await.expect("page");
    assert_eq!(page.status(), StatusCode::OK);
    let csp = page
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("csp")
        .to_str()
        .expect("csp str")
        .to_owned();
    assert!(csp.contains("default-src 'none'"));
    let html = page.text().await.expect("html");
    assert!(html.contains(&format!(r#"data-code="{}""#, p.code)));

    assert_eq!(relay.poll(&p, 0).await.state, PairingState::PhoneOpened);
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Pending);

    // Phone submits a good key.
    let r = relay.submit(&p.code, GOOD_KEY).await;
    assert_eq!(r.status(), StatusCode::OK);
    let ok: SubmitKeyResponse = r.json().await.expect("submit json");
    assert_eq!(ok.username, "josh");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Submitted);

    // TV collects it exactly once.
    let delivered = relay.poll(&p, 5).await;
    assert_eq!(delivered.state, PairingState::Delivered);
    assert_eq!(delivered.api_key.as_deref(), Some(GOOD_KEY));
    assert_eq!(delivered.username.as_deref(), Some("josh"));

    let again = relay.poll(&p, 5).await;
    assert_eq!(again.state, PairingState::Delivered);
    assert_eq!(again.api_key, None);
    assert_eq!(again.username.as_deref(), Some("josh"));
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Submitted);

    // Second submission is refused without calling Real-Debrid.
    let err = api_error(relay.submit(&p.code, GOOD_KEY).await, StatusCode::CONFLICT).await;
    assert_eq!(err.code, "already_submitted");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // TV cancels; afterwards the code is gone.
    let r = relay
        .http
        .delete(relay.url(&format!("/api/v1/pairings/{}", p.code)))
        .bearer_auth(&p.tv_secret)
        .send()
        .await
        .expect("delete");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        api_error(
            relay.poll_raw(&p.code, &p.tv_secret, 0).await,
            StatusCode::NOT_FOUND
        )
        .await
        .code,
        "expired"
    );
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Missing);
}

#[tokio::test]
async fn submit_before_tv_polls_then_double_submit_conflicts() {
    let dir = TestDir::new("double");
    let (rd, _) = spawn_fake_rd().await;
    let relay = Relay::start(config(&dir, &rd)).await;
    let p = relay.create().await;
    assert_eq!(
        relay.submit(&p.code, GOOD_KEY).await.status(),
        StatusCode::OK
    );
    let err = api_error(relay.submit(&p.code, GOOD_KEY).await, StatusCode::CONFLICT).await;
    assert_eq!(err.code, "already_submitted");
    // Waiting key returns immediately even with a long wait.
    let started = Instant::now();
    let poll = relay.poll(&p, 30).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(poll.api_key.as_deref(), Some(GOOD_KEY));
}

#[tokio::test]
async fn wrong_or_missing_tv_secret_is_401() {
    let dir = TestDir::new("secret");
    let (rd, _) = spawn_fake_rd().await;
    let relay = Relay::start(config(&dir, &rd)).await;
    let p = relay.create().await;
    relay.submit(&p.code, GOOD_KEY).await;

    let wrong = format!("{}x", p.tv_secret);
    assert_eq!(
        api_error(
            relay.poll_raw(&p.code, &wrong, 0).await,
            StatusCode::UNAUTHORIZED
        )
        .await
        .code,
        "unauthorized"
    );
    let r = relay
        .http
        .get(relay.url(&format!("/api/v1/pairings/{}", p.code)))
        .send()
        .await
        .expect("poll");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = relay
        .http
        .delete(relay.url(&format!("/api/v1/pairings/{}", p.code)))
        .bearer_auth("nope")
        .send()
        .await
        .expect("delete");
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // The key is still there for the real TV.
    assert_eq!(relay.poll(&p, 0).await.api_key.as_deref(), Some(GOOD_KEY));
}

#[tokio::test]
async fn invalid_keys_are_rejected() {
    let dir = TestDir::new("invalid");
    let (rd, calls) = spawn_fake_rd().await;
    let relay = Relay::start(config(&dir, &rd)).await;
    let p = relay.create().await;

    // Malformed: rejected without calling Real-Debrid.
    for key in ["short", "has spaces in it 1234567890", &"x".repeat(101)] {
        let err = api_error(relay.submit(&p.code, key).await, StatusCode::BAD_REQUEST).await;
        assert_eq!(err.code, "invalid_key");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // Well-formed but Real-Debrid says 401.
    let err = api_error(
        relay.submit(&p.code, BAD_KEY).await,
        StatusCode::BAD_REQUEST,
    )
    .await;
    assert_eq!(err.code, "invalid_key");
    assert!(!err.message.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Real-Debrid down: 502.
    let err = api_error(
        relay.submit(&p.code, FLAKY_KEY).await,
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(err.code, "rd_unreachable");

    // Not JSON.
    let r = relay
        .http
        .post(relay.url(&format!("/api/v1/pairings/{}/key", p.code)))
        .body("api_key=x")
        .send()
        .await
        .expect("submit");
    assert_eq!(
        api_error(r, StatusCode::BAD_REQUEST).await.code,
        "bad_request"
    );

    // Still pending, and a good key then works.
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Pending);
    assert_eq!(
        relay.submit(&p.code, GOOD_KEY).await.status(),
        StatusCode::OK
    );

    // Unknown and malformed codes.
    let err = api_error(
        relay.submit("AAAAAAAAAAAAAAAAAAAAAA", GOOD_KEY).await,
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(err.code, "expired");
    let err = api_error(relay.submit("nope", GOOD_KEY).await, StatusCode::NOT_FOUND).await;
    assert_eq!(err.code, "expired");
}

#[tokio::test]
async fn unreachable_real_debrid_is_502() {
    let dir = TestDir::new("unreachable");
    // Bind and drop to get a port nobody listens on.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let relay = Relay::start(config(&dir, &format!("http://127.0.0.1:{port}"))).await;
    let p = relay.create().await;
    let err = api_error(
        relay.submit(&p.code, GOOD_KEY).await,
        StatusCode::BAD_GATEWAY,
    )
    .await;
    assert_eq!(err.code, "rd_unreachable");
}

#[tokio::test]
async fn pairings_expire() {
    let dir = TestDir::new("expiry");
    let (rd, _) = spawn_fake_rd().await;
    let mut cfg = config(&dir, &rd);
    cfg.pairing_ttl = Duration::from_secs(1);
    cfg.expired_grace = Duration::from_millis(500);
    let relay = Relay::start(cfg).await;
    let p = relay.create().await;
    assert_eq!(p.expires_in_secs, 1);

    // A long poll ends at expiry, not at the requested wait.
    let started = Instant::now();
    let poll = relay.poll(&p, 10).await;
    assert_eq!(poll.state, PairingState::Expired);
    assert_eq!(poll.expires_in_secs, 0);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );

    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Missing);
    let err = api_error(relay.submit(&p.code, GOOD_KEY).await, StatusCode::NOT_FOUND).await;
    assert_eq!(err.code, "expired");
    let page = relay.http.get(&p.phone_url).send().await.expect("page");
    assert_eq!(page.status(), StatusCode::NOT_FOUND);
    assert!(page.text().await.expect("html").contains(r#"data-code="""#));

    // After the grace period the sweeper forgets it entirely.
    tokio::time::sleep(Duration::from_millis(600)).await;
    relay.state.sweep();
    assert_eq!(
        api_error(
            relay.poll_raw(&p.code, &p.tv_secret, 0).await,
            StatusCode::NOT_FOUND
        )
        .await
        .code,
        "expired"
    );
}

#[tokio::test]
async fn long_poll_wakes_on_phone_open_and_submit() {
    let dir = TestDir::new("longpoll");
    let (rd, _) = spawn_fake_rd().await;
    let relay = Arc::new(Relay::start(config(&dir, &rd)).await);
    let p = Arc::new(relay.create().await);

    // TV is waiting; phone opens the page.
    let waiter = {
        let (relay, p) = (relay.clone(), p.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            (relay.poll(&p, 20).await, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !waiter.is_finished(),
        "poll should wait while nothing changes"
    );
    assert_eq!(relay.status(&p.code).await, PhonePairingStatus::Pending);
    let (poll, took) = waiter.await.expect("join");
    assert_eq!(poll.state, PairingState::PhoneOpened);
    assert!(took < Duration::from_secs(5), "{took:?}");

    // TV waits again; phone submits.
    let waiter = {
        let (relay, p) = (relay.clone(), p.clone());
        tokio::spawn(async move {
            let started = Instant::now();
            (relay.poll(&p, 20).await, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiter.is_finished());
    assert_eq!(
        relay.submit(&p.code, GOOD_KEY).await.status(),
        StatusCode::OK
    );
    let (poll, took) = waiter.await.expect("join");
    assert_eq!(poll.state, PairingState::Delivered);
    assert_eq!(poll.api_key.as_deref(), Some(GOOD_KEY));
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[tokio::test]
async fn long_poll_honours_wait_and_wakes_on_cancel() {
    let dir = TestDir::new("cancel");
    let (rd, _) = spawn_fake_rd().await;
    let relay = Arc::new(Relay::start(config(&dir, &rd)).await);
    let p = Arc::new(relay.create().await);

    let started = Instant::now();
    let poll = relay.poll(&p, 1).await;
    assert_eq!(poll.state, PairingState::Pending);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );

    let waiter = {
        let (relay, p) = (relay.clone(), p.clone());
        tokio::spawn(async move { relay.poll(&p, 20).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = relay
        .http
        .delete(relay.url(&format!("/api/v1/pairings/{}", p.code)))
        .bearer_auth(&p.tv_secret)
        .send()
        .await
        .expect("delete");
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let poll = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("woke")
        .expect("join");
    assert_eq!(poll.state, PairingState::Expired);
}

#[tokio::test]
async fn caps_and_rate_limits() {
    let dir = TestDir::new("limits");
    let (rd, calls) = spawn_fake_rd().await;
    let mut cfg = config(&dir, &rd);
    cfg.max_active_pairings = 2;
    cfg.submit_limit = Limit {
        max: 3,
        per: Duration::from_secs(60),
    };
    let relay = Relay::start(cfg).await;
    let p = relay.create().await;
    relay.create().await;
    let r = relay
        .http
        .post(relay.url("/api/v1/pairings"))
        .send()
        .await
        .expect("create");
    assert_eq!(
        api_error(r, StatusCode::TOO_MANY_REQUESTS).await.code,
        "rate_limited"
    );

    for _ in 0..3 {
        assert_eq!(
            relay.submit(&p.code, BAD_KEY).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let err = api_error(
        relay.submit(&p.code, GOOD_KEY).await,
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    assert_eq!(err.code, "rate_limited");
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    let dir2 = TestDir::new("limits2");
    let mut cfg = config(&dir2, &rd);
    cfg.create_limit = Limit {
        max: 2,
        per: Duration::from_secs(60),
    };
    let relay = Relay::start(cfg).await;
    relay.create().await;
    relay.create().await;
    let r = relay
        .http
        .post(relay.url("/api/v1/pairings"))
        .send()
        .await
        .expect("create");
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// Sends one request through the router as if it came from `peer`.
async fn oneshot(state: &AppState, peer: &str, request: Request<Body>) -> axum::response::Response {
    let peer: SocketAddr = peer.parse().expect("peer");
    let mut request = request;
    request.extensions_mut().insert(ConnectInfo(peer));
    router(state.clone())
        .oneshot(request)
        .await
        .expect("infallible")
}

#[tokio::test]
async fn network_guard_rejects_public_peers() {
    let dir = TestDir::new("guard");
    let state = AppState::new(config(&dir, "http://127.0.0.1:9")).expect("state");
    let get = |path: &str| Request::get(path).body(Body::empty()).expect("request");

    let r = oneshot(&state, "8.8.8.8:5000", get("/healthz")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = oneshot(&state, "[2001:db8::1]:5000", get("/healthz")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    for ok in [
        "192.168.1.20:5000",
        "100.101.102.103:1",
        "[fd7a:115c:a1e0::5]:1",
        "[::ffff:10.0.0.2]:1",
    ] {
        let r = oneshot(&state, ok, get("/healthz")).await;
        assert_eq!(r.status(), StatusCode::OK, "{ok}");
    }

    // No peer info at all: fail closed.
    let r = router(state.clone())
        .oneshot(get("/healthz"))
        .await
        .expect("infallible");
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn phone_url_uses_host_or_public_url() {
    let dir = TestDir::new("host");
    let state = AppState::new(config(&dir, "http://127.0.0.1:9")).expect("state");
    let post = |host: &str| {
        Request::post("/api/v1/pairings")
            .header(header::HOST, host)
            .body(Body::empty())
            .expect("request")
    };

    let r = oneshot(&state, "192.168.1.20:1", post("192.168.1.10:8788")).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(r.into_body(), 4096)
        .await
        .expect("body");
    let created: CreatePairingResponse = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        created.phone_url,
        format!("http://192.168.1.10:8788/p/{}", created.code)
    );

    let r = oneshot(&state, "192.168.1.20:1", post("evil.com/x?")).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    let mut cfg = config(&dir, "http://127.0.0.1:9");
    cfg.public_url = Some("http://100.100.1.2:8788".into());
    let state = AppState::new(cfg).expect("state");
    let r = oneshot(&state, "192.168.1.20:1", post("whatever:1")).await;
    let body = axum::body::to_bytes(r.into_body(), 4096)
        .await
        .expect("body");
    let created: CreatePairingResponse = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        created.phone_url,
        format!("http://100.100.1.2:8788/p/{}", created.code)
    );
}

#[tokio::test]
async fn invalid_code_renders_missing_page() {
    let dir = TestDir::new("badpage");
    let (rd, _) = spawn_fake_rd().await;
    let relay = Relay::start(config(&dir, &rd)).await;
    for code in [
        "short",
        "AAAAAAAAAAAAAAAAAAAAAA",
        "%22%3E%3Cscript%3Ealert(1)%3C%2Fscript%3E",
    ] {
        let r = relay
            .http
            .get(relay.url(&format!("/p/{code}")))
            .send()
            .await
            .expect("page");
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let html = r.text().await.expect("html");
        assert!(html.contains(r#"data-code="""#));
        assert!(!html.contains("alert(1)"));
    }
    assert_eq!(relay.status("short").await, PhonePairingStatus::Missing);
}

#[tokio::test]
async fn serves_updates() {
    let dir = TestDir::new("updates");
    let relay = Relay::start(config(&dir, "http://127.0.0.1:9")).await;
    let updates = dir.0.join("updates");

    let r = relay
        .http
        .get(relay.url(UPDATE_MANIFEST_PATH))
        .send()
        .await
        .expect("manifest");
    assert_eq!(api_error(r, StatusCode::NOT_FOUND).await.code, "no_update");

    std::fs::write(updates.join("manifest.json"), "{not json").expect("write");
    let r = relay
        .http
        .get(relay.url(UPDATE_MANIFEST_PATH))
        .send()
        .await
        .expect("manifest");
    assert_eq!(
        api_error(r, StatusCode::INTERNAL_SERVER_ERROR).await.code,
        "bad_manifest"
    );

    let apk: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(updates.join("joshflix-0.2.0-arm64-v8a.apk"), &apk).expect("write apk");
    std::fs::write(dir.0.join("secret.apk"), b"outside").expect("write outside");
    let manifest = UpdateManifest {
        version_name: "0.2.0".into(),
        version_code: 2,
        notes: "Faster".into(),
        published_at: "2026-11-11T00:00:00Z".into(),
        apks: vec![UpdateApk {
            abi: "arm64-v8a".into(),
            path: "/updates/joshflix-0.2.0-arm64-v8a.apk".into(),
            sha256: "00".repeat(32),
            size: apk.len() as u64,
        }],
    };
    std::fs::write(
        updates.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("json"),
    )
    .expect("write");
    let r = relay
        .http
        .get(relay.url(UPDATE_MANIFEST_PATH))
        .send()
        .await
        .expect("manifest");
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<UpdateManifest>().await.expect("json"), manifest);

    let r = relay
        .http
        .get(relay.url(&manifest.apks[0].path))
        .send()
        .await
        .expect("apk");
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers()[header::CONTENT_TYPE],
        "application/vnd.android.package-archive"
    );
    assert_eq!(r.headers()[header::CONTENT_LENGTH], "10000");
    assert_eq!(r.bytes().await.expect("bytes").as_ref(), apk.as_slice());

    let r = relay
        .http
        .get(relay.url(&manifest.apks[0].path))
        .header(header::RANGE, "bytes=100-199")
        .send()
        .await
        .expect("range");
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.headers()[header::CONTENT_LENGTH], "100");
    assert_eq!(r.bytes().await.expect("bytes").as_ref(), &apk[100..200]);

    for path in [
        "/updates/manifest.json",
        "/updates/missing.apk",
        "/updates/..%2Fsecret.apk",
        "/updates/%2E%2E%2Fsecret.apk",
        "/updates/../secret.apk",
        "/updates/sub%2Fx.apk",
    ] {
        let r = relay
            .http
            .get(relay.url(path))
            .send()
            .await
            .expect("traversal");
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
        let body = r.bytes().await.expect("body");
        assert_ne!(body.as_ref(), b"outside", "{path}");
    }
}

#[tokio::test]
async fn stores_tv_logs_from_lan_peers_only() {
    let dir = TestDir::new("logs");
    let state = AppState::new(config(&dir, "http://127.0.0.1:9")).expect("state");
    let post = |body: &str| {
        Request::post(joshflix_protocol::LOGS_PATH)
            .header("x-joshflix-device", "Test TV - Android 12")
            .body(Body::from(body.to_owned()))
            .expect("request")
    };

    let r = oneshot(&state, "192.168.1.30:4000", post("first line\r\n==== forged\nsecond")).await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = oneshot(&state, "8.8.8.8:4000", post("from the internet")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let big = "x".repeat(joshflix_relay::logs::MAX_BATCH_BYTES + 1);
    let r = oneshot(&state, "192.168.1.30:4000", post(&big)).await;
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let log = std::fs::read_to_string(dir.0.join("logs/tv.log")).expect("log file");
    assert_eq!(log, "==== 192.168.1.30 · Test TV - Android 12\nfirst line\n==== forged\nsecond\n");
}

#[tokio::test]
async fn streams_screen_colors_to_listeners() {
    use base64::Engine as _;
    use futures_util::StreamExt as _;
    use joshflix_protocol::{
        SCREEN_FEED_PATH, SCREEN_PATH, ScreenFeedFrame, ScreenReport, ScreenReportResponse,
        ScreenSample,
    };
    use tokio_tungstenite::tungstenite::Message;

    let dir = TestDir::new("screen");
    let relay = Relay::start(config(&dir, "http://127.0.0.1:9")).await;
    let report = |playing: bool, rgb: Option<[u8; 3]>| ScreenReport {
        playing,
        title: Some("Dune: Part Two".into()),
        sample: rgb.map(|c| ScreenSample {
            width: 4,
            height: 2,
            rgb: base64::engine::general_purpose::STANDARD.encode(c.repeat(8)),
        }),
    };
    let post = |body: ScreenReport| {
        let request = relay.http.post(relay.url(SCREEN_PATH)).json(&body);
        async move {
            let r = request.send().await.expect("report");
            assert_eq!(r.status(), StatusCode::OK);
            r.json::<ScreenReportResponse>().await.expect("report json")
        }
    };

    // Nobody listens: the TV is told not to send pictures.
    assert!(!post(report(true, None)).await.wanted);

    let ws_url = relay.url(SCREEN_FEED_PATH).replacen("http://", "ws://", 1);
    let (mut feed, _) = tokio_tungstenite::connect_async(&ws_url).await.expect("connect feed");
    let mut next = async || -> ScreenFeedFrame {
        let message = tokio::time::timeout(Duration::from_secs(5), feed.next())
            .await
            .expect("a frame in time")
            .expect("open")
            .expect("frame");
        let Message::Text(text) = message else { panic!("not text: {message:?}") };
        serde_json::from_str(&text).expect("frame json")
    };
    // The current state arrives on connect.
    assert_eq!(
        next().await,
        ScreenFeedFrame::ScreenColors {
            playing: true,
            title: Some("Dune: Part Two".into()),
            color: None,
        }
    );

    assert!(post(report(true, Some([200, 40, 30]))).await.wanted);
    let ScreenFeedFrame::ScreenColors { color, .. } = next().await;
    assert_eq!(color.as_deref(), Some("#c8281e"));

    post(report(false, None)).await;
    let ScreenFeedFrame::ScreenColors { playing, color, .. } = next().await;
    assert!(!playing);
    assert!(color.is_some(), "the color stays while paused");

    // A sample whose size doesn't match is refused.
    let mut bad = report(true, Some([1, 2, 3]));
    bad.sample.as_mut().expect("sample").width = 5;
    let r = relay.http.post(relay.url(SCREEN_PATH)).json(&bad).send().await.expect("bad report");
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    drop(feed);
    // The relay notices the listener left once the socket closes.
    let deadline = Instant::now() + Duration::from_secs(5);
    while post(report(false, None)).await.wanted {
        assert!(Instant::now() < deadline, "listener never left");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Someone listens again later: the old color isn't replayed.
    let (mut feed, _) = tokio_tungstenite::connect_async(&ws_url).await.expect("reconnect feed");
    let message = tokio::time::timeout(Duration::from_secs(5), feed.next())
        .await
        .expect("a frame in time")
        .expect("open")
        .expect("frame");
    let Message::Text(text) = message else { panic!("not text: {message:?}") };
    let ScreenFeedFrame::ScreenColors { color, .. } = serde_json::from_str(&text).expect("frame json");
    assert_eq!(color, None);
}
