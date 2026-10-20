//! Collections: `value` arrays, `@odata.nextLink` paging and delta queries.

use std::collections::HashSet;

use futures::Stream;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::request::Request;

/// One page of a collection.
#[derive(Clone, Debug)]
pub struct Page<T> {
    /// Items that parsed as `T`.
    pub items: Vec<T>,
    /// Items in `value` that did not parse as `T` and were skipped (logged, never fatal).
    pub skipped: usize,
    /// The `@odata.nextLink`, when more pages exist.
    pub next_link: Option<String>,
}

/// Where a delta round stands after a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaOutcome {
    /// More changes are available now: follow this `@odata.nextLink`.
    Next(String),
    /// The round is complete: store this `@odata.deltaLink` and poll it later.
    Done(String),
}

/// One page of a delta query.
#[derive(Clone, Debug)]
pub struct DeltaPage<T> {
    /// Changed (or `@removed`) items that parsed as `T`.
    pub items: Vec<T>,
    /// Items that did not parse as `T` and were skipped.
    pub skipped: usize,
    /// Next link or final delta link.
    pub outcome: DeltaOutcome,
}

/// A whole delta round, all pages followed.
#[derive(Clone, Debug)]
pub struct DeltaRound<T> {
    /// All changed items, in page order.
    pub items: Vec<T>,
    /// Total items skipped as unparseable.
    pub skipped: usize,
    /// The `@odata.deltaLink` to poll next time.
    pub delta_link: String,
    /// Pages fetched.
    pub pages: usize,
}

#[derive(Deserialize)]
struct RawCollection {
    #[serde(default)]
    value: Option<Vec<serde_json::Value>>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

struct Parsed<T> {
    items: Vec<T>,
    skipped: usize,
    next_link: Option<String>,
    delta_link: Option<String>,
}

fn parse_collection<T: DeserializeOwned>(client: &Client, body: &[u8]) -> Result<Parsed<T>> {
    let raw: RawCollection = client.parse_json(body)?;
    let Some(values) = raw.value else {
        return Err(Error::new(ErrorKind::Malformed, "collection response has no `value` array"));
    };
    let mut items = Vec::with_capacity(values.len());
    let mut skipped = 0usize;
    for v in values {
        match serde_json::from_value::<T>(v) {
            Ok(item) => items.push(item),
            Err(_) => skipped = skipped.saturating_add(1),
        }
    }
    if skipped > 0 {
        tracing::warn!(name: "graph.items_skipped", skipped, "collection items failed to parse");
    }
    Ok(Parsed { items, skipped, next_link: raw.next_link, delta_link: raw.delta_link })
}

fn cap_error() -> Error {
    Error::new(ErrorKind::TooLarge, "page cap reached before the last page")
}

fn loop_error() -> Error {
    Error::new(ErrorKind::Malformed, "nextLink repeats an earlier page")
}

struct PageState {
    next: Option<Request>,
    pages: usize,
    seen: HashSet<String>,
}

impl Client {
    /// Fetches one page of a collection.
    ///
    /// # Errors
    /// As [`Client::send_raw`]; [`ErrorKind::Malformed`] if the response has no `value` array.
    pub async fn page<T: DeserializeOwned>(&self, req: Request) -> Result<Page<T>> {
        let resp = self.send_raw(req).await?;
        let p = parse_collection(self, &resp.body)?;
        Ok(Page { items: p.items, skipped: p.skipped, next_link: p.next_link })
    }

    /// Streams the pages of a collection, following `@odata.nextLink` (with the original
    /// request's headers) up to the client's page cap.
    ///
    /// The stream ends after the last page or the first error. Reaching the cap while more pages
    /// exist yields an [`ErrorKind::TooLarge`] error; a `nextLink` loop yields `Malformed`.
    pub fn pages<T>(&self, req: Request) -> impl Stream<Item = Result<Page<T>>> + Send + 'static
    where
        T: DeserializeOwned + Send + 'static,
    {
        self.pages_capped(req, self.inner.max_pages)
    }

    /// [`Client::pages`] with an explicit page cap.
    pub fn pages_capped<T>(
        &self,
        req: Request,
        max_pages: usize,
    ) -> impl Stream<Item = Result<Page<T>>> + Send + 'static
    where
        T: DeserializeOwned + Send + 'static,
    {
        let client = self.clone();
        let state = PageState { next: Some(req), pages: 0, seen: HashSet::new() };
        futures::stream::unfold(state, move |mut st| {
            let client = client.clone();
            async move {
                let req = st.next.take()?;
                if st.pages >= max_pages {
                    return Some((Err(cap_error()), st));
                }
                st.pages = st.pages.saturating_add(1);
                match client.page::<T>(req.clone()).await {
                    Ok(page) => {
                        if let Some(link) = &page.next_link {
                            if !st.seen.insert(link.clone()) {
                                return Some((Err(loop_error()), st));
                            }
                            st.next = Some(req.follow(link.clone()));
                        }
                        Some((Ok(page), st))
                    }
                    Err(e) => Some((Err(e), st)),
                }
            }
        })
    }

    /// Collects every item of a collection across pages, up to `max_pages`.
    ///
    /// # Errors
    /// The first page error, or [`ErrorKind::TooLarge`] if more than `max_pages` pages exist.
    pub async fn collect_all<T>(&self, req: Request, max_pages: usize) -> Result<Vec<T>>
    where
        T: DeserializeOwned + Send + 'static,
    {
        use futures::StreamExt;
        let mut out = Vec::new();
        let stream = self.pages_capped::<T>(req, max_pages);
        futures::pin_mut!(stream);
        while let Some(page) = stream.next().await {
            out.extend(page?.items);
        }
        Ok(out)
    }

    /// Fetches one page of a delta query (an initial `.../delta` request, a `nextLink` or a stored
    /// `deltaLink`).
    ///
    /// # Errors
    /// [`ErrorKind::DeltaExpired`] for 410 Gone / `syncStateNotFound` / `resyncRequired`: drop
    /// the stored link and start a fresh round. `Malformed` if the page has neither link.
    pub async fn delta_page<T: DeserializeOwned>(&self, req: Request) -> Result<DeltaPage<T>> {
        let resp = self.send_raw(req).await?;
        let p = parse_collection(self, &resp.body)?;
        let outcome = match (p.next_link, p.delta_link) {
            (Some(next), _) => DeltaOutcome::Next(next),
            (None, Some(delta)) => DeltaOutcome::Done(delta),
            (None, None) => {
                return Err(Error::new(ErrorKind::Malformed, "delta page has neither nextLink nor deltaLink"));
            }
        };
        Ok(DeltaPage { items: p.items, skipped: p.skipped, outcome })
    }

    /// Runs a whole delta round: follows `nextLink`s until a `deltaLink`, up to `max_pages`.
    ///
    /// # Errors
    /// As [`Client::delta_page`]; [`ErrorKind::TooLarge`] when the cap is hit; `Malformed` for a
    /// `nextLink` loop.
    pub async fn delta_round<T: DeserializeOwned>(&self, req: Request, max_pages: usize) -> Result<DeltaRound<T>> {
        let mut items = Vec::new();
        let mut skipped = 0usize;
        let mut seen = HashSet::new();
        let mut next = req.clone();
        for pages in 1..=max_pages {
            let page = self.delta_page::<T>(next).await?;
            items.extend(page.items);
            skipped = skipped.saturating_add(page.skipped);
            match page.outcome {
                DeltaOutcome::Done(delta_link) => return Ok(DeltaRound { items, skipped, delta_link, pages }),
                DeltaOutcome::Next(link) => {
                    if !seen.insert(link.clone()) {
                        return Err(loop_error());
                    }
                    next = req.follow(link);
                }
            }
        }
        Err(cap_error())
    }
}
