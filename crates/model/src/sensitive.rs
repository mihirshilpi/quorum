//! A string holding personal content, with a redacting `Debug`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Personal text such as a subject, body, preview, display name or status message.
///
/// `Debug` prints only the length (M-PUBLIC-DEBUG), so structs holding `Sensitive` fields can
/// derive `Debug` and still be logged safely. There is deliberately no `Display` impl: read the
/// text with [`Sensitive::as_str`] where it is meant to be shown.
///
/// Deserialising accepts `null` as the empty string.
///
/// ```
/// use quorum_model::Sensitive;
/// let s = Sensitive::new("Quarterly results");
/// assert_eq!(s.as_str(), "Quarterly results");
/// assert_eq!(format!("{s:?}"), "Sensitive(17 bytes)");
/// ```
#[derive(Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sensitive(String);

impl Sensitive {
    /// Wraps a string.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// Returns the text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the owned text.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Returns `true` if the text is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for Sensitive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sensitive({} bytes)", self.0.len())
    }
}

impl From<String> for Sensitive {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Sensitive {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl AsRef<str> for Sensitive {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for Sensitive {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for Sensitive {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl Serialize for Sensitive {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sensitive {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(Option::<String>::deserialize(d)?.unwrap_or_default()))
    }
}
