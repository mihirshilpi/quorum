//! The Graph [`Client`]: URL resolution, auth, retries, throttling and logging.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use url::Url;
use uuid::Uuid;

use crate::clock::{Clock, DynClock, SystemClock};
use crate::error::{Error, ErrorKind, Result};
use crate::http::{DynTransport, HttpRequest, HttpResponse, Transport};
use crate::json;
use crate::request::{Body, Method, Request, ResourceFamily, Target};
use crate::reqwest_transport::{DEFAULT_MAX_RESPONSE_BYTES, ReqwestConfig, ReqwestTransport};
use crate::throttle::{Jitter, Limiter, RateLimit, RetryPolicy, parse_retry_after};
use crate::token::{DynTokenSource, TokenSource};

/// Graph v1.0, the default base URL.
pub const DEFAULT_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
/// Graph beta; only used when [`ClientBuilder::beta`] is set (beta features are flag-gated).
pub const BETA_BASE_URL: &str = "https://graph.microsoft.com/beta";
/// Default cap on pages followed by [`Client::pages`] and friends.
pub const MAX_PAGES: usize = 1000;
/// Default concurrent requests per bucket key (Exchange allows 4 per mailbox).
pub const DEFAULT_BUCKET_CONCURRENCY: usize = 4;
/// Default upload-session hosts (domain suffixes) that chunk uploads may go to.
pub const DEFAULT_UPLOAD_HOST_SUFFIXES: &[&str] =
    &["sharepoint.com", "sharepoint.us", "sharepoint.de", "sharepoint.cn", "1drv.com", "onedrive.com"];

/// Headers the client owns; callers may not set them.
const RESERVED_HEADERS: &[&str] =
    &["authorization", "host", "content-length", "client-request-id", "transfer-encoding", "cookie", "connection"];

/// A cheap-to-clone Microsoft Graph client (M-SERVICES-CLONE).
///
/// Endpoint-agnostic: it sends [`Request`] values and returns JSON or raw responses. Every
/// request gets a fresh `client-request-id`; Graph's `request-id` is logged (never URLs,
/// bodies or tokens).
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<Inner>,
}

pub(crate) struct Inner {
    pub(crate) base: Url,
    pub(crate) base_path: String,
    pub(crate) transport: Box<dyn DynTransport>,
    pub(crate) tokens: Box<dyn DynTokenSource>,
    pub(crate) clock: Box<dyn DynClock>,
    pub(crate) retry: RetryPolicy,
    pub(crate) limiter: Limiter,
    pub(crate) jitter: Jitter,
    pub(crate) max_response_bytes: usize,
    pub(crate) max_json_depth: usize,
    pub(crate) max_pages: usize,
    pub(crate) upload_hosts: Vec<String>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.inner.base.as_str())
            .field("retry", &self.inner.retry)
            .field("max_pages", &self.inner.max_pages)
            .finish_non_exhaustive()
    }
}

/// Configures a [`Client`].
pub struct ClientBuilder {
    tokens: Box<dyn DynTokenSource>,
    transport: Option<Box<dyn DynTransport>>,
    clock: Option<Box<dyn DynClock>>,
    base_url: Option<String>,
    beta: bool,
    retry: RetryPolicy,
    rates: [RateLimit; 5],
    bucket_concurrency: usize,
    max_response_bytes: usize,
    max_json_depth: usize,
    max_pages: usize,
    upload_hosts: Vec<String>,
    jitter_seed: Option<u64>,
    reqwest: ReqwestConfig,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("base_url", &self.base_url)
            .field("beta", &self.beta)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// Sends through `transport` instead of the default [`ReqwestTransport`].
    #[must_use]
    pub fn transport(mut self, transport: impl Transport) -> Self {
        self.transport = Some(Box::new(transport));
        self
    }

    /// Uses `clock` for time and sleeps (tests pass a `FakeClock`).
    #[must_use]
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Some(Box::new(clock));
        self
    }

    /// Overrides the base URL (national clouds, a local mock server). It must be `https`, or
    /// `http` on a loopback host, and include the version path, e.g. `https://graph.microsoft.us/v1.0`.
    #[must_use]
    pub fn base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Uses Graph beta (`/beta`) instead of v1.0. Only for features listed as beta-gated.
    #[must_use]
    pub fn beta(mut self, beta: bool) -> Self {
        self.beta = beta;
        self
    }

    /// Sets the retry policy.
    #[must_use]
    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Sets the global rate for one resource family.
    #[must_use]
    pub fn rate(mut self, family: ResourceFamily, rate: RateLimit) -> Self {
        if let Some(slot) = self.rates.get_mut(family.index()) {
            *slot = rate;
        }
        self
    }

    /// Concurrent requests allowed per bucket key (default 4; minimum 1).
    #[must_use]
    pub fn bucket_concurrency(mut self, permits: usize) -> Self {
        self.bucket_concurrency = permits.max(1);
        self
    }

    /// Largest response body accepted (default 64 MiB).
    #[must_use]
    pub fn max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    /// Deepest JSON nesting accepted (default 64).
    #[must_use]
    pub fn max_json_depth(mut self, depth: usize) -> Self {
        self.max_json_depth = depth.max(1);
        self
    }

    /// Default page cap for paging and delta helpers (default [`MAX_PAGES`]).
    #[must_use]
    pub fn max_pages(mut self, pages: usize) -> Self {
        self.max_pages = pages.max(1);
        self
    }

    /// Domain suffixes upload-session URLs may point at (default
    /// [`DEFAULT_UPLOAD_HOST_SUFFIXES`]); the Graph host itself is always allowed.
    #[must_use]
    pub fn upload_host_suffixes<I, S>(mut self, suffixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.upload_hosts = suffixes.into_iter().map(|s| s.into().to_ascii_lowercase()).collect();
        self
    }

    /// Seeds the backoff jitter (deterministic tests).
    #[must_use]
    pub fn jitter_seed(mut self, seed: u64) -> Self {
        self.jitter_seed = Some(seed);
        self
    }

    /// Timeouts for the default reqwest transport (ignored with a custom transport).
    #[must_use]
    pub fn timeouts(mut self, connect: Duration, total: Duration) -> Self {
        self.reqwest.connect_timeout = connect;
        self.reqwest.timeout = total;
        self
    }

    /// Builds the client.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for an invalid base URL, both `base_url` and `beta`, or a TLS
    /// backend that cannot start.
    pub fn build(self) -> Result<Client> {
        let base_str = match (self.base_url, self.beta) {
            (Some(_), true) => {
                return Err(Error::new(ErrorKind::Config, "set either base_url or beta, not both"));
            }
            (Some(url), false) => url,
            (None, true) => BETA_BASE_URL.to_owned(),
            (None, false) => DEFAULT_BASE_URL.to_owned(),
        };
        let base = validate_base(&base_str)?;
        let base_path = base.path().trim_end_matches('/').to_owned();
        let transport: Box<dyn DynTransport> = match self.transport {
            Some(t) => t,
            None => {
                let mut cfg = self.reqwest;
                cfg.max_response_bytes = self.max_response_bytes;
                Box::new(ReqwestTransport::new(&cfg)?)
            }
        };
        let clock: Box<dyn DynClock> = self.clock.unwrap_or_else(|| Box::new(SystemClock));
        let seed = self.jitter_seed.unwrap_or_else(|| {
            let (hi, lo) = Uuid::new_v4().as_u64_pair();
            hi ^ lo
        });
        let limiter = Limiter::new(self.rates, self.bucket_concurrency, clock.now());
        let mut retry = self.retry;
        retry.max_attempts = retry.max_attempts.max(1);
        Ok(Client {
            inner: Arc::new(Inner {
                base,
                base_path,
                transport,
                tokens: self.tokens,
                clock,
                retry,
                limiter,
                jitter: Jitter::new(seed),
                max_response_bytes: self.max_response_bytes,
                max_json_depth: self.max_json_depth,
                max_pages: self.max_pages,
                upload_hosts: self.upload_hosts,
            }),
        })
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn validate_base(s: &str) -> Result<Url> {
    let mut url = Url::parse(s)?;
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && is_loopback(&url));
    if !scheme_ok || url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(Error::new(ErrorKind::Config, "base URL must be https (or http on loopback) without credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::new(ErrorKind::Config, "base URL must not have a query or fragment"));
    }
    let trimmed = url.path().trim_end_matches('/').to_owned();
    url.set_path(&trimmed);
    Ok(url)
}

impl Client {
    /// Starts configuring a client that authenticates with `tokens`.
    #[must_use]
    pub fn builder(tokens: impl TokenSource) -> ClientBuilder {
        ClientBuilder {
            tokens: Box::new(tokens),
            transport: None,
            clock: None,
            base_url: None,
            beta: false,
            retry: RetryPolicy::default(),
            rates: ResourceFamily::ALL.map(RateLimit::default_for),
            bucket_concurrency: DEFAULT_BUCKET_CONCURRENCY,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            max_json_depth: json::DEFAULT_MAX_DEPTH,
            max_pages: MAX_PAGES,
            upload_hosts: DEFAULT_UPLOAD_HOST_SUFFIXES.iter().map(|s| (*s).to_owned()).collect(),
            jitter_seed: None,
            reqwest: ReqwestConfig::default(),
        }
    }

    /// The base URL, e.g. `https://graph.microsoft.com/v1.0`.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.inner.base
    }

    /// The default page cap.
    #[must_use]
    pub fn max_pages(&self) -> usize {
        self.inner.max_pages
    }

    /// Whether `url` is on this client's Graph origin and version path.
    pub(crate) fn is_graph_url(&self, url: &Url) -> bool {
        let base = &self.inner.base;
        let prefix_ok = {
            let p = url.path();
            let bp = self.inner.base_path.as_str();
            p == bp || p.strip_prefix(bp).is_some_and(|rest| rest.starts_with('/'))
        };
        url.scheme() == base.scheme()
            && url.host() == base.host()
            && url.port_or_known_default() == base.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && prefix_ok
    }

    /// Resolves a request target to an absolute URL on the Graph host (SSRF guard).
    pub(crate) fn resolve(&self, req: &Request) -> Result<Url> {
        let mut url = match &req.target {
            Target::Path(path) => {
                if !path.starts_with('/') || path.starts_with("//") || path.contains('#') {
                    return Err(Error::new(ErrorKind::Config, "request path must start with a single '/'"));
                }
                let joined = format!("{}{}", self.inner.base.as_str().trim_end_matches('/'), path);
                let url = Url::parse(&joined)?;
                if !self.is_graph_url(&url) {
                    return Err(Error::new(ErrorKind::Config, "request path escapes the Graph base URL"));
                }
                url
            }
            Target::Link(link) => {
                let url = Url::parse(link)
                    .map_err(|e| Error::new(ErrorKind::Malformed, "link is not a URL").with_source(e))?;
                if !self.is_graph_url(&url) {
                    tracing::warn!(name: "graph.link_rejected", host = url.host_str().unwrap_or(""), "link not on Graph host");
                    return Err(Error::new(ErrorKind::Malformed, "link is not on the configured Graph host"));
                }
                url
            }
        };
        if !req.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in &req.query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// Validated caller headers plus body bytes and content type.
    fn prepare(&self, req: &Request) -> Result<(Vec<(String, String)>, Vec<u8>)> {
        let mut headers = Vec::with_capacity(req.headers.len().saturating_add(4));
        for (name, value) in &req.headers {
            validate_header(name, value)?;
            headers.push((name.clone(), value.clone()));
        }
        let has = |h: &[(String, String)], n: &str| h.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
        if !has(&headers, "accept") {
            headers.push(("Accept".to_owned(), "application/json".to_owned()));
        }
        let body = match &req.body {
            Body::Empty => Vec::new(),
            Body::Json(v) => {
                if !has(&headers, "content-type") {
                    headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
                }
                serde_json::to_vec(v)
                    .map_err(|e| Error::new(ErrorKind::Config, "request body is not serialisable").with_source(e))?
            }
            Body::Bytes { content_type, data } => {
                validate_header("Content-Type", content_type)?;
                if !has(&headers, "content-type") {
                    headers.push(("Content-Type".to_owned(), content_type.clone()));
                }
                data.clone()
            }
        };
        Ok((headers, body))
    }

    pub(crate) fn backoff(&self, retry: u32) -> Duration {
        self.inner.retry.backoff(retry, self.inner.jitter.next_unit())
    }

    pub(crate) fn retry_after_of(&self, resp: &HttpResponse) -> Option<Duration> {
        resp.header("retry-after").and_then(|v| parse_retry_after(v, self.inner.clock.now_utc()))
    }

    pub(crate) async fn sleep(&self, d: Duration) {
        self.inner.clock.sleep(d).await;
    }

    /// Sends with auth, throttling and retries; returns the final response whatever its status.
    pub(crate) async fn execute(&self, req: &Request) -> Result<HttpResponse> {
        let url = self.resolve(req)?;
        let (headers, body) = self.prepare(req)?;
        let family = req.resource_family();
        let client_request_id = Uuid::new_v4().to_string();
        self.execute_url(
            req.method,
            url,
            headers,
            body,
            family,
            req.bucket_key(),
            req.is_idempotent(),
            true,
            &client_request_id,
        )
        .await
    }

    /// The retry loop shared by Graph requests and (unauthenticated) upload chunks.
    pub(crate) async fn execute_url(
        &self,
        method: Method,
        url: Url,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        family: ResourceFamily,
        bucket: &str,
        idempotent: bool,
        authenticate: bool,
        client_request_id: &str,
    ) -> Result<HttpResponse> {
        let inner = &self.inner;
        let max_attempts = inner.retry.max_attempts;
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            let can_retry = idempotent && attempt < max_attempts;
            let outcome = {
                let _permit = inner.limiter.acquire_slot(bucket).await?;
                inner.limiter.acquire_rate(family, inner.clock.as_ref()).await?;
                let mut wire_headers = headers.clone();
                if authenticate {
                    let token = inner.tokens.token_dyn().await?;
                    wire_headers.push(("Authorization".to_owned(), format!("Bearer {token}")));
                    wire_headers.push(("client-request-id".to_owned(), client_request_id.to_owned()));
                }
                let http = HttpRequest { method, url: url.clone(), headers: wire_headers, body: body.clone() };
                inner.transport.send_dyn(http).await
            };
            match outcome {
                Ok(resp) => {
                    tracing::debug!(
                        name: "graph.response",
                        method = method.as_str(),
                        family = family.as_str(),
                        status = resp.status,
                        attempt,
                        request_id = resp.request_id().unwrap_or(""),
                        client_request_id,
                        "graph response"
                    );
                    if resp.body.len() > inner.max_response_bytes {
                        return Err(Error::new(ErrorKind::TooLarge, "response body exceeds cap")
                            .with_status(resp.status)
                            .with_request_id(resp.request_id().map(str::to_owned)));
                    }
                    let status = resp.status;
                    let throttled = matches!(status, 429 | 503 | 504);
                    let retry_after = if throttled { self.retry_after_of(&resp) } else { None };
                    if status == 429 || (throttled && retry_after.is_some()) {
                        let pause = retry_after.unwrap_or(inner.retry.base_delay);
                        inner.limiter.pause(family, pause.min(inner.retry.max_retry_after), inner.clock.now());
                    }
                    let server_error = (500..600).contains(&status) && status != 501 && status != 505;
                    if !(throttled || server_error) || !can_retry {
                        return Ok(resp);
                    }
                    let delay = match retry_after {
                        Some(d) if d > inner.retry.max_retry_after => return Ok(resp),
                        Some(d) => d,
                        None => self.backoff(attempt),
                    };
                    tracing::info!(
                        name: "graph.retry",
                        family = family.as_str(),
                        status,
                        attempt,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        request_id = resp.request_id().unwrap_or(""),
                        "retrying graph request"
                    );
                    inner.clock.sleep(delay).await;
                }
                Err(e) if e.kind() == ErrorKind::Network && can_retry => {
                    let delay = self.backoff(attempt);
                    tracing::info!(
                        name: "graph.retry",
                        family = family.as_str(),
                        attempt,
                        delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                        "retrying after network error"
                    );
                    inner.clock.sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Converts a final response into `Ok` (2xx/3xx) or the matching [`Error`].
    pub(crate) fn check(&self, resp: HttpResponse) -> Result<HttpResponse> {
        if (200..400).contains(&resp.status) {
            Ok(resp)
        } else {
            let retry_after = self.retry_after_of(&resp);
            Err(json::error_from_response(&resp, retry_after))
        }
    }

    /// Sends `req` and returns the raw response (2xx or 3xx).
    ///
    /// # Errors
    /// A 4xx/5xx after retries maps to the matching [`ErrorKind`]; also `Config` for bad
    /// paths/headers, `Malformed` for foreign links, `Network`, `TooLarge`.
    pub async fn send_raw(&self, req: Request) -> Result<HttpResponse> {
        let resp = self.execute(&req).await?;
        self.check(resp)
    }

    /// Sends `req` and parses the JSON response as `T` (unknown fields ignored; an empty 204
    /// body parses as `null`, so use `()` or `Option<_>`).
    ///
    /// # Errors
    /// As [`Client::send_raw`], plus [`ErrorKind::Malformed`] for invalid or too-deep JSON.
    pub async fn send_json<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        let resp = self.send_raw(req).await?;
        self.parse_json(&resp.body)
    }

    pub(crate) fn parse_json<T: DeserializeOwned>(&self, body: &[u8]) -> Result<T> {
        json::parse(body, self.inner.max_json_depth)
    }
}

fn validate_header(name: &str, value: &str) -> Result<()> {
    let name_ok = !name.is_empty()
        && name.len() <= 256
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !name_ok {
        return Err(Error::new(ErrorKind::Config, "invalid header name"));
    }
    if RESERVED_HEADERS.iter().any(|r| name.eq_ignore_ascii_case(r)) {
        return Err(Error::new(ErrorKind::Config, "header is set by the client"));
    }
    if value.len() > 8192 || value.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
        return Err(Error::new(ErrorKind::Config, "invalid header value"));
    }
    Ok(())
}
