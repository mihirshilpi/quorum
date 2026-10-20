//! Request values, built without I/O.

use std::fmt;

use serde::Serialize;

use crate::error::Result;

/// HTTP method of a Graph request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
}

impl Method {
    /// The method name as sent on the wire (`"GET"`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
        }
    }

    /// Parses a method name, case-insensitively. Returns `None` for unknown methods.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let m = match s.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            "HEAD" => Self::Head,
            _ => return None,
        };
        Some(m)
    }

    /// Whether repeating the method has the same effect as sending it once (RFC 9110 §9.2.2).
    #[must_use]
    pub fn is_idempotent(self) -> bool {
        matches!(self, Self::Get | Self::Head | Self::Put | Self::Delete)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which Graph service a request hits; each family has its own rate limiter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourceFamily {
    /// Outlook mail (`/messages`, `/mailFolders`, `/sendMail`).
    Mail,
    /// Outlook calendar (`/events`, `/calendarView`, `/calendars`, free/busy).
    Calendar,
    /// Teams chats, channels and presence.
    Teams,
    /// OneDrive and SharePoint (`/drive`, `/drives`, `/sites`).
    Files,
    /// Everything else (`/me`, `/users`, `/$batch`, ...).
    Other,
}

impl ResourceFamily {
    /// All families, in a fixed order.
    pub const ALL: [Self; 5] = [Self::Mail, Self::Calendar, Self::Teams, Self::Files, Self::Other];

    /// Stable index into per-family tables (position in [`ResourceFamily::ALL`]).
    #[must_use]
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Mail => 0,
            Self::Calendar => 1,
            Self::Teams => 2,
            Self::Files => 3,
            Self::Other => 4,
        }
    }

    /// Short stable name for logs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mail => "mail",
            Self::Calendar => "calendar",
            Self::Teams => "teams",
            Self::Files => "files",
            Self::Other => "other",
        }
    }

    /// Guesses the family from a request path or URL (segments are matched case-insensitively).
    ///
    /// Teams segments win over mail ones because `/chats/{id}/messages` is Teams, not mail.
    #[must_use]
    pub fn infer(path: &str) -> Self {
        let path = path.split(['?', '#']).next().unwrap_or("");
        let segments: Vec<String> = path.split('/').map(str::to_ascii_lowercase).collect();
        let has = |names: &[&str]| segments.iter().any(|s| names.contains(&s.as_str()));
        if has(&["chats", "teams", "channels", "presence", "presences", "communications", "joinedteams"]) {
            Self::Teams
        } else if has(&["drive", "drives", "sites", "items", "root"]) {
            Self::Files
        } else if has(&[
            "events",
            "calendar",
            "calendars",
            "calendarview",
            "calendargroups",
            "getschedule",
            "findmeetingtimes",
        ]) {
            Self::Calendar
        } else if has(&["messages", "mailfolders", "sendmail", "mailboxsettings", "inferenceclassification"]) {
            Self::Mail
        } else {
            Self::Other
        }
    }
}

/// Where a request goes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// Relative to the client's base URL, starting with `/` (e.g. `/me/messages`).
    Path(String),
    /// An absolute `@odata.nextLink` / `@odata.deltaLink`; must be on the configured Graph host.
    Link(String),
}

/// A request body.
#[derive(Clone, PartialEq)]
pub enum Body {
    /// No body.
    Empty,
    /// A JSON document, sent as `application/json`.
    Json(serde_json::Value),
    /// Raw bytes with an explicit content type.
    Bytes {
        /// MIME type sent as `Content-Type`.
        content_type: String,
        /// The bytes.
        data: Vec<u8>,
    },
}

impl fmt::Debug for Body {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("Empty"),
            Self::Json(_) => f.write_str("Json(<redacted>)"),
            Self::Bytes { content_type, data } => {
                write!(f, "Bytes {{ content_type: {content_type:?}, len: {} }}", data.len())
            }
        }
    }
}

/// A Graph request as a plain value: method, target, query, headers, body and retry policy.
///
/// Built without I/O and executed by [`Client`](crate::Client). `Debug` shows only the method,
/// family and header names: paths, query values and bodies may hold addresses or ids.
///
/// ```
/// use quorum_graph::Request;
/// let req = Request::get("/me/messages")
///     .query("$top", "50")
///     .query("$select", "id,subject")
///     .prefer("outlook.body-content-type=\"text\"");
/// assert!(req.is_idempotent());
/// ```
#[derive(Clone)]
pub struct Request {
    pub(crate) method: Method,
    pub(crate) target: Target,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Body,
    pub(crate) idempotent: Option<bool>,
    pub(crate) family: Option<ResourceFamily>,
    pub(crate) bucket: Option<String>,
}

impl Request {
    /// A request to `path`, relative to the client's base URL (e.g. `/me/messages`).
    #[must_use]
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self::with_target(method, Target::Path(path.into()))
    }

    /// A `GET` of an absolute `@odata.nextLink` or `@odata.deltaLink` returned by Graph.
    ///
    /// The link must be on the client's Graph host and version; anything else is rejected with
    /// [`ErrorKind::Malformed`](crate::ErrorKind::Malformed) before any I/O (SSRF guard).
    #[must_use]
    pub fn link(link: impl Into<String>) -> Self {
        Self::with_target(Method::Get, Target::Link(link.into()))
    }

    fn with_target(method: Method, target: Target) -> Self {
        Self {
            method,
            target,
            query: Vec::new(),
            headers: Vec::new(),
            body: Body::Empty,
            idempotent: None,
            family: None,
            bucket: None,
        }
    }

    /// `GET path`.
    #[must_use]
    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::Get, path)
    }

    /// `POST path`.
    #[must_use]
    pub fn post(path: impl Into<String>) -> Self {
        Self::new(Method::Post, path)
    }

    /// `PUT path`.
    #[must_use]
    pub fn put(path: impl Into<String>) -> Self {
        Self::new(Method::Put, path)
    }

    /// `PATCH path`.
    #[must_use]
    pub fn patch(path: impl Into<String>) -> Self {
        Self::new(Method::Patch, path)
    }

    /// `DELETE path`.
    #[must_use]
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new(Method::Delete, path)
    }

    /// Appends a query parameter (values are percent-encoded when sent).
    #[must_use]
    pub fn query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Adds a request header. `Authorization`, `Host`, `Content-Length` and `client-request-id`
    /// are set by the client and rejected here at send time with `ErrorKind::Config`.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds a `Prefer` header (e.g. `odata.maxpagesize=50`, `outlook.timezone="UTC"`).
    #[must_use]
    pub fn prefer(self, value: impl Into<String>) -> Self {
        self.header("Prefer", value)
    }

    /// Adds `ConsistencyLevel: eventual`, needed for advanced directory queries (`$count`, `$search`).
    #[must_use]
    pub fn consistency_eventual(self) -> Self {
        self.header("ConsistencyLevel", "eventual")
    }

    /// Sets a JSON body.
    #[must_use]
    pub fn json_value(mut self, body: serde_json::Value) -> Self {
        self.body = Body::Json(body);
        self
    }

    /// Serialises `body` as the JSON body.
    ///
    /// # Errors
    /// [`ErrorKind::Config`](crate::ErrorKind::Config) if `body` cannot be serialised.
    pub fn json<T: Serialize + ?Sized>(self, body: &T) -> Result<Self> {
        let value = serde_json::to_value(body).map_err(|e| {
            crate::Error::new(crate::ErrorKind::Config, "request body is not serialisable").with_source(e)
        })?;
        Ok(self.json_value(value))
    }

    /// Sets a raw byte body with its content type.
    #[must_use]
    pub fn bytes(mut self, content_type: impl Into<String>, data: Vec<u8>) -> Self {
        self.body = Body::Bytes { content_type: content_type.into(), data };
        self
    }

    /// Declares whether the request may be retried after a failure. Defaults to the method's
    /// idempotency; set `true` on a `POST`/`PATCH` only when it carries an idempotency guard
    /// (e.g. an event `transactionId`).
    #[must_use]
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = Some(idempotent);
        self
    }

    /// Overrides the inferred [`ResourceFamily`] (used for rate limiting).
    #[must_use]
    pub fn family(mut self, family: ResourceFamily) -> Self {
        self.family = Some(family);
        self
    }

    /// Sets the concurrency bucket key, e.g. the mailbox or user id; requests sharing a key share
    /// a concurrency limit. Defaults to `"me"`.
    #[must_use]
    pub fn bucket(mut self, key: impl Into<String>) -> Self {
        self.bucket = Some(key.into());
        self
    }

    /// The HTTP method.
    #[must_use]
    pub fn method(&self) -> Method {
        self.method
    }

    /// The path or absolute link as given.
    #[must_use]
    pub fn target(&self) -> &str {
        match &self.target {
            Target::Path(p) | Target::Link(p) => p,
        }
    }

    /// Whether the target is an absolute link rather than a relative path.
    #[must_use]
    pub fn is_link(&self) -> bool {
        matches!(self.target, Target::Link(_))
    }

    /// The body.
    #[must_use]
    pub fn body(&self) -> &Body {
        &self.body
    }

    /// The headers added to this request.
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Whether the client may retry this request.
    #[must_use]
    pub fn is_idempotent(&self) -> bool {
        self.idempotent.unwrap_or_else(|| self.method.is_idempotent())
    }

    /// The resource family, explicit or inferred from the target.
    #[must_use]
    pub fn resource_family(&self) -> ResourceFamily {
        self.family.unwrap_or_else(|| ResourceFamily::infer(self.target()))
    }

    /// The concurrency bucket key (`"me"` unless set).
    #[must_use]
    pub fn bucket_key(&self) -> &str {
        self.bucket.as_deref().unwrap_or("me")
    }

    /// A `GET` of `link` that keeps this request's headers, family and bucket (used to follow
    /// `nextLink`s so `Prefer` headers carry over).
    #[must_use]
    pub fn follow(&self, link: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            target: Target::Link(link.into()),
            query: Vec::new(),
            headers: self.headers.clone(),
            body: Body::Empty,
            idempotent: None,
            family: Some(self.resource_family()),
            bucket: self.bucket.clone(),
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<&str> = self.headers.iter().map(|(n, _)| n.as_str()).collect();
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("target", &"<redacted>")
            .field("query_params", &self.query.len())
            .field("headers", &header_names)
            .field("body", &self.body)
            .field("idempotent", &self.is_idempotent())
            .field("family", &self.resource_family())
            .finish_non_exhaustive()
    }
}
