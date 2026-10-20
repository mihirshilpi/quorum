//! Teams presence: read the user's and colleagues' presence, hold an application presence session
//! (keep-alive), set a preferred presence and a status message.
//!
//! Graph's `statusMessage.message.content` maps to [`Presence::status_message`] and
//! `statusMessage.expiryDateTime` (a `dateTimeTimeZone`, resolved to an instant) to
//! [`Presence::expiry`]; the presence `id` is the user id.

use std::time::Duration;

use jiff::Timestamp;
use quorum_model::{Activity, Availability, DateTimeZone, GraphEnum, Presence, Sensitive, UserId};
use serde::Deserialize;
use serde_json::{Value, json};

use super::chat::{cap, id_of, lenient, opaque};
use crate::batch::BatchItem;
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::request::{Request, ResourceFamily};

/// Concurrency bucket for presence requests.
pub const PRESENCE_BUCKET: &str = "presence";
/// Most user ids Graph accepts in one `getPresencesByUserId` call.
pub const MAX_PRESENCE_IDS: usize = 650;
/// Shortest `setPresence` session expiry.
pub const MIN_SESSION_EXPIRY: Duration = Duration::from_secs(5 * 60);
/// Longest `setPresence` session expiry.
pub const MAX_SESSION_EXPIRY: Duration = Duration::from_secs(4 * 60 * 60);
/// Longest preferred-presence expiry we send (Graph's own defaults go up to 7 days).
pub const MAX_PREFERRED_EXPIRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Longest status message Teams shows.
pub const MAX_STATUS_MESSAGE_CHARS: usize = 280;

fn presence_req(req: Request) -> Request {
    req.family(ResourceFamily::Teams).bucket(PRESENCE_BUCKET)
}

fn config(detail: &'static str) -> Error {
    Error::new(ErrorKind::Config, detail)
}

/// Formats a duration as ISO 8601 (`PT1H5M`), whole seconds.
#[must_use]
pub fn iso8601_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let mut out = String::from("PT");
    if h > 0 {
        out.push_str(&format!("{h}H"));
    }
    if m > 0 {
        out.push_str(&format!("{m}M"));
    }
    if s > 0 || (h == 0 && m == 0) {
        out.push_str(&format!("{s}S"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Mapping.
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireItemBody {
    #[serde(default, deserialize_with = "lenient")]
    content: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireStatusMessage {
    #[serde(default, deserialize_with = "lenient")]
    message: Option<WireItemBody>,
    #[serde(default, deserialize_with = "lenient")]
    expiry_date_time: Option<DateTimeZone>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePresence {
    #[serde(default, deserialize_with = "lenient")]
    id: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    availability: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    activity: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    status_message: Option<WireStatusMessage>,
}

/// Maps a Graph `presence` to a [`Presence`] (`None` if it is not an object).
#[must_use]
pub fn parse_presence(value: Value) -> Option<Presence> {
    if !value.is_object() {
        return None;
    }
    let w: WirePresence = serde_json::from_value(value).ok()?;
    let (status_message, expiry) = match w.status_message {
        Some(sm) => {
            let text = sm.message.and_then(|m| m.content).filter(|c| !c.trim().is_empty());
            // An expiry that cannot be resolved (e.g. Graph's year-9999 "never") means none.
            let expiry = sm.expiry_date_time.and_then(|e| e.to_timestamp().ok());
            (text.map(|t| Sensitive::new(cap(t, 4096))), expiry)
        }
        None => (None, None),
    };
    Some(Presence {
        user_id: id_of(w.id),
        availability: w.availability.as_deref().map(Availability::from_graph).unwrap_or_default(),
        activity: w.activity.as_deref().map(Activity::from_graph).unwrap_or_default(),
        status_message,
        expiry,
    })
}

// ---------------------------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------------------------

/// `GET /me/presence`.
#[must_use]
pub fn my_presence_request() -> Request {
    presence_req(Request::get("/me/presence"))
}

/// `GET /users/{id}/presence`.
#[must_use]
pub fn user_presence_request(user: &UserId) -> Request {
    presence_req(Request::get(format!("/users/{}/presence", super::chat::seg(user.as_str()))))
}

/// `POST /communications/getPresencesByUserId`, one request per [`MAX_PRESENCE_IDS`] distinct
/// ids (duplicates dropped, order kept). The requests are read-only, so they are marked
/// retryable.
#[must_use]
pub fn presences_by_user_id_requests(users: &[UserId]) -> Vec<Request> {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<&str> = users.iter().map(UserId::as_str).filter(|id| seen.insert(*id)).collect();
    unique
        .chunks(MAX_PRESENCE_IDS)
        .map(|chunk| {
            presence_req(
                Request::post("/communications/getPresencesByUserId")
                    .json_value(json!({"ids": chunk}))
                    .idempotent(true),
            )
        })
        .collect()
}

/// Availability/activity pairs `setPresence` accepts.
const SESSION_PAIRS: &[(Availability, Activity)] = &[
    (Availability::Available, Activity::Available),
    (Availability::Busy, Activity::InACall),
    (Availability::Busy, Activity::InAConferenceCall),
    (Availability::Away, Activity::Away),
    (Availability::DoNotDisturb, Activity::Presenting),
];

/// Availability/activity pairs `setUserPreferredPresence` accepts.
const PREFERRED_PAIRS: &[(Availability, Activity)] = &[
    (Availability::Available, Activity::Available),
    (Availability::Busy, Activity::Busy),
    (Availability::DoNotDisturb, Activity::DoNotDisturb),
    (Availability::BeRightBack, Activity::BeRightBack),
    (Availability::Away, Activity::Away),
    (Availability::Offline, Activity::OffWork),
];

fn session_id(id: &str) -> Result<&str> {
    let ok = id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if ok { Ok(id) } else { Err(config("presence sessionId must be the app's client id (a GUID)")) }
}

/// `POST /me/presence/setPresence`: the application presence session (keep-alive).
/// `client_id` is Quorum's application id; `expiration` must be 5 minutes to 4 hours.
///
/// # Errors
/// [`ErrorKind::Config`] for a non-GUID client id, an unsupported availability/activity pair or an
/// out-of-range expiration.
pub fn set_presence_request(
    client_id: &str,
    availability: &Availability,
    activity: &Activity,
    expiration: Duration,
) -> Result<Request> {
    let sid = session_id(client_id)?;
    if !SESSION_PAIRS.iter().any(|(a, b)| a == availability && b == activity) {
        return Err(config("unsupported availability/activity pair for setPresence"));
    }
    if !(MIN_SESSION_EXPIRY..=MAX_SESSION_EXPIRY).contains(&expiration) {
        return Err(config("setPresence expiration must be PT5M to PT4H"));
    }
    let body = json!({
        "sessionId": sid,
        "availability": availability.as_graph(),
        "activity": activity.as_graph(),
        "expirationDuration": iso8601_duration(expiration),
    });
    Ok(presence_req(Request::post("/me/presence/setPresence").json_value(body).idempotent(true)))
}

/// `POST /me/presence/clearPresence` for our session.
///
/// # Errors
/// [`ErrorKind::Config`] for a non-GUID client id.
pub fn clear_presence_request(client_id: &str) -> Result<Request> {
    let sid = session_id(client_id)?;
    Ok(presence_req(Request::post("/me/presence/clearPresence").json_value(json!({"sessionId": sid})).idempotent(true)))
}

/// `POST /me/presence/setUserPreferredPresence`; without `expiration` Graph's default applies.
///
/// # Errors
/// [`ErrorKind::Config`] for an unsupported pair or an expiration outside 1 minute to 7 days.
pub fn set_preferred_presence_request(
    availability: &Availability,
    activity: &Activity,
    expiration: Option<Duration>,
) -> Result<Request> {
    if !PREFERRED_PAIRS.iter().any(|(a, b)| a == availability && b == activity) {
        return Err(config("unsupported availability/activity pair for setUserPreferredPresence"));
    }
    let mut body = json!({"availability": availability.as_graph(), "activity": activity.as_graph()});
    if let Some(e) = expiration {
        if e < Duration::from_secs(60) || e > MAX_PREFERRED_EXPIRY {
            return Err(config("preferred presence expiration must be 1 minute to 7 days"));
        }
        if let Some(obj) = body.as_object_mut() {
            obj.insert("expirationDuration".into(), Value::String(iso8601_duration(e)));
        }
    }
    Ok(presence_req(Request::post("/me/presence/setUserPreferredPresence").json_value(body).idempotent(true)))
}

/// `POST /me/presence/clearUserPreferredPresence`.
#[must_use]
pub fn clear_preferred_presence_request() -> Request {
    presence_req(Request::post("/me/presence/clearUserPreferredPresence").json_value(json!({})).idempotent(true))
}

/// `POST /me/presence/setStatusMessage`; an empty `message` clears it. `expiry` (UTC) is when
/// Teams drops it.
///
/// # Errors
/// [`ErrorKind::Config`] for a message over 280 characters or with control characters (other than
/// newlines).
pub fn set_status_message_request(message: &str, expiry: Option<Timestamp>) -> Result<Request> {
    if message.chars().count() > MAX_STATUS_MESSAGE_CHARS {
        return Err(config("status message is longer than 280 characters"));
    }
    if message.chars().any(|c| c.is_control() && c != '\n') {
        return Err(config("status message has control characters"));
    }
    let mut status = json!({"message": {"content": message, "contentType": "text"}});
    if let (Some(e), Some(obj)) = (expiry, status.as_object_mut()) {
        let dtz = DateTimeZone::from_timestamp_utc(e);
        obj.insert("expiryDateTime".into(), json!({"dateTime": dtz.date_time.to_string(), "timeZone": "UTC"}));
    }
    Ok(presence_req(
        Request::post("/me/presence/setStatusMessage").json_value(json!({"statusMessage": status})).idempotent(true),
    ))
}

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

fn presences_of(v: &Value) -> Vec<Presence> {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| a.iter().take(MAX_PRESENCE_IDS).cloned().filter_map(parse_presence).collect())
        .unwrap_or_default()
}

impl Client {
    /// The signed-in user's presence.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn my_presence(&self) -> Result<Presence> {
        self.send_mapped(my_presence_request(), "presence", parse_presence).await
    }

    /// One user's presence.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn user_presence(&self, user: &UserId) -> Result<Presence> {
        self.send_mapped(user_presence_request(user), "presence", parse_presence).await
    }

    /// Presence of many users: ids are de-duplicated and chunked by [`MAX_PRESENCE_IDS`]; one
    /// chunk is a direct call, several go out together through `$batch`.
    ///
    /// # Errors
    /// The first failing chunk's error; as [`Client::send_json`] / [`Client::batch`].
    pub async fn presences_by_user_id(&self, users: &[UserId]) -> Result<Vec<Presence>> {
        let mut requests = presences_by_user_id_requests(users);
        if requests.len() <= 1 {
            let Some(req) = requests.pop() else { return Ok(Vec::new()) };
            let v: Value = self.send_json(req).await?;
            return Ok(presences_of(&v));
        }
        let items = requests.into_iter().enumerate().map(|(i, r)| BatchItem::new(format!("p{i}"), r)).collect();
        let mut out = Vec::new();
        for resp in self.batch(items).await? {
            let v: Value = resp.json()?;
            out.extend(presences_of(&v));
        }
        Ok(out)
    }

    /// Sets (or refreshes) the application presence session; call again before it expires.
    ///
    /// # Errors
    /// As [`set_presence_request`] and [`Client::send_raw`].
    pub async fn set_presence(
        &self,
        client_id: &str,
        availability: &Availability,
        activity: &Activity,
        expiration: Duration,
    ) -> Result<()> {
        self.send_unit(set_presence_request(client_id, availability, activity, expiration)?).await
    }

    /// Ends the application presence session (on sleep or quit).
    ///
    /// # Errors
    /// As [`clear_presence_request`] and [`Client::send_raw`].
    pub async fn clear_presence(&self, client_id: &str) -> Result<()> {
        self.send_unit(clear_presence_request(client_id)?).await
    }

    /// Sets the user's preferred presence (what they picked in the status menu).
    ///
    /// # Errors
    /// As [`set_preferred_presence_request`] and [`Client::send_raw`].
    pub async fn set_preferred_presence(
        &self,
        availability: &Availability,
        activity: &Activity,
        expiration: Option<Duration>,
    ) -> Result<()> {
        self.send_unit(set_preferred_presence_request(availability, activity, expiration)?).await
    }

    /// Clears the user's preferred presence.
    ///
    /// # Errors
    /// As [`Client::send_raw`].
    pub async fn clear_preferred_presence(&self) -> Result<()> {
        self.send_unit(clear_preferred_presence_request()).await
    }

    /// Sets or clears the status message.
    ///
    /// # Errors
    /// As [`set_status_message_request`] and [`Client::send_raw`].
    pub async fn set_status_message(&self, message: &str, expiry: Option<Timestamp>) -> Result<()> {
        self.send_unit(set_status_message_request(message, expiry)?).await
    }
}

/// Validates an opaque presence-related id (exposed for callers building their own requests).
///
/// # Errors
/// [`ErrorKind::Config`] for an empty, oversized or control-character id.
pub fn validate_session_id(id: &str) -> Result<()> {
    opaque(id, "session id")?;
    session_id(id).map(|_| ())
}
