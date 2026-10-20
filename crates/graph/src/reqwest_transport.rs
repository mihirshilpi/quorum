//! The production [`Transport`] on reqwest (rustls, HTTP/2, gzip/brotli).

use std::time::Duration;

use crate::error::{Error, ErrorKind, Result};
use crate::http::{HttpRequest, HttpResponse, Transport};
use crate::request::Method;

/// Default cap on a response body after decompression.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Settings for [`ReqwestTransport`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReqwestConfig {
    /// TCP + TLS connect timeout.
    pub connect_timeout: Duration,
    /// Whole-request timeout (send + receive).
    pub timeout: Duration,
    /// Largest accepted (decompressed) body; larger responses fail with [`ErrorKind::TooLarge`].
    pub max_response_bytes: usize,
}

impl Default for ReqwestConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            timeout: Duration::from_secs(120),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

/// HTTPS transport using reqwest with rustls. Redirects are not followed (the caller sees 3xx),
/// so a redirect can never carry a request to a host the client did not approve.
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    max_response_bytes: usize,
}

impl ReqwestTransport {
    /// Builds the transport.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] if the TLS backend cannot be initialised.
    pub fn new(config: &ReqwestConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .gzip(true)
            .brotli(true)
            .connect_timeout(config.connect_timeout)
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("Quorum/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::new(ErrorKind::Config, "cannot build HTTP client").with_source(e))?;
        Ok(Self { client, max_response_bytes: config.max_response_bytes })
    }
}

fn method(m: Method) -> reqwest::Method {
    match m {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Patch => reqwest::Method::PATCH,
        Method::Delete => reqwest::Method::DELETE,
        Method::Head => reqwest::Method::HEAD,
    }
}

fn network(e: reqwest::Error) -> Error {
    let detail = if e.is_timeout() {
        "request timed out"
    } else if e.is_connect() {
        "connection failed"
    } else {
        "HTTP transport error"
    };
    // reqwest errors can include the URL; strip it before keeping the source.
    Error::new(ErrorKind::Network, detail).with_source(e.without_url())
}

impl Transport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &request.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::new(ErrorKind::Config, "invalid header name"))?;
            let value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| Error::new(ErrorKind::Config, "invalid header value"))?;
            headers.append(name, value);
        }
        let mut builder = self.client.request(method(request.method), request.url).headers(headers);
        if !request.body.is_empty() {
            builder = builder.body(request.body);
        }
        let mut resp = builder.send().await.map_err(network)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(n, v)| v.to_str().ok().map(|v| (n.as_str().to_owned(), v.to_owned())))
            .collect();
        let cap = self.max_response_bytes;
        let too_large = || Error::new(ErrorKind::TooLarge, "response body exceeds cap");
        if resp.content_length().is_some_and(|len| usize::try_from(len).map_or(true, |len| len > cap)) {
            return Err(too_large());
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(network)? {
            if body.len().saturating_add(chunk.len()) > cap {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse { status, headers, body })
    }
}
