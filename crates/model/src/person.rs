//! People (directory users and contacts).

use serde::{Deserialize, Serialize};

use crate::address::MailAddress;
use crate::id::UserId;
use crate::sensitive::Sensitive;

/// A person as shown on people cards and in pickers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Person {
    /// The user id, for directory users.
    #[serde(default)]
    pub id: Option<UserId>,
    /// The display name.
    #[serde(default)]
    pub display_name: Sensitive,
    /// The primary email address.
    #[serde(default)]
    pub email: Option<MailAddress>,
    /// Job title.
    #[serde(default)]
    pub job_title: Option<Sensitive>,
    /// Department.
    #[serde(default)]
    pub department: Option<Sensitive>,
    /// Office location.
    #[serde(default)]
    pub office_location: Option<Sensitive>,
    /// A phone number.
    #[serde(default)]
    pub phone: Option<Sensitive>,
}
