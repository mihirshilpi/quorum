//! Email addresses.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, ErrorKind};
use crate::sensitive::Sensitive;

/// The maximum length of an email address in bytes (RFC 5321 path limit, rounded up).
pub const MAX_ADDRESS_LEN: usize = 320;

/// A validated SMTP address such as `someone@example.com`.
///
/// Validation is deliberately loose (real mail is messy) but rejects anything that cannot be an
/// address: it must contain an `@` with a non-empty local part and domain, be at most
/// [`MAX_ADDRESS_LEN`] bytes, and contain no whitespace, control characters or `<`, `>`.
/// Surrounding whitespace is trimmed. `Debug` prints only the length (M-PUBLIC-DEBUG).
///
/// ```
/// use quorum_model::MailAddress;
/// let a = MailAddress::new("someone@example.com")?;
/// assert_eq!(a.domain(), "example.com");
/// assert!(MailAddress::new("not an address").is_err());
/// # Ok::<(), quorum_model::Error>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MailAddress(String);

impl MailAddress {
    /// Validates and wraps an address.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] of kind [`ErrorKind::Empty`], [`ErrorKind::TooLong`] or
    /// [`ErrorKind::InvalidAddress`] if the value cannot be an address.
    pub fn new(value: impl Into<String>) -> Result<Self, Error> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(Error::new(ErrorKind::Empty, "MailAddress"));
        }
        if trimmed.len() > MAX_ADDRESS_LEN {
            return Err(Error::new(ErrorKind::TooLong { max: MAX_ADDRESS_LEN }, "MailAddress"));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(Error::new(ErrorKind::ControlCharacter, "MailAddress"));
        }
        let bad_char = |c: char| c.is_whitespace() || c == '<' || c == '>';
        let valid_shape = match trimmed.rsplit_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
            }
            None => false,
        };
        if !valid_shape || trimmed.chars().any(bad_char) {
            return Err(Error::new(ErrorKind::InvalidAddress, "MailAddress"));
        }
        if trimmed.len() == value.len() { Ok(Self(value)) } else { Ok(Self(trimmed.to_owned())) }
    }

    /// Returns the address.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the part before the last `@`.
    #[must_use]
    pub fn local_part(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(local, _)| local)
    }

    /// Returns the part after the last `@`.
    #[must_use]
    pub fn domain(&self) -> &str {
        self.0.rsplit_once('@').map_or("", |(_, domain)| domain)
    }

    /// Compares two addresses ignoring ASCII case (how Exchange matches them).
    #[must_use]
    pub fn eq_ignore_case(&self, other: &Self) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

impl fmt::Debug for MailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MailAddress({} bytes)", self.0.len())
    }
}

impl AsRef<str> for MailAddress {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for MailAddress {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::new(s)
    }
}

impl TryFrom<String> for MailAddress {
    type Error = Error;
    fn try_from(value: String) -> Result<Self, Error> {
        Self::new(value)
    }
}

impl Serialize for MailAddress {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MailAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = String::deserialize(d)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A display name and address pair, as in Graph's `emailAddress` resource.
///
/// Graph sometimes returns senders without a usable SMTP address (an empty string or a legacy
/// Exchange X.500 path). Deserialising is therefore lenient: an invalid `address` becomes
/// `None` instead of failing the whole message. `Debug` redacts both fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct EmailAddress {
    /// The display name, e.g. `"Alex Wilber"`. May be empty.
    pub name: Sensitive,
    /// The SMTP address, if Graph supplied a valid one.
    pub address: Option<MailAddress>,
}

impl EmailAddress {
    /// Creates an address with a display name.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] if `address` is not a valid [`MailAddress`].
    pub fn new(name: impl Into<String>, address: impl Into<String>) -> Result<Self, Error> {
        Ok(Self { name: Sensitive::new(name), address: Some(MailAddress::new(address)?) })
    }

    /// Returns the display name, falling back to the address, then to the empty string.
    #[must_use]
    pub fn display(&self) -> &str {
        if self.name.is_empty() { self.address.as_ref().map_or("", MailAddress::as_str) } else { self.name.as_str() }
    }
}

impl<'de> Deserialize<'de> for EmailAddress {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            name: Sensitive,
            #[serde(default)]
            address: Option<String>,
        }
        let raw = Raw::deserialize(d)?;
        Ok(Self { name: raw.name, address: raw.address.and_then(|a| MailAddress::new(a).ok()) })
    }
}
