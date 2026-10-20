//! End-to-end tests of the public API with fake browser, transport, store and clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use quorum_auth::{
    AuthConfig, Authenticator, BrowserOpener, ClaimsChallenge, Clock, ErrorKind, FakeClock, FakeTransport,
    FormResponse, MemoryStore, SignInOptions,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;

const OID: &str = "11111111-1111-1111-1111-111111111111";
const TID: &str = "22222222-2222-2222-2222-222222222222";
const ACCOUNT_ID: &str = "11111111-1111-1111-1111-111111111111.22222222-2222-2222-2222-222222222222";

fn config() -> AuthConfig {
    AuthConfig::builder("00000000-0000-0000-0000-0000000000aa").tenant(TID).build().unwrap()
}

fn id_token(nonce: Option<&str>) -> String {
    let mut claims = serde_json::json!({
        "oid": OID, "tid": TID, "preferred_username": "test.user@example.test", "name": "Test User"
    });
    if let Some(n) = nonce {
        claims["nonce"] = n.into();
    }
    format!("eyJhbGciOiJub25lIn0.{}.c2ln", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn token_json(at: &str, rt: &str, expires_in: u64, nonce: Option<&str>) -> String {
    serde_json::json!({
        "token_type": "Bearer", "access_token": at, "refresh_token": rt, "expires_in": expires_in,
        "scope": "Mail.ReadWrite Chat.ReadWrite openid profile", "id_token": id_token(nonce)
    })
    .to_string()
}

fn ok(body: String) -> quorum_auth::Result<FormResponse> {
    Ok(FormResponse { status: 200, body: body.into_bytes() })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Good,
    WrongState,
    Denied,
    Oversized,
}

/// A browser that performs the redirect itself, like a user signing in.
#[derive(Debug, Clone)]
struct FakeBrowser {
    mode: Mode,
    opened: Arc<Mutex<Vec<Url>>>,
    nonce: Arc<Mutex<Option<String>>>,
    page: Arc<Mutex<Option<String>>>,
}

impl FakeBrowser {
    fn new(mode: Mode) -> Self {
        Self { mode, opened: Arc::default(), nonce: Arc::default(), page: Arc::default() }
    }
}

impl FakeBrowser {
    /// The page the browser received, waiting briefly for its task to finish reading it.
    async fn page(&self) -> Option<String> {
        for _ in 0..200 {
            if let Some(p) = self.page.lock().unwrap().clone() {
                return Some(p);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        None
    }
}

impl BrowserOpener for FakeBrowser {
    fn open(&self, url: &Url) -> quorum_auth::Result<()> {
        self.opened.lock().unwrap().push(url.clone());
        let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
        *self.nonce.lock().unwrap() = q.get("nonce").cloned();
        let redirect = Url::parse(&q["redirect_uri"]).unwrap();
        let port = redirect.port().unwrap();
        let state = q["state"].clone();
        let mode = self.mode;
        let page = self.page.clone();
        tokio::spawn(async move {
            // A speculative connection that never sends anything must not block the real one.
            let _idle = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            // A favicon request is answered and ignored.
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf).await;
            assert!(buf.starts_with("HTTP/1.1 404"));

            if mode == Mode::Oversized {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                let huge = format!("GET /?code=x&state={state}&pad={} HTTP/1.1\r\n\r\n", "A".repeat(64 * 1024));
                let _ = s.write_all(huge.as_bytes()).await;
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf).await;
                *page.lock().unwrap() = Some(buf);
                return;
            }
            let target = match mode {
                Mode::Good => format!("/?code=auth-code-1&state={state}&session_state=abc"),
                Mode::WrongState => "/?code=auth-code-1&state=forged".to_owned(),
                Mode::Denied => format!("/?error=access_denied&error_description=nope&state={state}"),
                Mode::Oversized => unreachable!(),
            };
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n").as_bytes()).await.unwrap();
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf).await;
            *page.lock().unwrap() = Some(buf);
        });
        Ok(())
    }
}

fn authenticator(transport: FakeTransport, store: MemoryStore, clock: FakeClock) -> Authenticator {
    Authenticator::builder(config()).transport(transport).store(store).clock(clock).build().unwrap()
}

/// Token endpoint fake that answers the code exchange with the nonce the browser saw.
fn code_exchange_transport(browser: &FakeBrowser) -> FakeTransport {
    let nonce = browser.nonce.clone();
    FakeTransport::new().with_handler(move |req| {
        assert_eq!(req.field("grant_type"), Some("authorization_code"));
        assert_eq!(req.field("code"), Some("auth-code-1"));
        assert!(req.field("code_verifier").is_some_and(|v| v.len() >= 43));
        assert!(req.field("redirect_uri").is_some_and(|r| r.starts_with("http://localhost:")));
        let n = nonce.lock().unwrap().clone();
        ok(token_json("access-1", "refresh-1", 3600, n.as_deref()))
    })
}

#[tokio::test]
async fn loopback_sign_in_end_to_end() {
    let browser = FakeBrowser::new(Mode::Good);
    let store = MemoryStore::new();
    let transport = code_exchange_transport(&browser);
    let auth = authenticator(transport.clone(), store.clone(), FakeClock::default());

    let claims = ClaimsChallenge::from_www_authenticate(&format!(
        "Bearer error=\"insufficient_claims\", claims=\"{}\"",
        STANDARD.encode(r#"{"access_token":{"nbf":{"essential":true,"value":"1"}}}"#)
    ));
    let provider =
        auth.sign_in(&browser, SignInOptions::default().claims(claims).timeout(Duration::from_secs(20))).await.unwrap();

    // Authorize URL carries PKCE S256, state, nonce and the claims challenge.
    let url = browser.opened.lock().unwrap()[0].clone();
    assert!(url.as_str().starts_with(&format!("https://login.microsoftonline.com/{TID}/oauth2/v2.0/authorize?")));
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["scope"], "https://graph.microsoft.com/.default offline_access openid profile");
    assert!(q["claims"].contains("\"nbf\""));
    assert!(q["state"].len() >= 32 && q["nonce"].len() >= 32);

    // The browser got the friendly page.
    let page = browser.page().await.unwrap();
    assert!(page.starts_with("HTTP/1.1 200") && page.contains("You can close this window"));

    assert_eq!(provider.account().id(), ACCOUNT_ID);
    assert_eq!(provider.account().username(), "test.user@example.test");
    assert_eq!(provider.access_token().await.unwrap().secret(), "access-1");
    assert_eq!(transport.requests().len(), 1, "fresh token served from memory");
    assert!(provider.missing_scopes(&["Mail.ReadWrite"]).is_empty());
    assert_eq!(provider.missing_scopes(&["Mail.Send"]), ["Mail.Send"]);

    // Refresh token is in the store; the access token is not.
    let raw = store.raw(&format!("account:{ACCOUNT_ID}")).unwrap();
    assert!(raw.contains("refresh-1") && !raw.contains("access-1"));
    assert_eq!(auth.accounts().unwrap().len(), 1);

    // Sign out removes it, and restore then finds nothing.
    auth.sign_out(ACCOUNT_ID).unwrap();
    assert!(auth.restore(ACCOUNT_ID).unwrap().is_none());
    assert!(auth.accounts().unwrap().is_empty());
}

#[tokio::test]
async fn state_mismatch_is_rejected() {
    let browser = FakeBrowser::new(Mode::WrongState);
    let transport = code_exchange_transport(&browser);
    let auth = authenticator(transport.clone(), MemoryStore::new(), FakeClock::default());
    let err = auth.sign_in(&browser, SignInOptions::default().timeout(Duration::from_secs(20))).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::StateMismatch);
    assert!(transport.requests().is_empty(), "no code exchange for a forged redirect");
    assert!(browser.page().await.unwrap().starts_with("HTTP/1.1 400"));
}

#[tokio::test]
async fn user_declining_is_cancelled() {
    let browser = FakeBrowser::new(Mode::Denied);
    let auth = authenticator(FakeTransport::new(), MemoryStore::new(), FakeClock::default());
    let err = auth.sign_in(&browser, SignInOptions::default().timeout(Duration::from_secs(20))).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

#[tokio::test]
async fn oversized_request_is_refused_and_times_out() {
    let browser = FakeBrowser::new(Mode::Oversized);
    let auth = authenticator(FakeTransport::new(), MemoryStore::new(), FakeClock::default());
    let err = auth.sign_in(&browser, SignInOptions::default().timeout(Duration::from_millis(1500))).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Timeout);
    let page = browser.page().await.unwrap_or_default();
    assert!(page.is_empty() || page.starts_with("HTTP/1.1 431"), "{page}");
}

#[tokio::test]
async fn nonce_mismatch_is_rejected() {
    let browser = FakeBrowser::new(Mode::Good);
    let transport = FakeTransport::new().with_handler(|_| ok(token_json("a", "r", 3600, Some("other-nonce"))));
    let store = MemoryStore::new();
    let auth = authenticator(transport, store.clone(), FakeClock::default());
    let err = auth.sign_in(&browser, SignInOptions::default().timeout(Duration::from_secs(20))).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::StateMismatch);
    assert!(store.keys().is_empty());
}

/// Seeds a stored account by running a device-code sign-in against the fakes.
async fn seeded(transport: FakeTransport, clock: FakeClock) -> (Authenticator, MemoryStore) {
    let store = MemoryStore::new();
    let auth = authenticator(transport, store.clone(), clock);
    let seed = MemoryStore::new();
    let seed_tr = FakeTransport::new();
    seed_tr.push_json(200, r#"{"device_code":"dc","user_code":"ABCD","verification_uri":"https://login.example.test/device","expires_in":900,"interval":5}"#);
    seed_tr.push_json(200, &token_json("seed-at", "refresh-0", 3600, None));
    let seeder = Authenticator::builder(config())
        .transport(seed_tr)
        .store(seed.clone())
        .clock(FakeClock::default())
        .build()
        .unwrap();
    let code = seeder.start_device_code().await.unwrap();
    seeder.finish_device_code(&code).await.unwrap();
    for k in seed.keys() {
        store_copy(&seed, &store, &k);
    }
    (auth, store)
}

fn store_copy(from: &MemoryStore, to: &MemoryStore, key: &str) {
    use quorum_auth::SecretStore;
    to.set(key, &from.raw(key).unwrap()).unwrap();
}

#[tokio::test]
async fn restore_then_refresh_is_single_flight() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let transport = FakeTransport::new().with_delay(Duration::from_millis(50)).with_handler(move |req| {
        assert_eq!(req.field("grant_type"), Some("refresh_token"));
        assert_eq!(req.field("refresh_token"), Some("refresh-0"));
        let n = c.fetch_add(1, Ordering::SeqCst);
        ok(token_json(&format!("access-{n}"), "refresh-rotated", 3600, None))
    });
    let (auth, store) = seeded(transport, FakeClock::default()).await;
    let provider = auth.restore(ACCOUNT_ID).unwrap().unwrap();
    assert_eq!(provider.granted_scopes().as_slice().len(), 4, "scopes restored from the store");

    let handles: Vec<_> = (0..16)
        .map(|_| {
            let p = provider.clone();
            tokio::spawn(async move { p.access_token().await.unwrap() })
        })
        .collect();
    for h in handles {
        assert_eq!(h.await.unwrap().secret(), "access-0");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "one refresh for 16 concurrent callers");
    assert!(store.raw(&format!("account:{ACCOUNT_ID}")).unwrap().contains("refresh-rotated"));
}

#[tokio::test]
async fn refreshes_five_minutes_before_expiry() {
    let clock = FakeClock::default();
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let transport = FakeTransport::new().with_handler(move |_| {
        let n = c.fetch_add(1, Ordering::SeqCst);
        ok(token_json(&format!("access-{n}"), "refresh-0", 3600, None))
    });
    let (auth, _store) = seeded(transport, clock.clone()).await;
    let p = auth.restore(ACCOUNT_ID).unwrap().unwrap();

    let t0 = p.access_token().await.unwrap();
    assert_eq!(t0.secret(), "access-0");
    assert_eq!(t0.expires_at(), clock.now().checked_add(jiff::SignedDuration::from_secs(3600)).unwrap());

    clock.advance(Duration::from_secs(3600 - 5 * 60 - 1)); // 5 min + 1 s left: still cached
    assert_eq!(p.access_token().await.unwrap().secret(), "access-0");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    clock.advance(Duration::from_secs(2)); // inside the 5 min margin: refresh
    assert_eq!(p.access_token().await.unwrap().secret(), "access-1");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // invalidate() with a stale token keeps the newer one; with the current one forces refresh.
    p.invalidate(&t0).await;
    assert_eq!(p.access_token().await.unwrap().secret(), "access-1");
    let t1 = p.access_token().await.unwrap();
    p.invalidate(&t1).await;
    assert_eq!(p.access_token().await.unwrap().secret(), "access-2");
}

#[tokio::test]
async fn transient_failure_serves_unexpired_token_then_errors() {
    let clock = FakeClock::default();
    let transport = FakeTransport::new();
    transport.push(ok(token_json("access-0", "refresh-0", 3600, None)));
    let (auth, _) = seeded(transport.clone(), clock.clone()).await;
    let p = auth.restore(ACCOUNT_ID).unwrap().unwrap();
    p.access_token().await.unwrap();

    clock.advance(Duration::from_secs(3600 - 60));
    transport.push_json(503, "<html>busy</html>");
    assert_eq!(p.access_token().await.unwrap().secret(), "access-0", "still valid for 60 s");

    clock.advance(Duration::from_secs(120));
    transport.push_json(503, "<html>busy</html>");
    assert_eq!(p.access_token().await.unwrap_err().kind(), ErrorKind::Network);
}

#[tokio::test]
async fn invalid_grant_requires_interaction_and_fails_fast() {
    let transport = FakeTransport::new();
    transport.push_json(
        400,
        r#"{"error":"invalid_grant","error_description":"AADSTS700082: expired","error_codes":[700082]}"#,
    );
    let (auth, _) = seeded(transport.clone(), FakeClock::default()).await;
    let p = auth.restore(ACCOUNT_ID).unwrap().unwrap();
    let e = p.access_token().await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::InteractionRequired);
    assert_eq!(e.oauth_error(), Some("invalid_grant"));
    // No second network call.
    assert_eq!(p.access_token().await.unwrap_err().kind(), ErrorKind::InteractionRequired);
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn missing_refresh_token_requires_interaction() {
    let (auth, store) = seeded(FakeTransport::new(), FakeClock::default()).await;
    let p = auth.restore(ACCOUNT_ID).unwrap().unwrap();
    auth.sign_out(ACCOUNT_ID).unwrap();
    assert!(store.raw(&format!("account:{ACCOUNT_ID}")).is_none());
    assert_eq!(p.access_token().await.unwrap_err().kind(), ErrorKind::InteractionRequired);
}

#[tokio::test]
async fn store_failure_is_a_store_error() {
    let (auth, store) = seeded(FakeTransport::new(), FakeClock::default()).await;
    store.set_failing(true);
    assert_eq!(auth.restore(ACCOUNT_ID).unwrap_err().kind(), ErrorKind::Store);
    store.set_failing(false);
    // A corrupt item is a Store error, not a panic.
    {
        use quorum_auth::SecretStore;
        store.set(&format!("account:{ACCOUNT_ID}"), "{garbage").unwrap();
    }
    assert_eq!(auth.restore(ACCOUNT_ID).unwrap_err().kind(), ErrorKind::Store);
    assert!(auth.accounts().unwrap().is_empty());
}

#[tokio::test]
async fn device_code_polls_with_interval_and_slow_down() {
    let clock = FakeClock::default();
    let transport = FakeTransport::new();
    transport.push_json(
        200,
        r#"{"device_code":"dc-secret","user_code":"ABCD-EFGH","verification_uri":"https://login.example.test/device","expires_in":900,"interval":5,"message":"To sign in, enter ABCD-EFGH"}"#,
    );
    transport.push_json(400, r#"{"error":"authorization_pending"}"#);
    transport.push_json(400, r#"{"error":"slow_down"}"#);
    transport.push_json(400, r#"{"error":"authorization_pending"}"#);
    transport.push_json(200, &token_json("dev-at", "dev-rt", 3600, None));
    let store = MemoryStore::new();
    let auth = authenticator(transport.clone(), store.clone(), clock.clone());

    let code = auth.start_device_code().await.unwrap();
    assert_eq!(code.user_code(), "ABCD-EFGH");
    assert_eq!(code.message(), "To sign in, enter ABCD-EFGH");
    assert!(!format!("{code:?}").contains("dc-secret"));
    let p = auth.finish_device_code(&code).await.unwrap();
    assert_eq!(p.access_token().await.unwrap().secret(), "dev-at");
    assert_eq!(p.account().id(), ACCOUNT_ID);

    let secs: Vec<u64> = clock.sleeps().iter().map(Duration::as_secs).collect();
    assert_eq!(secs, [5, 5, 10, 10], "interval honoured, slow_down adds 5 s");
    let reqs = transport.requests();
    assert!(reqs[0].url.as_str().ends_with("/oauth2/v2.0/devicecode"));
    assert_eq!(reqs[1].field("grant_type"), Some("urn:ietf:params:oauth:grant-type:device_code"));
    assert_eq!(reqs[1].field("device_code"), Some("dc-secret"));
    assert!(store.raw(&format!("account:{ACCOUNT_ID}")).unwrap().contains("dev-rt"));
}

#[tokio::test]
async fn device_code_expiry_and_decline() {
    let clock = FakeClock::default();
    let transport = FakeTransport::new()
        .with_handler(|_| Ok(FormResponse { status: 400, body: br#"{"error":"authorization_pending"}"#.to_vec() }));
    transport.push_json(200, r#"{"device_code":"d","user_code":"U","verification_uri":"https://x.example.test/","expires_in":30,"interval":5}"#);
    let auth = authenticator(transport, MemoryStore::new(), clock.clone());
    let code = auth.start_device_code().await.unwrap();
    assert_eq!(auth.finish_device_code(&code).await.unwrap_err().kind(), ErrorKind::Timeout);

    let transport = FakeTransport::new();
    transport.push_json(200, r#"{"device_code":"d","user_code":"U","verification_uri":"https://x.example.test/"}"#);
    transport.push_json(400, r#"{"error":"authorization_declined"}"#);
    let auth = authenticator(transport, MemoryStore::new(), clock);
    let code = auth.start_device_code().await.unwrap();
    assert_eq!(auth.finish_device_code(&code).await.unwrap_err().kind(), ErrorKind::Cancelled);

    // Garbage device-code responses are errors.
    for body in ["", "{}", r#"{"device_code":"d","user_code":"U","verification_uri":"javascript:alert(1)"}"#] {
        let transport = FakeTransport::new();
        transport.push_json(200, body);
        let auth = authenticator(transport, MemoryStore::new(), FakeClock::default());
        assert!(auth.start_device_code().await.is_err(), "{body}");
    }
}

#[test]
fn public_types_redact_secrets() {
    let auth = authenticator(FakeTransport::new(), MemoryStore::new(), FakeClock::default());
    let s = format!("{auth:?}");
    assert!(!s.contains("refresh"), "{s}");
}
