//! Last-resort panic guard (M-PANIC-CONTINUATION).
//!
//! Every command and every sync task runs under [`guarded`]. A panic that escapes is turned
//! into an [`ErrorKind::Bug`] error; the caller then restarts the affected unit from stored
//! state. Production code has no panics by lint, so this only catches bugs in dependencies.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures::FutureExt;

use crate::error::{Error, ErrorKind, Result};

/// Runs `fut`, converting an escaped panic into an error.
pub async fn guarded<T>(what: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!(target: "quorum::guard", unit = what, detail = %detail, "panic caught");
            Err(Error::new(ErrorKind::Bug, format!("{what} failed unexpectedly; your data is safe")))
        }
    }
}
