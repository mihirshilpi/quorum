//! Wire-level request/response values and the [`Transport`] seam (M-MOCKABLE-SYSCALLS).

use std::fmt;
use std::future::Future;

use futures::future::BoxFuture;
use serde::de::DeserializeOwned;
use url::Url;

use crate::error::Result;
use crate::json;
use crate::request::Method;

/// A fully resolved HTTP request handed to a [`Transport`].
///
/// `Debug` redacts the URL's path and query, the `Authorization` header and the body.
#[derive(Clone)]
pub struct HttpRequest {
    /// The method.
    pub method: Method,
    /// The absolute URL.
    pub url: Url,
    /// Headers, in order.
    pub headers: Vec<(String, String)>,
    /// The body (empty for none).
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// The first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("host", &self.url.host_str())
            .field("headers", &names)
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// An HTTP response returned by a [`Transport`]. Header names are compared case-insensitively.
#[derive(Clone)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// Headers, in order.
    pub headers: Vec<(String, String)>,
    /// The (decompressed) body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A response with `status`, no headers and an empty body.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }

    /// The first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    /// Graph's `request-id` header, for support.
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.header("request-id")
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parses the body as JSON under the default depth cap. An empty body parses as `null`.
    ///
    /// # Errors
    /// [`ErrorKind::Malformed`](crate::ErrorKind::Malformed) for invalid or too-deep JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        json::parse(&self.body, json::DEFAULT_MAX_DEPTH)
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("request_id", &self.request_id())
            .field("body_len", &self.body.len())
            .finish_non_exhaustive()
    }
}

pub(crate) fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Sends HTTP requests. Production uses [`ReqwestTransport`](crate::ReqwestTransport); tests use
/// `FixtureTransport` (feature `test-util`).
///
/// Implementations return non-2xx responses as `Ok`; only failures to get a response at all
/// (connection, TLS, timeout, oversized body) are errors, normally
/// [`ErrorKind::Network`](crate::ErrorKind::Network) or
/// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge).
pub trait Transport: Send + Sync + 'static {
    /// Sends `request` and returns the response.
    fn send(&self, request: HttpRequest) -> impl Future<Output = Result<HttpResponse>> + Send;
}

/// Object-safe form of [`Transport`], used inside the client.
pub(crate) trait DynTransport: Send + Sync {
    fn send_dyn(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>>;
}

impl<T: Transport> DynTransport for T {
    fn send_dyn(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse>> {
        Box::pin(self.send(request))
    }
}
