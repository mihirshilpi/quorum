//! OneDrive and SharePoint drives, items, sites and lists.

use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::de::null_default;
use crate::enums::graph_enum;
use crate::id::{DriveId, DriveItemId, ListId, ListItemId, SiteId};
use crate::identity::Identity;
use crate::sensitive::Sensitive;

graph_enum!(
    /// The kind of a drive.
    DriveKind {
        /// A personal OneDrive (consumer).
        Personal = "personal",
        /// OneDrive for work or school.
        Business = "business",
        /// A SharePoint document library.
        DocumentLibrary = "documentLibrary",
    }
);

/// A drive (OneDrive or document library).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    /// The drive id.
    pub id: DriveId,
    /// Personal, business or document library.
    pub drive_type: DriveKind,
    /// The drive name.
    #[serde(default)]
    pub name: Sensitive,
    /// The owner.
    #[serde(default)]
    pub owner: Option<Identity>,
    /// The web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

/// The file facet of a drive item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    /// The MIME type (untrusted).
    #[serde(default)]
    pub mime_type: Option<String>,
}

/// The folder facet of a drive item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderFacet {
    /// Number of children.
    #[serde(default, deserialize_with = "null_default")]
    pub child_count: u32,
}

/// A file or folder in a drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    /// The item id.
    pub id: DriveItemId,
    /// The drive it is in.
    pub drive_id: DriveId,
    /// The parent folder.
    #[serde(default)]
    pub parent_id: Option<DriveItemId>,
    /// The name (untrusted: never use it as a local path without sanitising).
    #[serde(default)]
    pub name: Sensitive,
    /// Size in bytes (for folders, the total of the contents).
    #[serde(default, deserialize_with = "null_default")]
    pub size: u64,
    /// Present for files.
    #[serde(default)]
    pub file: Option<FileFacet>,
    /// Present for folders.
    #[serde(default)]
    pub folder: Option<FolderFacet>,
    /// Who last modified it.
    #[serde(default)]
    pub last_modified_by: Option<Identity>,
    /// When it was last modified.
    #[serde(default)]
    pub last_modified_date_time: Option<Timestamp>,
    /// When it was created.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// The web link.
    #[serde(default)]
    pub web_url: Option<String>,
    /// The entity tag (changes on any change).
    #[serde(default)]
    pub e_tag: Option<String>,
    /// The content tag (changes when the content changes).
    #[serde(default)]
    pub c_tag: Option<String>,
    /// Whether the item is shared.
    #[serde(default, deserialize_with = "null_default")]
    pub shared: bool,
}

impl DriveItem {
    /// Returns `true` if the item is a folder.
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.folder.is_some()
    }

    /// Returns the MIME type, for files.
    #[must_use]
    pub fn mime_type(&self) -> Option<&str> {
        self.file.as_ref().and_then(|f| f.mime_type.as_deref())
    }
}

/// A SharePoint site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    /// The site id.
    pub id: SiteId,
    /// The site title.
    #[serde(default)]
    pub display_name: Sensitive,
    /// The URL segment name.
    #[serde(default)]
    pub name: Option<String>,
    /// The web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

/// A SharePoint list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List {
    /// The list id.
    pub id: ListId,
    /// The site it belongs to.
    #[serde(default)]
    pub site_id: Option<SiteId>,
    /// The list title.
    #[serde(default)]
    pub display_name: Sensitive,
    /// The list template, e.g. `genericList` or `documentLibrary`.
    #[serde(default)]
    pub template: Option<String>,
    /// The web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

/// An item in a SharePoint list.
///
/// `fields` holds the column values as Graph returns them; `Debug` shows only the column count.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListItem {
    /// The item id.
    pub id: ListItemId,
    /// The list it belongs to.
    #[serde(default)]
    pub list_id: Option<ListId>,
    /// Column values.
    #[serde(default, deserialize_with = "null_default")]
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// When it was created.
    #[serde(default)]
    pub created_date_time: Option<Timestamp>,
    /// When it was last modified.
    #[serde(default)]
    pub last_modified_date_time: Option<Timestamp>,
    /// The web link.
    #[serde(default)]
    pub web_url: Option<String>,
}

impl ListItem {
    /// Returns a column value as a string, if it is one.
    #[must_use]
    pub fn field_str(&self, column: &str) -> Option<&str> {
        self.fields.get(column).and_then(serde_json::Value::as_str)
    }
}

impl fmt::Debug for ListItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ListItem")
            .field("id", &self.id)
            .field("list_id", &self.list_id)
            .field("fields", &format_args!("{} columns", self.fields.len()))
            .field("created_date_time", &self.created_date_time)
            .field("last_modified_date_time", &self.last_modified_date_time)
            .finish_non_exhaustive()
    }
}
