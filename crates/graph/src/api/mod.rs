//! Typed wrappers for the Graph endpoints Quorum uses, returning `quorum-model` types.
//!
//! Each wrapper comes in two halves: a `*_request` function that builds a [`Request`](crate::Request)
//! without I/O (so tests and `$batch` callers can inspect or reuse it) and an `async` call on a
//! [`Client`](crate::Client) that sends it and maps the JSON to model types.

pub mod calendar;
pub mod chat;
pub mod files;
pub mod mail;
pub mod people;
pub mod presence;
pub mod search;
pub mod sites;
pub mod teams;
