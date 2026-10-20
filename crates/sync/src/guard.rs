//! Last-resort panic guard for sync passes, discovery and outbox handlers (M-PANIC-CONTINUATION).

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;

use crate::error::{Error, ErrorKind, Result};

/// Runs `fut`, turning an escaped panic into an [`ErrorKind::Bug`] error.
pub(crate) async fn guarded<T>(what: &'static str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => {
            // The payload may carry tenant data from a dependency's message: never log it.
            tracing::error!(name: "sync.panic_caught", unit = what, "panic caught in sync unit");
            Err(Error::new(ErrorKind::Bug, format!("{what} failed unexpectedly")))
        }
    }
}

/// Runs the synchronous `f`, turning an escaped panic into `None`.
pub(crate) fn guarded_sync<T>(what: &'static str, f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::error!(name: "sync.panic_caught", unit = what, "panic caught in sync unit");
            None
        }
    }
}
