//! JSON `$batch`: up to 20 sub-requests per round trip.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::http::{HttpResponse, find_header};
use crate::json;
use crate::request::{Body, Request, ResourceFamily};
use crate::throttle::parse_retry_after;

/// Most sub-requests Graph accepts in one `$batch`.
pub const MAX_BATCH_SIZE: usize = 20;
/// Longest sub-request id accepted.
const MAX_ID_LEN: usize = 64;

/// A sub-request of a batch, with a caller-chosen id unique within the call.
#[derive(Clone, Debug)]
pub struct BatchItem {
    id: String,
    request: Request,
    depends_on: Vec<String>,
}

impl BatchItem {
    /// A sub-request with `id`.
    #[must_use]
    pub fn new(id: impl Into<String>, request: Request) -> Self {
        Self { id: id.into(), request, depends_on: Vec::new() }
    }

    /// Runs this item only after item `id` (which must appear earlier in the list) succeeds.
    #[must_use]
    pub fn depends_on(mut self, id: impl Into<String>) -> Self {
        self.depends_on.push(id.into());
        self
    }

    /// The id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The request.
    #[must_use]
    pub fn request(&self) -> &Request {
        &self.request
    }
}

/// The response to one sub-request. `Debug` omits the body.
#[derive(Clone)]
pub struct BatchResponse {
    id: String,
    status: u16,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

impl fmt::Debug for BatchResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BatchResponse")
            .field("id", &self.id)
            .field("status", &self.status)
            .field("has_body", &self.body.is_some())
            .finish_non_exhaustive()
    }
}

impl BatchResponse {
    /// The sub-request id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The HTTP status of the sub-request.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }

    /// The JSON body, if any.
    #[must_use]
    pub fn body(&self) -> Option<&Value> {
        self.body.as_ref()
    }

    /// Parses the body as `T` after checking the status.
    ///
    /// # Errors
    /// The mapped [`Error`] for a non-2xx status; [`ErrorKind::Malformed`] if the body does not
    /// parse as `T`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        if !self.is_success() {
            return Err(self.error());
        }
        Ok(serde_json::from_value(self.body.clone().unwrap_or(Value::Null))?)
    }

    /// The [`Error`] this sub-response represents (for a non-2xx status).
    #[must_use]
    pub fn error(&self) -> Error {
        let body = self.body.as_ref().and_then(|b| serde_json::to_vec(b).ok()).unwrap_or_default();
        let resp = HttpResponse { status: self.status, headers: self.headers.clone(), body };
        let retry_after = self.header("retry-after").and_then(|v| parse_retry_after(v, jiff::Timestamp::now()));
        json::error_from_response(&resp, retry_after)
    }
}

fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

/// Splits items into chunks of ≤ [`MAX_BATCH_SIZE`], keeping each dependency group together.
fn plan(items: &[BatchItem]) -> Result<Vec<Vec<usize>>> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if item.id.is_empty() || item.id.len() > MAX_ID_LEN {
            return Err(config("batch item id must be 1..=64 bytes"));
        }
        for dep in &item.depends_on {
            if !index.contains_key(dep.as_str()) {
                return Err(config("dependsOn must name an earlier item in the batch"));
            }
        }
        if index.insert(item.id.as_str(), i).is_some() {
            return Err(config("batch item ids must be unique"));
        }
        if matches!(item.request.body, Body::Bytes { .. }) {
            return Err(config("batch items cannot carry raw byte bodies"));
        }
    }
    // Union-find over dependency edges.
    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        for _ in 0..parent.len() {
            let p = parent.get(x).copied().unwrap_or(x);
            if p == x {
                break;
            }
            x = p;
        }
        x
    }
    for (i, item) in items.iter().enumerate() {
        for dep in &item.depends_on {
            if let Some(&j) = index.get(dep.as_str()) {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if let Some(slot) = parent.get_mut(a) {
                    *slot = b;
                }
            }
        }
    }
    // Groups in order of first appearance.
    let mut group_of_root: HashMap<usize, usize> = HashMap::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in 0..items.len() {
        let root = find(&mut parent, i);
        let g = *group_of_root.entry(root).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len().saturating_sub(1)
        });
        if let Some(group) = groups.get_mut(g) {
            group.push(i);
        }
    }
    let mut chunks: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for group in groups {
        if group.len() > MAX_BATCH_SIZE {
            return Err(config("a dependsOn chain is longer than 20 items"));
        }
        if current.len().saturating_add(group.len()) > MAX_BATCH_SIZE {
            chunks.push(std::mem::take(&mut current));
        }
        current.extend(group);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    for chunk in &mut chunks {
        chunk.sort_unstable();
    }
    Ok(chunks)
}

impl Client {
    /// The sub-request `url` (relative to the version, e.g. `/me/messages?$top=1`).
    fn batch_url(&self, req: &Request) -> Result<String> {
        let url = self.resolve(req)?;
        let path = url.path().strip_prefix(self.inner.base_path.as_str()).unwrap_or(url.path());
        let path = if path.is_empty() { "/" } else { path };
        Ok(match url.query() {
            Some(q) => format!("{path}?{q}"),
            None => path.to_owned(),
        })
    }

    fn batch_envelope(&self, items: &[BatchItem], chunk: &[usize]) -> Result<Value> {
        let mut requests = Vec::with_capacity(chunk.len());
        for &i in chunk {
            let Some(item) = items.get(i) else { continue };
            let req = &item.request;
            let mut obj = Map::new();
            obj.insert("id".into(), Value::String(item.id.clone()));
            obj.insert("method".into(), Value::String(req.method.as_str().to_owned()));
            obj.insert("url".into(), Value::String(self.batch_url(req)?));
            let mut headers = Map::new();
            for (k, v) in &req.headers {
                headers.insert(k.clone(), Value::String(v.clone()));
            }
            if let Body::Json(body) = &req.body {
                headers.entry("Content-Type").or_insert_with(|| Value::String("application/json".into()));
                obj.insert("body".into(), body.clone());
            }
            if !headers.is_empty() {
                obj.insert("headers".into(), Value::Object(headers));
            }
            if !item.depends_on.is_empty() {
                let deps = item.depends_on.iter().cloned().map(Value::String).collect();
                obj.insert("dependsOn".into(), Value::Array(deps));
            }
            requests.push(Value::Object(obj));
        }
        Ok(serde_json::json!({ "requests": requests }))
    }

    /// Sends `items` as one or more `$batch` calls (≤ 20 each; dependency chains stay together)
    /// and returns one [`BatchResponse`] per item, in input order.
    ///
    /// Sub-requests answered 429/503/504 are retried individually after their own `Retry-After`
    /// (idempotent ones only); items that failed with 424 because of them are retried afterwards
    /// once their dependencies succeed.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] for duplicate ids, unknown or forward `dependsOn`, byte bodies or a
    /// chain over 20; errors of the `$batch` call itself; `Malformed` if a response is missing.
    pub async fn batch(&self, items: Vec<BatchItem>) -> Result<Vec<BatchResponse>> {
        let chunks = plan(&items)?;
        let mut results: Vec<Option<BatchResponse>> = vec![None; items.len()];
        for chunk in &chunks {
            let envelope = self.batch_envelope(&items, chunk)?;
            let all_idempotent = chunk.iter().filter_map(|&i| items.get(i)).all(|it| it.request.is_idempotent());
            let first_family = chunk.first().and_then(|&i| items.get(i)).map(|it| it.request.resource_family());
            let mut req = Request::post("/$batch")
                .json_value(envelope)
                .idempotent(all_idempotent)
                .family(first_family.unwrap_or(ResourceFamily::Other));
            if let Some(bucket) = chunk.first().and_then(|&i| items.get(i)).and_then(|it| it.request.bucket.clone()) {
                req = req.bucket(bucket);
            }
            let resp = self.send_raw(req).await?;
            let body: Value = self.parse_json(&resp.body)?;
            let responses = body.get("responses").and_then(Value::as_array).cloned().unwrap_or_default();
            let wanted: HashMap<&str, usize> =
                chunk.iter().filter_map(|&i| items.get(i).map(|it| (it.id.as_str(), i))).collect();
            for r in responses {
                let Some(parsed) = parse_sub_response(&r) else { continue };
                if let Some(&i) = wanted.get(parsed.id.as_str())
                    && let Some(slot) = results.get_mut(i)
                {
                    *slot = Some(parsed);
                }
            }
        }
        self.retry_throttled(&items, &mut results).await;
        let mut out = Vec::with_capacity(items.len());
        for r in results {
            match r {
                Some(r) => out.push(r),
                None => return Err(Error::new(ErrorKind::Malformed, "batch response is missing a sub-response")),
            }
        }
        Ok(out)
    }

    async fn retry_throttled(&self, items: &[BatchItem], results: &mut [Option<BatchResponse>]) {
        let now_utc = self.inner.clock.now_utc();
        let max_wait = self.inner.retry.max_retry_after;
        let mut throttled: Vec<(Duration, usize)> = Vec::new();
        for (i, r) in results.iter().enumerate() {
            let Some(r) = r else { continue };
            let Some(item) = items.get(i) else { continue };
            if matches!(r.status, 429 | 503 | 504) && item.request.is_idempotent() {
                let wait = r.header("retry-after").and_then(|v| parse_retry_after(v, now_utc));
                let wait = wait.unwrap_or_else(|| self.backoff(1));
                if wait <= max_wait {
                    throttled.push((wait, i));
                }
            }
        }
        if throttled.is_empty() {
            return;
        }
        throttled.sort_by_key(|&(wait, i)| (wait, i));
        let mut waited = Duration::ZERO;
        for (wait, i) in throttled {
            if wait > waited {
                self.sleep(wait.saturating_sub(waited)).await;
                waited = wait;
            }
            self.retry_one(items, results, i).await;
        }
        // Items that failed only because a dependency was throttled.
        let mut ok: HashSet<String> = HashSet::new();
        for i in 0..items.len() {
            let (Some(item), Some(Some(r))) = (items.get(i), results.get(i)) else { continue };
            if r.is_success() {
                ok.insert(item.id.clone());
                continue;
            }
            if r.status == 424
                && item.request.is_idempotent()
                && !item.depends_on.is_empty()
                && item.depends_on.iter().all(|d| ok.contains(d))
            {
                self.retry_one(items, results, i).await;
                if results.get(i).and_then(Option::as_ref).is_some_and(BatchResponse::is_success) {
                    ok.insert(item.id.clone());
                }
            }
        }
    }

    async fn retry_one(&self, items: &[BatchItem], results: &mut [Option<BatchResponse>], i: usize) {
        let Some(item) = items.get(i) else { return };
        match self.execute(&item.request).await {
            Ok(resp) => {
                let body = if resp.body.is_empty() { None } else { self.parse_json::<Value>(&resp.body).ok() };
                if let Some(slot) = results.get_mut(i) {
                    *slot =
                        Some(BatchResponse { id: item.id.clone(), status: resp.status, headers: resp.headers, body });
                }
            }
            Err(e) => {
                tracing::warn!(name: "graph.batch_retry_failed", kind = e.kind().as_str(), "batch sub-request retry failed");
            }
        }
    }
}

fn parse_sub_response(v: &Value) -> Option<BatchResponse> {
    let id = v.get("id")?.as_str()?;
    if id.len() > MAX_ID_LEN {
        return None;
    }
    let status = u16::try_from(v.get("status")?.as_u64()?).ok()?;
    let headers = v
        .get("headers")
        .and_then(Value::as_object)
        .map(|h| h.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned()))).collect())
        .unwrap_or_default();
    let body = v.get("body").cloned();
    Some(BatchResponse { id: id.to_owned(), status, headers, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> BatchItem {
        BatchItem::new(id, Request::get("/me"))
    }

    #[test]
    fn plan_chunks_by_twenty_and_keeps_chains_together() {
        let mut items: Vec<BatchItem> = (0..19).map(|i| item(&format!("a{i}"))).collect();
        items.push(item("c0"));
        items.push(item("c1").depends_on("c0"));
        let chunks = plan(&items).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 19);
        assert_eq!(chunks[1], vec![19, 20]);
    }

    #[test]
    fn plan_rejects_bad_items() {
        assert_eq!(plan(&[item("x"), item("x")]).unwrap_err().kind(), ErrorKind::Config);
        assert_eq!(plan(&[item("x").depends_on("y"), item("y")]).unwrap_err().kind(), ErrorKind::Config);
        let chain: Vec<BatchItem> = (0..21)
            .map(|i| if i == 0 { item("n0") } else { item(&format!("n{i}")).depends_on(format!("n{}", i - 1)) })
            .collect();
        assert_eq!(plan(&chain).unwrap_err().kind(), ErrorKind::Config);
    }
}
