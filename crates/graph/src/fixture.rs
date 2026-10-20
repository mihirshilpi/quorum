//! `FixtureTransport`: replays synthetic Graph responses for tests (feature `test-util`).
//!
//! Routes match on method + path (the `/v1.0` or `/beta` prefix and the host are ignored)
//! and an optional subset of query parameters. When several routes match, the one with the most
//! query constraints wins (ties: first added). Each route holds a script of responses; every call
//! takes the next one and the last repeats. Unmatched requests get a Graph-shaped 404 with code
//! `fixtureNotFound`. Every request is recorded for assertions.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Deserialize;

use crate::error::{Error, ErrorKind, Result};
use crate::http::{HttpRequest, HttpResponse, Transport};
use crate::request::Method;

/// The repository's `fixtures/graph/` folder.
#[must_use]
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("fixtures").join("graph")
}

/// One scripted reply.
#[derive(Clone, Debug)]
pub enum FixtureResponse {
    /// An HTTP response.
    Http(HttpResponse),
    /// A transport failure ([`ErrorKind::Network`]).
    NetworkError,
}

impl FixtureResponse {
    /// A response with `status` and an empty body.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self::Http(HttpResponse::new(status))
    }

    /// A JSON response.
    #[must_use]
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::bytes(status, serde_json::to_vec(body).unwrap_or_default())
    }

    /// A response whose body is `body` verbatim (sent as JSON).
    #[must_use]
    pub fn bytes(status: u16, body: impl Into<Vec<u8>>) -> Self {
        let mut resp = HttpResponse::new(status);
        resp.headers.push(("content-type".into(), "application/json".into()));
        resp.body = body.into();
        Self::Http(resp)
    }

    /// A response whose body is the file `name` under [`fixtures_dir`].
    ///
    /// # Errors
    /// [`ErrorKind::Config`] if the file cannot be read.
    pub fn file(status: u16, name: &str) -> Result<Self> {
        let path = fixtures_dir().join(name);
        let body = std::fs::read(&path)
            .map_err(|e| Error::new(ErrorKind::Config, format!("cannot read fixture {name}")).with_source(e))?;
        Ok(Self::bytes(status, body))
    }

    /// Adds a response header.
    #[must_use]
    pub fn with_header(self, name: &str, value: &str) -> Self {
        match self {
            Self::Http(mut r) => {
                r.headers.push((name.to_owned(), value.to_owned()));
                Self::Http(r)
            }
            Self::NetworkError => Self::NetworkError,
        }
    }
}

/// A route: method, path, required query parameters and a response script.
#[derive(Clone, Debug)]
pub struct Route {
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    responses: VecDeque<FixtureResponse>,
}

impl Route {
    /// A route for `method path` (path without the version prefix, e.g. `/me/messages`).
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self { method, path: path.into(), query: Vec::new(), responses: VecDeque::new() }
    }

    /// Requires query parameter `key` to equal `value` (decoded).
    #[must_use]
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Appends a response to the script.
    #[must_use]
    pub fn respond(mut self, response: FixtureResponse) -> Self {
        self.responses.push_back(response);
        self
    }

    fn matches(&self, req: &HttpRequest) -> bool {
        if self.method != req.method || strip_version(req.url.path()) != self.path {
            return false;
        }
        let pairs: Vec<(String, String)> =
            req.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        self.query.iter().all(|q| pairs.contains(q))
    }
}

fn strip_version(path: &str) -> &str {
    for prefix in ["/v1.0", "/beta"] {
        if let Some(rest) = path.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return if rest.is_empty() { "/" } else { rest };
        }
    }
    path
}

#[derive(Debug, Default)]
struct State {
    routes: Vec<Route>,
    calls: Vec<HttpRequest>,
}

/// A [`Transport`] that answers from routes and records every request. Clones share state.
#[derive(Clone, Debug, Default)]
pub struct FixtureTransport {
    state: Arc<Mutex<State>>,
}

#[derive(Deserialize)]
struct Manifest {
    routes: Vec<ManifestRoute>,
}

#[derive(Deserialize)]
struct ManifestRoute {
    method: String,
    path: String,
    #[serde(default)]
    query: BTreeMap<String, String>,
    responses: Vec<ManifestResponse>,
}

#[derive(Deserialize)]
struct ManifestResponse {
    #[serde(default = "default_status")]
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

fn default_status() -> u16 {
    200
}

impl FixtureTransport {
    /// An empty transport (every request gets the `fixtureNotFound` 404).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads routes from a `routes.json` manifest in `dir` (see `fixtures/graph/README.md`);
    /// response `file`s are relative to `dir`.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] if the manifest or a file is missing or invalid.
    pub fn from_manifest(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let cfg = |what: &str| Error::new(ErrorKind::Config, format!("fixture manifest: {what}"));
        let text = std::fs::read(dir.join("routes.json")).map_err(|e| cfg("cannot read routes.json").with_source(e))?;
        let manifest: Manifest =
            serde_json::from_slice(&text).map_err(|e| cfg("invalid routes.json").with_source(e))?;
        let transport = Self::new();
        for r in manifest.routes {
            let method = Method::parse(&r.method).ok_or_else(|| cfg("unknown method"))?;
            let mut route = Route::new(method, r.path);
            for (k, v) in r.query {
                route = route.query(k, v);
            }
            for resp in r.responses {
                let body = match (resp.file, resp.body) {
                    (Some(file), _) => {
                        std::fs::read(dir.join(&file)).map_err(|e| cfg("cannot read response file").with_source(e))?
                    }
                    (None, Some(body)) => serde_json::to_vec(&body).unwrap_or_default(),
                    (None, None) => Vec::new(),
                };
                let mut fr = FixtureResponse::bytes(resp.status, body);
                for (k, v) in &resp.headers {
                    fr = fr.with_header(k, v);
                }
                route = route.respond(fr);
            }
            transport.add(route);
        }
        Ok(transport)
    }

    /// [`FixtureTransport::from_manifest`] on [`fixtures_dir`].
    ///
    /// # Errors
    /// As [`FixtureTransport::from_manifest`].
    pub fn from_default_manifest() -> Result<Self> {
        Self::from_manifest(fixtures_dir())
    }

    /// Adds a route.
    pub fn add(&self, route: Route) -> &Self {
        self.lock().routes.push(route);
        self
    }

    /// Shorthand: `method path` always answers `response`.
    pub fn on(&self, method: Method, path: &str, response: FixtureResponse) -> &Self {
        self.add(Route::new(method, path).respond(response))
    }

    /// Every request received so far, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<HttpRequest> {
        self.lock().calls.clone()
    }

    /// Number of requests received so far.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lock().calls.len()
    }

    fn reply(&self, req: HttpRequest) -> FixtureResponse {
        let mut st = self.lock();
        let best = st
            .routes
            .iter()
            .enumerate()
            .filter(|(_, r)| r.matches(&req))
            .max_by(|(ia, a), (ib, b)| a.query.len().cmp(&b.query.len()).then(ib.cmp(ia)))
            .map(|(i, _)| i);
        st.calls.push(req);
        let Some(route) = best.and_then(|i| st.routes.get_mut(i)) else {
            return FixtureResponse::json(
                404,
                &serde_json::json!({"error": {"code": "fixtureNotFound", "message": "no fixture route matched"}}),
            );
        };
        if route.responses.len() > 1 {
            route.responses.pop_front().unwrap_or(FixtureResponse::status(500))
        } else {
            route.responses.front().cloned().unwrap_or(FixtureResponse::status(204))
        }
    }
}

impl Transport for FixtureTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        match self.reply(request) {
            FixtureResponse::Http(resp) => Ok(resp),
            FixtureResponse::NetworkError => Err(Error::new(ErrorKind::Network, "scripted network error")),
        }
    }
}
