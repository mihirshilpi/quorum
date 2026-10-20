//! Who did something: a user, an application or a device.

use serde::{Deserialize, Serialize};

use crate::enums::graph_enum;
use crate::sensitive::Sensitive;

graph_enum!(
    /// The kind of an [`Identity`].
    #[derive(Default)]
    IdentityKind {
        /// A person.
        #[default]
        User = "user",
        /// An application or bot.
        Application = "application",
        /// A device.
        Device = "device",
    }
);

/// A reference to a user, application or device, as in Graph's `identitySet`.
///
/// The id is kept as a plain string because it may be a user, app or device id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    /// Whether this is a user, application or device.
    #[serde(default)]
    pub kind: IdentityKind,
    /// The object id, if known.
    #[serde(default)]
    pub id: Option<String>,
    /// The display name. May be empty.
    #[serde(default)]
    pub display_name: Sensitive,
}
