//! The HTTP seam for the token and device-code endpoints (M-MOCKABLE-SYSCALLS).

use std::fmt;
use std::time::Duration;

use url::Url;

use crate::BoxFuture;
use crate::error::{Error, ErrorKind, Result};

/// Largest response body we read from the identity provider.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// A form POST to the identity provider.
#[derive(Clone)]
pub struct FormRequest {
    /// The endpoint URL (no secrets in it).
    pub url: Url,
    /// The `application/x-www-form-urlencoded` fields. Values may be secrets.
    pub form: Vec<(String, String)>,
}

impl FormRequest {
    /// The value of form field `name`, if present.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.form.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The URL-encoded body.
    #[must_use]
    pub fn encoded_body(&self) -> String {
        url::form_urlencoded::Serializer::new(String::new()).extend_pairs(&self.form).finish()
    }
}

impl fmt::Debug for FormRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&str> = self.form.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("FormRequest").field("url", &self.url.as_str()).field("fields", &keys).finish()
    }
}

/// The identity provider's answer: status and (capped) body.
#[derive(Clone)]
pub struct FormResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body, at most [`MAX_RESPONSE_BYTES`].
    pub body: Vec<u8>,
}

impl fmt::Debug for FormResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormResponse").field("status", &self.status).field("body_len", &self.body.len()).finish()
    }
}

/// Sends form POSTs to the identity provider.
///
/// Production uses [`ReqwestTransport`]; tests use `FakeTransport` (feature `test-util`).
pub trait Transport: fmt::Debug + Send + Sync + 'static {
    /// POSTs `request` and returns the status and body.
    ///
    /// # Errors
    /// [`ErrorKind::Network`] when the request cannot be sent or the body cannot be read;
    /// [`ErrorKind::InvalidResponse`] when the body exceeds [`MAX_RESPONSE_BYTES`].
    fn post_form(&self, request: FormRequest) -> BoxFuture<'_, Result<FormResponse>>;
}

/// [`Transport`] over `reqwest` with rustls, no redirects and a 30 s timeout.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Builds the HTTP client.
    ///
    /// # Errors
    /// [`ErrorKind::Network`] when the TLS backend cannot be initialised.
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("Quorum/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Uses an existing `reqwest` client (it should not follow redirects).
    #[must_use]
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Transport for ReqwestTransport {
    fn post_form(&self, request: FormRequest) -> BoxFuture<'_, Result<FormResponse>> {
        Box::pin(async move {
            let mut resp = self
                .client
                .post(request.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(request.encoded_body())
                .send()
                .await?;
            let status = resp.status().as_u16();
            if resp.content_length().is_some_and(|l| l > MAX_RESPONSE_BYTES as u64) {
                return Err(Error::new(ErrorKind::InvalidResponse, "token response too large"));
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(Error::new(ErrorKind::InvalidResponse, "token response too large"));
                }
                body.extend_from_slice(&chunk);
            }
            Ok(FormResponse { status, body })
        })
    }
}

#[cfg(any(test, feature = "test-util"))]
pub use fake::FakeTransport;

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::collections::VecDeque;
    use std::fmt;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use super::{FormRequest, FormResponse, Transport};
    use crate::BoxFuture;
    use crate::error::{Error, ErrorKind, Result};

    type Handler = Arc<dyn Fn(&FormRequest) -> Result<FormResponse> + Send + Sync>;

    /// A scripted [`Transport`] for tests: queued responses first, then the handler.
    ///
    /// Clones share the queue and the request log.
    #[derive(Clone, Default)]
    pub struct FakeTransport {
        queue: Arc<Mutex<VecDeque<Result<FormResponse>>>>,
        handler: Option<Handler>,
        requests: Arc<Mutex<Vec<FormRequest>>>,
        delay: Duration,
    }

    impl FakeTransport {
        /// A transport with an empty queue and no handler (requests fail with `Network`).
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Answers requests the queue does not cover with `handler`.
        #[must_use]
        pub fn with_handler(
            mut self,
            handler: impl Fn(&FormRequest) -> Result<FormResponse> + Send + Sync + 'static,
        ) -> Self {
            self.handler = Some(Arc::new(handler));
            self
        }

        /// Waits `delay` (tokio time) before answering, to let concurrent callers overlap.
        #[must_use]
        pub fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        /// Queues a JSON response.
        pub fn push_json(&self, status: u16, body: &str) {
            self.push(Ok(FormResponse { status, body: body.as_bytes().to_vec() }));
        }

        /// Queues any response or error.
        pub fn push(&self, response: Result<FormResponse>) {
            self.queue.lock().unwrap_or_else(PoisonError::into_inner).push_back(response);
        }

        /// Every request received so far.
        #[must_use]
        pub fn requests(&self) -> Vec<FormRequest> {
            self.requests.lock().unwrap_or_else(PoisonError::into_inner).clone()
        }
    }

    impl fmt::Debug for FakeTransport {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("FakeTransport").finish_non_exhaustive()
        }
    }

    impl Transport for FakeTransport {
        fn post_form(&self, request: FormRequest) -> BoxFuture<'_, Result<FormResponse>> {
            Box::pin(async move {
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                self.requests.lock().unwrap_or_else(PoisonError::into_inner).push(request.clone());
                let queued = self.queue.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
                match (queued, &self.handler) {
                    (Some(r), _) => r,
                    (None, Some(h)) => h(&request),
                    (None, None) => Err(Error::new(ErrorKind::Network, "fake transport has no response")),
                }
            })
        }
    }
}
