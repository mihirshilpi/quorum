//! Presence: the signed-in user's own presence, batched presence of watched people, and the
//! outbox handlers that set a preferred presence and a status message.

use std::time::Duration;

use futures::future::BoxFuture;
use jiff::Timestamp;
use quorum_model::{Activity, Availability, GraphEnum, Presence, UserId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::watch::PresenceWatch;
use super::{ME_ID_SETTING, ME_SETTING};
use crate::{Cadence, Ctx, Error, ErrorKind, OutboxHandler, Outcome, Resource, Result};

/// Outbox kind: set the preferred presence.
pub const KIND_SET_PREFERRED: &str = "presence.set_preferred";
/// Outbox kind: clear the preferred presence (back to automatic).
pub const KIND_CLEAR_PREFERRED: &str = "presence.clear_preferred";
/// Outbox kind: set (or clear, with an empty message) the status message.
pub const KIND_STATUS_MESSAGE: &str = "presence.set_status_message";
/// Outbox ordering key for every write to the user's own presence.
pub const OWN_PRESENCE_RESOURCE: &str = "presence:me";
/// Longest status message (Teams' limit).
pub const MAX_STATUS_MESSAGE_CHARS: usize = quorum_graph::api::presence::MAX_STATUS_MESSAGE_CHARS;
/// Longest preferred-presence duration accepted, in minutes (7 days).
pub const MAX_DURATION_MINUTES: u64 = 7 * 24 * 60;

/// What the user picked in the status menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceChoice {
    /// A preferred availability/activity pair for `setUserPreferredPresence`.
    Set {
        /// Graph availability.
        availability: Availability,
        /// Graph activity.
        activity: Activity,
    },
    /// Reset to automatic (`clearUserPreferredPresence`).
    Reset,
}

impl PresenceChoice {
    /// Parses a menu choice: `available`, `busy`, `dnd`, `brb`, `away`, `offline`, `reset`
    /// (also the long and Graph spellings: `do_not_disturb`, `BeRightBack`, `appear_offline`).
    /// Returns `None` for anything else.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        if s.len() > 64 {
            return None;
        }
        let norm: String = s.chars().filter(|c| !matches!(c, '_' | '-' | ' ')).flat_map(char::to_lowercase).collect();
        let set = |availability, activity| Some(Self::Set { availability, activity });
        match norm.as_str() {
            "available" => set(Availability::Available, Activity::Available),
            "busy" => set(Availability::Busy, Activity::Busy),
            "dnd" | "donotdisturb" => set(Availability::DoNotDisturb, Activity::DoNotDisturb),
            "brb" | "berightback" => set(Availability::BeRightBack, Activity::BeRightBack),
            "away" | "appearaway" => set(Availability::Away, Activity::Away),
            "offline" | "appearoffline" => set(Availability::Offline, Activity::OffWork),
            "reset" | "automatic" | "auto" => Some(Self::Reset),
            _ => None,
        }
    }

    /// The outbox kind and payload for this choice with an optional duration.
    #[must_use]
    pub fn outbox_payload(&self, duration: Option<Duration>) -> (&'static str, Value) {
        match self {
            Self::Set { availability, activity } => (
                KIND_SET_PREFERRED,
                json!({
                    "availability": availability.as_graph(),
                    "activity": activity.as_graph(),
                    "expirationSecs": duration.map(|d| d.as_secs()),
                }),
            ),
            Self::Reset => (KIND_CLEAR_PREFERRED, json!({})),
        }
    }
}

/// The short presence id the UI and agents use (`available`, `busy`, `dnd`, `brb`, `away`,
/// `offline`, `unknown`).
#[must_use]
pub fn short_id(p: &Availability) -> &'static str {
    match p {
        Availability::Available | Availability::AvailableIdle => "available",
        Availability::Busy | Availability::BusyIdle => "busy",
        Availability::DoNotDisturb => "dnd",
        Availability::BeRightBack => "brb",
        Availability::Away => "away",
        Availability::Offline => "offline",
        _ => "unknown",
    }
}

/// Validates a status message: at most 280 characters, no control characters but newlines.
///
/// # Errors
/// [`ErrorKind::Invalid`] when it is too long or has control characters.
pub fn validate_status_message(message: &str) -> Result<()> {
    if message.chars().count() > MAX_STATUS_MESSAGE_CHARS {
        return Err(Error::new(ErrorKind::Invalid, "status message is longer than 280 characters"));
    }
    if message.chars().any(|c| c.is_control() && c != '\n') {
        return Err(Error::new(ErrorKind::Invalid, "status message has control characters"));
    }
    Ok(())
}

/// The user's own id, once known (from `/me`).
///
/// # Errors
/// [`ErrorKind::Store`] if the store fails.
pub async fn my_id(store: &quorum_store::Store) -> Result<Option<UserId>> {
    let id: Option<String> = store.setting(ME_ID_SETTING.to_owned()).await?;
    Ok(id.and_then(|s| UserId::new(s).ok()))
}

/// The user's own presence: `GET /me/presence` (and `/me` once, to learn the user id).
#[derive(Debug, Default)]
pub struct MyPresence;

impl Resource for MyPresence {
    fn key(&self) -> String {
        "presence.me".into()
    }

    fn area(&self) -> &'static str {
        "people"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(30, 120, 600)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let mut me = my_id(&ctx.store).await?;
            if me.is_none() {
                // Learn who we are once; presence still works if this fails.
                if let Ok(person) = ctx.graph.me().await
                    && let Some(id) = person.id.clone()
                {
                    let id_str = id.as_str().to_owned();
                    ctx.store
                        .write(move |tx| {
                            tx.upsert_people(std::slice::from_ref(&person))?;
                            tx.set_setting(ME_ID_SETTING, &id_str)?;
                            tx.set_setting(ME_SETTING, &person)
                        })
                        .await?;
                    me = Some(id);
                }
            }
            let mut presence = ctx.graph.my_presence().await?;
            if let Some(id) = me {
                presence.user_id = Some(id);
            }
            // A presence or status-message write still in the outbox wins over what the
            // server said before it landed (no flicker back to the old state).
            if presence.user_id.is_some() && !own_presence_write_pending(ctx).await? {
                ctx.store.upsert_presences(vec![presence], Timestamp::now()).await?;
            }
            Ok(Outcome::CaughtUp)
        })
    }
}

/// Whether a write to the user's own presence (preferred presence, status message) is
/// still queued or being sent.
async fn own_presence_write_pending(ctx: &Ctx) -> Result<bool> {
    let entries = ctx.store.read(|r| r.outbox_entries(quorum_store::MAX_PAGE)).await?;
    Ok(entries.items.iter().any(|e| {
        e.state != quorum_store::OutboxState::Failed
            && [KIND_SET_PREFERRED, KIND_CLEAR_PREFERRED, KIND_STATUS_MESSAGE].contains(&e.kind.as_str())
    }))
}

/// Presence of everyone the UI shows: `getPresencesByUserId` over the watched set.
#[derive(Debug, Clone)]
pub struct WatchedPresence {
    watch: PresenceWatch,
}

impl WatchedPresence {
    /// A resource polling `watch`.
    #[must_use]
    pub fn new(watch: PresenceWatch) -> Self {
        Self { watch }
    }
}

impl Resource for WatchedPresence {
    fn key(&self) -> String {
        "presence.watch".into()
    }

    fn area(&self) -> &'static str {
        "people"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(30, 120, 600)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let ids = self.watch.active();
            if ids.is_empty() {
                return Ok(Outcome::CaughtUp);
            }
            let mut presences = ctx.graph.presences_by_user_id(&ids).await?;
            // Never overwrite the user's own optimistic presence while its write is pending.
            if own_presence_write_pending(ctx).await?
                && let Some(me) = my_id(&ctx.store).await?
            {
                presences.retain(|p| p.user_id.as_ref() != Some(&me));
            }
            let fetched = presences.len();
            ctx.store.upsert_presences(presences, Timestamp::now()).await?;
            tracing::debug!(name: "sync.presence_watch", watched = ids.len(), fetched, "presence batch");
            Ok(Outcome::CaughtUp)
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetPreferredPayload {
    availability: String,
    activity: String,
    #[serde(default)]
    expiration_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusPayload {
    #[serde(default)]
    message: String,
    #[serde(default)]
    expiry: Option<Timestamp>,
}

fn permanent(msg: &'static str) -> Error {
    Error::new(ErrorKind::Permanent, msg)
}

/// Replays `presence.set_preferred`.
#[derive(Debug, Default)]
pub struct SetPreferredHandler;

impl OutboxHandler for SetPreferredHandler {
    fn kind(&self) -> &'static str {
        KIND_SET_PREFERRED
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: SetPreferredPayload =
                serde_json::from_value(payload).map_err(|_| permanent("malformed presence payload"))?;
            let availability = Availability::from_graph(&p.availability);
            let activity = Activity::from_graph(&p.activity);
            let expiration = p.expiration_secs.map(Duration::from_secs);
            // Validate locally so a bad entry fails permanently instead of retrying.
            quorum_graph::api::presence::set_preferred_presence_request(&availability, &activity, expiration)
                .map_err(|_| permanent("unsupported presence"))?;
            ctx.graph.set_preferred_presence(&availability, &activity, expiration).await?;
            Ok(Value::Null)
        })
    }
}

/// Replays `presence.clear_preferred`.
#[derive(Debug, Default)]
pub struct ClearPreferredHandler;

impl OutboxHandler for ClearPreferredHandler {
    fn kind(&self) -> &'static str {
        KIND_CLEAR_PREFERRED
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, _payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            ctx.graph.clear_preferred_presence().await?;
            Ok(Value::Null)
        })
    }
}

/// Replays `presence.set_status_message`.
#[derive(Debug, Default)]
pub struct StatusMessageHandler;

impl OutboxHandler for StatusMessageHandler {
    fn kind(&self) -> &'static str {
        KIND_STATUS_MESSAGE
    }

    fn apply<'a>(&'a self, ctx: &'a Ctx, payload: Value) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let p: StatusPayload =
                serde_json::from_value(payload).map_err(|_| permanent("malformed status message payload"))?;
            validate_status_message(&p.message).map_err(|_| permanent("invalid status message"))?;
            ctx.graph.set_status_message(&p.message, p.expiry).await?;
            Ok(Value::Null)
        })
    }
}

/// Applies a preferred presence to a cached presence (the optimistic local change).
#[must_use]
pub fn with_choice(mut current: Presence, choice: &PresenceChoice) -> Presence {
    if let PresenceChoice::Set { availability, activity } = choice {
        current.availability = availability.clone();
        current.activity = activity.clone();
    }
    current
}
