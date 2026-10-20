//! Resumable large-file upload sessions (OneDrive / SharePoint `createUploadSession`).

use std::fmt;
use std::future::Future;

use serde::Deserialize;
use url::Url;
use uuid::Uuid;

use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::request::{Method, Request, ResourceFamily};

/// Chunk sizes must be a multiple of this (320 KiB), per the Graph upload-session docs.
pub const UPLOAD_CHUNK_UNIT: usize = 320 * 1024;
/// Largest chunk Graph accepts in one request (192 × 320 KiB = 60 MiB).
pub const MAX_UPLOAD_CHUNK_UNITS: usize = 192;

/// A validated chunk size: a multiple of 320 KiB, at most 60 MiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkSize(usize);

impl ChunkSize {
    /// `units` × 320 KiB, `1..=192`.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] outside that range.
    pub fn from_units(units: usize) -> Result<Self> {
        if units == 0 || units > MAX_UPLOAD_CHUNK_UNITS {
            return Err(Error::new(ErrorKind::Config, "chunk size must be 1..=192 units of 320 KiB"));
        }
        Ok(Self(units.saturating_mul(UPLOAD_CHUNK_UNIT)))
    }

    /// The size in bytes.
    #[must_use]
    pub fn bytes(self) -> usize {
        self.0
    }
}

impl Default for ChunkSize {
    /// 32 × 320 KiB = 10 MiB.
    fn default() -> Self {
        Self(32 * UPLOAD_CHUNK_UNIT)
    }
}

/// Bytes to upload, read chunk by chunk so large files need not sit in memory.
pub trait ChunkSource: Send + Sync {
    /// Total length in bytes.
    fn len(&self) -> u64;

    /// Whether the source is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads exactly `len` bytes starting at `offset`.
    fn read_at(&self, offset: u64, len: usize) -> impl Future<Output = Result<Vec<u8>>> + Send;
}

impl ChunkSource for [u8] {
    fn len(&self) -> u64 {
        u64::try_from(<[u8]>::len(self)).unwrap_or(u64::MAX)
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start = usize::try_from(offset).map_err(|_| Error::new(ErrorKind::Bug, "offset out of range"))?;
        let end = start.checked_add(len).ok_or_else(|| Error::new(ErrorKind::Bug, "offset out of range"))?;
        self.get(start..end).map(<[u8]>::to_vec).ok_or_else(|| Error::new(ErrorKind::Bug, "read past end of source"))
    }
}

impl ChunkSource for Vec<u8> {
    fn len(&self) -> u64 {
        ChunkSource::len(self.as_slice())
    }

    fn read_at(&self, offset: u64, len: usize) -> impl Future<Output = Result<Vec<u8>>> + Send {
        ChunkSource::read_at(self.as_slice(), offset, len)
    }
}

/// An upload session. The URL is pre-authenticated, so `Debug` redacts it.
#[derive(Clone)]
pub struct UploadSession {
    upload_url: Url,
    expiration: Option<String>,
    next_expected_ranges: Vec<String>,
}

impl fmt::Debug for UploadSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UploadSession")
            .field("upload_url", &"<redacted>")
            .field("expiration", &self.expiration)
            .field("next_expected_ranges", &self.next_expected_ranges)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawSession {
    #[serde(default, rename = "uploadUrl")]
    upload_url: Option<String>,
    #[serde(default, rename = "expirationDateTime")]
    expiration: Option<String>,
    #[serde(default, rename = "nextExpectedRanges")]
    next_expected_ranges: Option<Vec<String>>,
}

/// Most ranges kept from a status response.
const MAX_RANGES: usize = 64;

impl UploadSession {
    /// Rebuilds a stored session (e.g. after restart) to resume it.
    ///
    /// # Errors
    /// [`ErrorKind::Malformed`] if `upload_url` is not a URL.
    pub fn from_parts(upload_url: &str, expiration: Option<String>, next_expected_ranges: Vec<String>) -> Result<Self> {
        let upload_url =
            Url::parse(upload_url).map_err(|e| Error::new(ErrorKind::Malformed, "bad uploadUrl").with_source(e))?;
        Ok(Self { upload_url, expiration, next_expected_ranges })
    }

    /// The pre-authenticated upload URL. Treat as a secret.
    #[must_use]
    pub fn upload_url(&self) -> &Url {
        &self.upload_url
    }

    /// `expirationDateTime` as sent by Graph.
    #[must_use]
    pub fn expiration(&self) -> Option<&str> {
        self.expiration.as_deref()
    }

    /// `nextExpectedRanges`, e.g. `["327680-"]`.
    #[must_use]
    pub fn next_expected_ranges(&self) -> &[String] {
        &self.next_expected_ranges
    }

    /// The first byte Graph still expects, if the ranges say.
    #[must_use]
    pub fn next_offset(&self) -> Option<u64> {
        first_range_start(&self.next_expected_ranges)
    }
}

fn first_range_start(ranges: &[String]) -> Option<u64> {
    ranges.iter().filter_map(|r| r.split('-').next()?.trim().parse::<u64>().ok()).min()
}

fn parse_session(client: &Client, body: &[u8], fallback_url: Option<&Url>) -> Result<UploadSession> {
    let raw: RawSession = client.parse_json(body)?;
    let upload_url = match (raw.upload_url, fallback_url) {
        (Some(u), _) => Url::parse(&u).map_err(|e| Error::new(ErrorKind::Malformed, "bad uploadUrl").with_source(e))?,
        (None, Some(u)) => u.clone(),
        (None, None) => return Err(Error::new(ErrorKind::Malformed, "upload session has no uploadUrl")),
    };
    let mut ranges = raw.next_expected_ranges.unwrap_or_default();
    ranges.truncate(MAX_RANGES);
    Ok(UploadSession { upload_url, expiration: raw.expiration, next_expected_ranges: ranges })
}

impl Client {
    fn check_upload_url(&self, url: &Url) -> Result<()> {
        if self.is_graph_url(url) {
            return Ok(());
        }
        let host = url.host_str().unwrap_or("").to_ascii_lowercase();
        let host_ok = self
            .inner
            .upload_hosts
            .iter()
            .any(|s| host == *s || host.strip_suffix(s.as_str()).is_some_and(|rest| rest.ends_with('.')));
        if url.scheme() != "https" || !host_ok || !url.username().is_empty() || url.password().is_some() {
            return Err(Error::new(ErrorKind::Malformed, "upload URL is not on an allowed host"));
        }
        Ok(())
    }

    /// Creates an upload session. `req` is the caller's `POST .../createUploadSession` (path and
    /// optional body such as `{"item":{"@microsoft.graph.conflictBehavior":"rename"}}`).
    ///
    /// # Errors
    /// As [`Client::send_raw`]; `Malformed` if the response has no usable `uploadUrl` or it
    /// points at a host outside the allowed upload hosts.
    pub async fn create_upload_session(&self, req: Request) -> Result<UploadSession> {
        let req = if req.family.is_none() { req.family(ResourceFamily::Files) } else { req };
        let resp = self.send_raw(req).await?;
        let session = parse_session(self, &resp.body, None)?;
        self.check_upload_url(&session.upload_url)?;
        Ok(session)
    }

    async fn upload_call(
        &self,
        method: Method,
        session: &UploadSession,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        idempotent: bool,
    ) -> Result<crate::HttpResponse> {
        self.check_upload_url(&session.upload_url)?;
        let crid = Uuid::new_v4().to_string();
        // The upload URL is pre-authenticated: never send our bearer token to it.
        let authenticate = self.is_graph_url(&session.upload_url);
        self.execute_url(
            method,
            session.upload_url.clone(),
            headers,
            body,
            ResourceFamily::Files,
            "upload",
            idempotent,
            authenticate,
            &crid,
        )
        .await
    }

    /// Asks Graph which byte ranges it still expects (to resume after a failure or restart).
    ///
    /// # Errors
    /// [`ErrorKind::NotFound`] if the session expired; otherwise as [`Client::send_raw`].
    pub async fn upload_status(&self, session: &UploadSession) -> Result<UploadSession> {
        let resp = self.upload_call(Method::Get, session, Vec::new(), Vec::new(), true).await?;
        let resp = self.check(resp)?;
        let mut s = parse_session(self, &resp.body, Some(&session.upload_url))?;
        s.upload_url = session.upload_url.clone();
        Ok(s)
    }

    /// Cancels an upload session.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn cancel_upload(&self, session: &UploadSession) -> Result<()> {
        let resp = self.upload_call(Method::Delete, session, Vec::new(), Vec::new(), true).await?;
        self.check(resp).map(|_| ())
    }

    /// Uploads `source` in `chunk`-sized pieces, starting from the session's
    /// `nextExpectedRanges` (so a stored session resumes), and returns the created item's JSON.
    ///
    /// After a failed chunk (network, 5xx, 416) it asks [`Client::upload_status`] where to
    /// continue, up to the retry policy's attempt count.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for an empty source; `NotFound` if the session expired;
    /// `Malformed` if Graph expects bytes beyond the end; the last error once retries run out.
    pub async fn upload<S: ChunkSource + ?Sized>(
        &self,
        session: &UploadSession,
        source: &S,
        chunk: ChunkSize,
    ) -> Result<serde_json::Value> {
        let total = source.len();
        if total == 0 {
            return Err(Error::new(ErrorKind::Config, "upload sessions need a non-empty source"));
        }
        let chunk_len = u64::try_from(chunk.bytes()).unwrap_or(u64::MAX);
        let max_failures = self.inner.retry.max_attempts;
        let max_steps = total
            .div_ceil(chunk_len.max(1))
            .saturating_add(u64::from(max_failures).saturating_mul(2))
            .saturating_add(4);
        let mut offset = session.next_offset().unwrap_or(0);
        let mut failures: u32 = 0;
        let mut steps: u64 = 0;
        loop {
            steps = steps.saturating_add(1);
            if steps > max_steps {
                return Err(Error::new(ErrorKind::Malformed, "upload made no progress"));
            }
            if offset >= total {
                return Err(Error::new(ErrorKind::Malformed, "server expects bytes beyond the end of the file"));
            }
            let len = chunk_len.min(total.saturating_sub(offset));
            let len_usize = usize::try_from(len).map_err(|_| Error::new(ErrorKind::Bug, "chunk too large"))?;
            let data = source.read_at(offset, len_usize).await?;
            if data.len() != len_usize {
                return Err(Error::new(ErrorKind::Bug, "chunk source returned the wrong length"));
            }
            let last = offset.saturating_add(len).saturating_sub(1);
            let headers = vec![("Content-Range".to_owned(), format!("bytes {offset}-{last}/{total}"))];
            // Chunk PUTs are not retried blindly: on failure we ask the session where to resume.
            let result = self.upload_call(Method::Put, session, headers, data, false).await;
            let retryable = match result {
                Ok(resp) if matches!(resp.status, 200 | 201) => return self.parse_json(&resp.body),
                Ok(resp) if resp.status == 202 => {
                    let s = parse_session(self, &resp.body, Some(&session.upload_url))?;
                    match s.next_offset() {
                        Some(next) => {
                            offset = next;
                            failures = 0;
                            continue;
                        }
                        None => return Err(Error::new(ErrorKind::Malformed, "202 without nextExpectedRanges")),
                    }
                }
                Ok(resp) if resp.status == 404 => {
                    return Err(Error::new(ErrorKind::NotFound, "upload session expired").with_status(404));
                }
                Ok(resp) if resp.status == 416 || resp.status == 429 || resp.status >= 500 => {
                    let retry_after = self.retry_after_of(&resp);
                    let err = self.check(resp).err();
                    (err.unwrap_or_else(|| Error::new(ErrorKind::Server, "chunk failed")), retry_after)
                }
                Ok(resp) => {
                    return Err(self.check(resp).err().unwrap_or_else(|| Error::new(ErrorKind::Bug, "chunk failed")));
                }
                Err(e) if e.kind() == ErrorKind::Network => (e, None),
                Err(e) => return Err(e),
            };
            failures = failures.saturating_add(1);
            if failures >= max_failures {
                return Err(retryable.0);
            }
            let delay = retryable.1.unwrap_or_else(|| self.backoff(failures));
            tracing::info!(name: "graph.upload_resume", failures, "upload chunk failed; resuming from status");
            self.sleep(delay).await;
            let status = self.upload_status(session).await?;
            match status.next_offset() {
                Some(next) => offset = next,
                None => return Err(Error::new(ErrorKind::Malformed, "upload status has no nextExpectedRanges")),
            }
        }
    }
}
