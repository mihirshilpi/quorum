//! Typed Microsoft Graph REST client with paging, delta, batching and throttling.
//!
//! This is the transport-level layer of Quorum's Graph access (ADR 0003): endpoint-agnostic,
//! JSON in and out. Endpoint models live elsewhere.
//!
//! - [`Request`]: a plain value (method, path or `nextLink`, query, headers, body, idempotency)
//!   built without I/O.
//! - [`Client`]: cheap to clone; resolves requests against the Graph base URL (rejecting links to
//!   other hosts), adds the bearer token from a [`TokenSource`] and a fresh `client-request-id`,
//!   honours `Retry-After`, backs off with jitter, limits concurrency per bucket (mailbox) and rate
//!   per [`ResourceFamily`], and caps response size and JSON depth.
//! - Paging ([`Client::pages`]), delta ([`Client::delta_page`], [`DeltaOutcome`]), `$batch`
//!   ([`Client::batch`]) and upload sessions ([`Client::upload`]).
//! - Seams (M-MOCKABLE-SYSCALLS): [`Transport`], [`TokenSource`], [`Clock`]. With feature
//!   `test-util`: `FixtureTransport` and `FakeClock`.
//!
//! ```no_run
//! # async fn demo() -> quorum_graph::Result<()> {
//! use quorum_graph::{Client, Request, StaticToken};
//! let client = Client::builder(StaticToken::new("token")).build()?;
//! let me: serde_json::Value = client.send_json(Request::get("/me")).await?;
//! # let _ = me; Ok(()) }
//! ```

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::indexing_slicing
)]
#![warn(missing_docs)]

pub mod api;
mod batch;
mod client;
mod clock;
mod error;
#[cfg(any(test, feature = "test-util"))]
mod fixture;
mod http;
mod json;
mod paging;
mod request;
mod reqwest_transport;
mod throttle;
mod token;
mod upload;

pub use batch::{BatchItem, BatchResponse, MAX_BATCH_SIZE};
pub use client::{
    BETA_BASE_URL, Client, ClientBuilder, DEFAULT_BASE_URL, DEFAULT_BUCKET_CONCURRENCY, DEFAULT_UPLOAD_HOST_SUFFIXES,
    MAX_PAGES,
};
#[cfg(any(test, feature = "test-util"))]
pub use clock::FakeClock;
pub use clock::{Clock, SystemClock};
pub use error::{Error, ErrorKind, Result};
#[cfg(any(test, feature = "test-util"))]
pub use fixture::{FixtureResponse, FixtureTransport, Route, fixtures_dir};
pub use http::{HttpRequest, HttpResponse, Transport};
pub use json::DEFAULT_MAX_DEPTH;
pub use paging::{DeltaOutcome, DeltaPage, DeltaRound, Page};
pub use request::{Body, Method, Request, ResourceFamily};
pub use reqwest_transport::{DEFAULT_MAX_RESPONSE_BYTES, ReqwestConfig, ReqwestTransport};
pub use throttle::{RateLimit, RetryPolicy, parse_retry_after};
pub use token::{StaticToken, TokenSource};
pub use upload::{ChunkSize, ChunkSource, MAX_UPLOAD_CHUNK_UNITS, UPLOAD_CHUNK_UNIT, UploadSession};
