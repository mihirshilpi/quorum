//! Fakes shared by the sync engine tests: a scripted resource, module and outbox handler, a
//! fixture-backed Graph client and an in-memory store.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use quorum_graph::{Client, FakeClock, FixtureTransport, RateLimit, ResourceFamily, StaticToken};
use quorum_store::Store;
use quorum_sync::{Cadence, Ctx, Error, ErrorKind, Module, OutboxHandler, Outcome, Resource, Result};
use serde_json::Value;
use tokio::time::Instant;

pub const CLIENT_ID: &str = "00000000-0000-0000-0000-0000000000c1";

pub fn graph(t: &FixtureTransport) -> Client {
    let mut b = Client::builder(StaticToken::new("test-token"));
    for f in ResourceFamily::ALL {
        b = b.rate(f, RateLimit::new(10_000.0, 10_000).unwrap());
    }
    b.transport(t.clone()).clock(FakeClock::new()).jitter_seed(1).build().unwrap()
}

pub fn ctx(t: &FixtureTransport) -> Ctx {
    Ctx { graph: graph(t), store: Store::open_in_memory().unwrap(), client_id: CLIENT_ID.to_owned() }
}

/// One scripted pass result.
#[derive(Debug, Clone)]
pub enum Step {
    Ok(Outcome),
    Err(ErrorKind, Option<Duration>),
    Panic,
    /// Sleep this long, then catch up.
    Slow(Duration),
}

/// A resource that plays a script (then catches up forever) and records when it ran.
pub struct FakeResource {
    pub key: String,
    pub area: &'static str,
    pub cadence: Mutex<Cadence>,
    pub script: Mutex<VecDeque<Step>>,
    pub runs: Mutex<Vec<Instant>>,
    pub active: Arc<AtomicUsize>,
    pub max_active: Arc<AtomicUsize>,
}

impl FakeResource {
    pub fn new(key: &str, area: &'static str, cadence: Cadence) -> Arc<Self> {
        Arc::new(Self {
            key: key.to_owned(),
            area,
            cadence: Mutex::new(cadence),
            script: Mutex::new(VecDeque::new()),
            runs: Mutex::new(Vec::new()),
            active: Arc::default(),
            max_active: Arc::default(),
        })
    }

    pub fn script(self: &Arc<Self>, steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        self.script.lock().unwrap().extend(steps);
        Arc::clone(self)
    }

    pub fn count(&self) -> usize {
        self.runs.lock().unwrap().len()
    }

    pub fn runs(&self) -> Vec<Instant> {
        self.runs.lock().unwrap().clone()
    }

    /// Gaps between consecutive runs.
    pub fn gaps(&self) -> Vec<Duration> {
        let r = self.runs();
        r.windows(2).map(|w| w[1] - w[0]).collect()
    }
}

impl Resource for FakeResource {
    fn key(&self) -> String {
        self.key.clone()
    }

    fn area(&self) -> &'static str {
        self.area
    }

    fn cadence(&self) -> Cadence {
        *self.cadence.lock().unwrap()
    }

    fn run<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            self.runs.lock().unwrap().push(Instant::now());
            let step = self.script.lock().unwrap().pop_front().unwrap_or(Step::Ok(Outcome::CaughtUp));
            match step {
                Step::Ok(o) => Ok(o),
                Step::Err(kind, ra) => {
                    let e = Error::new(kind, "scripted failure");
                    Err(match ra {
                        Some(d) => e.with_retry_after(d),
                        None => e,
                    })
                }
                Step::Panic => panic!("scripted panic"),
                Step::Slow(d) => {
                    let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                    self.max_active.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(d).await;
                    self.active.fetch_sub(1, Ordering::SeqCst);
                    Ok(Outcome::CaughtUp)
                }
            }
        })
    }
}

/// A module whose resource list tests can change.
pub struct FakeModule {
    pub name: &'static str,
    pub resources: Mutex<Vec<Arc<dyn Resource>>>,
    pub discovers: AtomicUsize,
    pub handlers: Vec<Arc<dyn OutboxHandler>>,
    pub discover_script: Mutex<VecDeque<Step>>,
}

impl FakeModule {
    pub fn new(name: &'static str, resources: Vec<Arc<dyn Resource>>) -> Arc<Self> {
        Self::with_handlers(name, resources, Vec::new())
    }

    pub fn with_handlers(
        name: &'static str,
        resources: Vec<Arc<dyn Resource>>,
        handlers: Vec<Arc<dyn OutboxHandler>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            name,
            resources: Mutex::new(resources),
            discovers: AtomicUsize::new(0),
            handlers,
            discover_script: Mutex::new(VecDeque::new()),
        })
    }

    pub fn set(&self, resources: Vec<Arc<dyn Resource>>) {
        *self.resources.lock().unwrap() = resources;
    }

    pub fn discovers(&self) -> usize {
        self.discovers.load(Ordering::SeqCst)
    }
}

impl Module for FakeModule {
    fn name(&self) -> &'static str {
        self.name
    }

    fn discover<'a>(&'a self, _ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Arc<dyn Resource>>>> {
        Box::pin(async move {
            self.discovers.fetch_add(1, Ordering::SeqCst);
            let step = self.discover_script.lock().unwrap().pop_front();
            match step {
                Some(Step::Err(kind, _)) => return Err(Error::new(kind, "scripted discover failure")),
                Some(Step::Panic) => panic!("scripted discover panic"),
                _ => {}
            }
            Ok(self.resources.lock().unwrap().clone())
        })
    }

    fn handlers(&self) -> Vec<Arc<dyn OutboxHandler>> {
        self.handlers.clone()
    }
}

/// A handler that plays a script of results per call and records payloads.
pub struct FakeHandler {
    pub kind: &'static str,
    pub script: Mutex<VecDeque<Step>>,
    pub applied: Mutex<Vec<Value>>,
    pub reverted: Mutex<Vec<Value>>,
    pub result: Value,
}

impl FakeHandler {
    pub fn new(kind: &'static str) -> Arc<Self> {
        Self::returning(kind, Value::Null)
    }

    pub fn returning(kind: &'static str, result: Value) -> Arc<Self> {
        Arc::new(Self {
            kind,
            script: Mutex::new(VecDeque::new()),
            applied: Mutex::new(Vec::new()),
            reverted: Mutex::new(Vec::new()),
            result,
        })
    }

    pub fn script(self: &Arc<Self>, steps: impl IntoIterator<Item = Step>) -> Arc<Self> {
        self.script.lock().unwrap().extend(steps);
        Arc::clone(self)
    }

    pub fn applied(&self) -> Vec<Value> {
        self.applied.lock().unwrap().clone()
    }

    pub fn reverted(&self) -> Vec<Value> {
        self.reverted.lock().unwrap().clone()
    }
}

impl OutboxHandler for FakeHandler {
    fn kind(&self) -> &'static str {
        self.kind
    }

    fn apply<'a>(&'a self, _ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.applied.lock().unwrap().push(payload);
            let step = self.script.lock().unwrap().pop_front();
            match step {
                Some(Step::Err(kind, ra)) => {
                    let e = Error::new(kind, "scripted failure");
                    Err(match ra {
                        Some(d) => e.with_retry_after(d),
                        None => e,
                    })
                }
                Some(Step::Panic) => panic!("scripted handler panic"),
                _ => Ok(self.result.clone()),
            }
        })
    }

    fn revert<'a>(&'a self, _ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.reverted.lock().unwrap().push(payload);
            Ok(())
        })
    }
}

/// Lets virtual time pass in small steps until `cond` holds (or `max` elapses).
pub async fn wait_for(max: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + max;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    cond()
}

/// Lets virtual time pass.
pub async fn advance(d: Duration) {
    tokio::time::sleep(d).await;
}
