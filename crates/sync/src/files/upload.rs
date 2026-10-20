//! Uploading local files: a simple `PUT` up to 4 MiB, otherwise a resumable upload session
//! whose URL is kept in an encrypted blob so a restart resumes where Graph left off.

use std::path::PathBuf;

use quorum_graph::api::files::{ConflictBehavior, create_upload_session_request};
use quorum_graph::{ChunkSize, ChunkSource, UploadSession};
use quorum_model::{DriveId, DriveItem, DriveItemId};
use quorum_store::Store;
use serde::{Deserialize, Serialize};

use super::delta::gerr;
use super::state::{self, UploadState};
use crate::{Ctx, Error, ErrorKind, Result};

/// Files up to this size use a single `PUT`.
pub const SMALL_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024;

/// A local file read chunk by chunk. Each chunk read records the bytes Graph has accepted so
/// far (everything before the chunk's offset) as upload progress.
#[derive(Debug, Clone)]
pub struct FileSource {
    path: PathBuf,
    len: u64,
    progress: Option<(Store, String)>,
}

impl FileSource {
    /// Opens `path` for reading; the length is taken now.
    ///
    /// # Errors
    /// [`ErrorKind::Permanent`] if it is not a readable regular file.
    pub async fn open(path: PathBuf) -> Result<Self> {
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| Error::new(ErrorKind::Permanent, "the file to upload is not readable").with_source(e))?;
        if !meta.is_file() {
            return Err(Error::new(ErrorKind::Permanent, "only regular files can be uploaded"));
        }
        Ok(Self { path, len: meta.len(), progress: None })
    }

    /// Records progress of operation `op` in `store` while reading.
    #[must_use]
    pub fn with_progress(mut self, store: Store, op: String) -> Self {
        self.progress = Some((store, op));
        self
    }

    async fn read_all(&self) -> Result<Vec<u8>> {
        tokio::fs::read(&self.path)
            .await
            .map_err(|e| Error::new(ErrorKind::Permanent, "the file to upload is not readable").with_source(e))
    }
}

impl ChunkSource for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    async fn read_at(&self, offset: u64, len: usize) -> quorum_graph::Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
        let io = |e: std::io::Error| {
            quorum_graph::Error::new(quorum_graph::ErrorKind::Config, "reading the local file failed").with_source(e)
        };
        if let Some((store, op)) = &self.progress {
            // Best effort: progress is cosmetic.
            let _ = state::set_upload_state(store, op.clone(), UploadState::Uploading, Some(offset)).await;
        }
        let mut f = tokio::fs::File::open(&self.path).await.map_err(io)?;
        f.seek(std::io::SeekFrom::Start(offset)).await.map_err(io)?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf).await.map_err(io)?;
        Ok(buf)
    }
}

/// The stored form of an upload session (the URL is a secret; kept only in an encrypted blob).
#[derive(Serialize, Deserialize)]
struct StoredSession {
    url: String,
    expiration: Option<String>,
}

/// What to upload.
#[derive(Debug, Clone)]
pub(crate) struct UploadJob {
    pub drive: DriveId,
    pub parent: DriveItemId,
    pub name: String,
    pub op: String,
    pub conflict: ConflictBehavior,
}

/// Guesses a content type from the extension (Graph re-detects it; this only has to be valid).
fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "txt" | "md" | "csv" | "log" => "text/plain",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Uploads `source` per `job`. Large files resume a stored session when there is one; an
/// expired session starts over.
pub(crate) async fn upload_file(ctx: &Ctx, job: &UploadJob, source: FileSource) -> Result<DriveItem> {
    if source.len <= SMALL_UPLOAD_LIMIT {
        let data = source.read_all().await?;
        return ctx
            .graph
            .upload_small(&job.drive, &job.parent, &job.name, content_type(&job.name), data, job.conflict)
            .await
            .map_err(gerr);
    }
    let blob = state::upload_session_blob(&job.op);
    let mut session = match load_session(&ctx.store, &blob).await {
        Some(s) => match ctx.graph.upload_status(&s).await {
            Ok(status) => Some(status),
            Err(e) if e.kind() == quorum_graph::ErrorKind::NotFound => {
                tracing::info!(name: "files.upload_session_expired", "upload session expired; starting over");
                let _ = ctx.store.delete_blob(blob.clone()).await;
                None
            }
            Err(e) => return Err(gerr(e)),
        },
        None => None,
    };
    if session.is_none() {
        let req = create_upload_session_request(&job.drive, &job.parent, &job.name, job.conflict).map_err(gerr)?;
        let s = ctx.graph.create_upload_session(req).await.map_err(gerr)?;
        remember_upload_session(&ctx.store, &job.op, s.upload_url().as_str(), s.expiration()).await?;
        session = Some(s);
    }
    let Some(session) = session else { return Err(Error::new(ErrorKind::Bug, "no upload session")) };
    let value = match ctx.graph.upload(&session, &source, ChunkSize::default()).await {
        Ok(v) => v,
        Err(e) if e.kind() == quorum_graph::ErrorKind::NotFound => {
            let _ = ctx.store.delete_blob(blob.clone()).await;
            return Err(Error::new(ErrorKind::Remote, "upload session expired; will start over").with_source(e));
        }
        Err(e) => return Err(gerr(e)),
    };
    let _ = ctx.store.delete_blob(blob).await;
    quorum_graph::api::files::parse_drive_item(value, Some(&job.drive))
        .ok_or_else(|| Error::new(ErrorKind::Permanent, "uploaded item could not be read"))
}

/// Stores the upload session of operation `op` (encrypted) so a later attempt, even after a
/// restart, resumes it instead of starting over.
///
/// # Errors
/// The store failed.
pub async fn remember_upload_session(store: &Store, op: &str, url: &str, expiration: Option<&str>) -> Result<()> {
    let stored = StoredSession { url: url.to_owned(), expiration: expiration.map(str::to_owned) };
    let bytes = serde_json::to_vec(&stored).map_err(|e| Error::new(ErrorKind::Bug, "encode session").with_source(e))?;
    store.put_blob(state::upload_session_blob(op), bytes).await?;
    Ok(())
}

async fn load_session(store: &Store, blob: &str) -> Option<UploadSession> {
    let bytes = store.get_blob(blob.to_owned()).await.ok().flatten()?;
    let s: StoredSession = serde_json::from_slice(&bytes).ok()?;
    UploadSession::from_parts(&s.url, s.expiration, Vec::new()).ok()
}

#[cfg(test)]
mod tests {
    use super::content_type;

    #[test]
    fn content_types_are_valid() {
        assert_eq!(content_type("a.PDF"), "application/pdf");
        assert_eq!(content_type("noext"), "application/octet-stream");
        assert_eq!(content_type("x.docx"), "application/octet-stream");
    }
}
