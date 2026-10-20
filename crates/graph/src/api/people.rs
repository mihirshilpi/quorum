//! People and directory: the signed-in user, photos, user search, org chart (`manager`,
//! `directReports`), relevance-ranked people (`/me/people`), group members and personal
//! contacts.
//!
//! Directory users, `/me/people` entries and contacts all map to [`Person`]:
//! - email: `mail`, else `userPrincipalName` when it is an address (users); the first
//!   `scoredEmailAddresses` entry (people); the first `emailAddresses` entry (contacts);
//! - phone: the first `businessPhones` entry, else `mobilePhone` (users, contacts); the first
//!   `phones` entry (people).

use quorum_model::{EmailAddress, MailAddress, Person, Sensitive, UserId};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::mail::{Mailbox, config, mapped_page, null_default, segment};
use crate::client::Client;
use crate::error::{Error, ErrorKind, Result};
use crate::paging::Page;
use crate::request::{Method, Request, ResourceFamily};

/// Fields of a directory user the store keeps.
pub const USER_SELECT: &str =
    "id,displayName,mail,userPrincipalName,jobTitle,department,officeLocation,businessPhones,mobilePhone";
/// Fields of a `/me/people` entry the store keeps.
pub const PEOPLE_SELECT: &str = "id,displayName,scoredEmailAddresses,jobTitle,department,officeLocation,phones";
/// Fields of a contact the store keeps.
pub const CONTACT_SELECT: &str = "id,displayName,givenName,surname,emailAddresses,jobTitle,department,officeLocation,businessPhones,mobilePhone,companyName";
/// Default page size for people collections.
pub const DEFAULT_PEOPLE_PAGE_SIZE: u32 = 25;
/// Longest search text accepted.
const MAX_QUERY_LEN: usize = 256;

fn non_empty(s: Option<Sensitive>) -> Option<Sensitive> {
    s.filter(|s| !s.as_str().trim().is_empty())
}

fn first_phone(list: Vec<Option<String>>) -> Option<Sensitive> {
    list.into_iter().flatten().find(|p| !p.trim().is_empty()).map(Sensitive::new)
}

/// Maps a directory `user` (or a `directoryObject` that is a user) to a [`Person`]. Returns
/// `None` for other object types (groups, devices, service principals) and items without a valid `id` or that do not parse.
#[must_use]
pub fn person_from_user_json(v: Value) -> Option<Person> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        #[serde(default, rename = "@odata.type")]
        odata_type: Option<String>,
        id: UserId,
        #[serde(default)]
        display_name: Sensitive,
        #[serde(default)]
        mail: Option<String>,
        #[serde(default)]
        user_principal_name: Option<String>,
        #[serde(default)]
        job_title: Option<Sensitive>,
        #[serde(default)]
        department: Option<Sensitive>,
        #[serde(default)]
        office_location: Option<Sensitive>,
        #[serde(default, deserialize_with = "null_default")]
        business_phones: Vec<Option<String>>,
        #[serde(default)]
        mobile_phone: Option<String>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    if w.odata_type.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case("#microsoft.graph.user")) {
        return None;
    }
    let email = w
        .mail
        .and_then(|m| MailAddress::new(m).ok())
        .or_else(|| w.user_principal_name.and_then(|u| MailAddress::new(u).ok()));
    let mut phones = w.business_phones;
    phones.push(w.mobile_phone);
    Some(Person {
        id: Some(w.id),
        display_name: w.display_name,
        email,
        job_title: non_empty(w.job_title),
        department: non_empty(w.department),
        office_location: non_empty(w.office_location),
        phone: first_phone(phones),
    })
}

/// Maps a `/me/people` entry to a [`Person`]. Its `id` is kept only when it is a valid id (for
/// people outside the directory it is not a user id that resolves elsewhere).
#[must_use]
pub fn person_from_people_json(v: Value) -> Option<Person> {
    #[derive(Deserialize)]
    struct Scored {
        #[serde(default)]
        address: Option<String>,
    }
    #[derive(Deserialize)]
    struct Phone {
        #[serde(default)]
        number: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        display_name: Sensitive,
        #[serde(default, deserialize_with = "null_default")]
        scored_email_addresses: Vec<Scored>,
        #[serde(default)]
        job_title: Option<Sensitive>,
        #[serde(default)]
        department: Option<Sensitive>,
        #[serde(default)]
        office_location: Option<Sensitive>,
        #[serde(default, deserialize_with = "null_default")]
        phones: Vec<Phone>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    Some(Person {
        id: w.id.and_then(|id| UserId::new(id).ok()),
        display_name: w.display_name,
        email: w.scored_email_addresses.into_iter().find_map(|s| s.address.and_then(|a| MailAddress::new(a).ok())),
        job_title: non_empty(w.job_title),
        department: non_empty(w.department),
        office_location: non_empty(w.office_location),
        phone: first_phone(w.phones.into_iter().map(|p| p.number).collect()),
    })
}

fn user_get(path: String) -> Request {
    Request::get(path).family(ResourceFamily::Other).query("$select", USER_SELECT)
}

fn user_path(user: &Mailbox, rest: &str) -> Result<String> {
    Ok(format!("{}{rest}", user.prefix()?))
}

// ---------------------------------------------------------------------------------------------
// Me, users, org chart.
// ---------------------------------------------------------------------------------------------

/// Builds `GET /me` (or `/users/{id|upn}`) with [`USER_SELECT`].
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid user id.
pub fn get_user_request(user: &Mailbox) -> Result<Request> {
    Ok(user_get(user.prefix()?))
}

/// Builds `GET {user}/manager`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid user id.
pub fn manager_request(user: &Mailbox) -> Result<Request> {
    Ok(user_get(user_path(user, "/manager")?))
}

/// Builds `GET {user}/directReports`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid user id.
pub fn direct_reports_request(user: &Mailbox) -> Result<Request> {
    Ok(user_get(user_path(user, "/directReports")?).query("$top", "100"))
}

fn clean_query(q: &str) -> Result<String> {
    let cleaned: String =
        q.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').take(MAX_QUERY_LEN).collect();
    let cleaned = cleaned.trim().to_owned();
    if cleaned.is_empty() { Err(config("search text is empty")) } else { Ok(cleaned) }
}

/// Builds `GET /users?$search="displayName:{q}" OR "mail:{q}"` with `ConsistencyLevel: eventual`
/// (required for `$search` on directory objects). Quotes and backslashes are stripped from `q`.
///
/// # Errors
/// [`ErrorKind::Config`] for empty search text.
pub fn search_users_request(query: &str, top: Option<u32>) -> Result<Request> {
    let q = clean_query(query)?;
    Ok(user_get("/users".to_owned())
        .query("$search", format!("\"displayName:{q}\" OR \"mail:{q}\""))
        .query("$top", top.unwrap_or(DEFAULT_PEOPLE_PAGE_SIZE).clamp(1, 999).to_string())
        .query("$count", "true")
        .consistency_eventual())
}

/// Builds `GET /users?$filter=startswith(displayName,'p') or startswith(mail,'p')` (quotes in
/// `prefix` are doubled, as OData requires).
///
/// # Errors
/// [`ErrorKind::Config`] for an empty prefix.
pub fn users_starting_with_request(prefix: &str, top: Option<u32>) -> Result<Request> {
    let p = clean_query(prefix)?.replace('\'', "''");
    Ok(user_get("/users".to_owned())
        .query("$filter", format!("startswith(displayName,'{p}') or startswith(mail,'{p}')"))
        .query("$top", top.unwrap_or(DEFAULT_PEOPLE_PAGE_SIZE).clamp(1, 999).to_string())
        .query("$count", "true")
        .consistency_eventual())
}

/// Builds `GET /me/people` (relevance-ranked), optionally with `$search`.
///
/// # Errors
/// [`ErrorKind::Config`] for search text that is empty after cleaning.
pub fn people_request(search: Option<&str>, top: Option<u32>) -> Result<Request> {
    let mut req = Request::get("/me/people")
        .family(ResourceFamily::Other)
        .query("$select", PEOPLE_SELECT)
        .query("$top", top.unwrap_or(DEFAULT_PEOPLE_PAGE_SIZE).clamp(1, 1000).to_string());
    if let Some(q) = search {
        req = req.query("$search", format!("\"{}\"", clean_query(q)?));
    }
    Ok(req)
}

/// Builds `GET /groups/{id}/members` (users only are mapped).
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid group id.
pub fn group_members_request(group_id: &str) -> Result<Request> {
    Ok(user_get(format!("/groups/{}/members", segment(group_id)?)).query("$top", "100"))
}

// ---------------------------------------------------------------------------------------------
// Photos.
// ---------------------------------------------------------------------------------------------

/// A profile photo size Graph stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhotoSize {
    /// 48×48.
    S48,
    /// 64×64.
    S64,
    /// 96×96.
    S96,
    /// 120×120.
    S120,
    /// 240×240.
    S240,
    /// 360×360.
    S360,
    /// 432×432.
    S432,
    /// 504×504.
    S504,
    /// 648×648.
    S648,
}

impl PhotoSize {
    /// The Graph size id, e.g. `"96x96"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::S48 => "48x48",
            Self::S64 => "64x64",
            Self::S96 => "96x96",
            Self::S120 => "120x120",
            Self::S240 => "240x240",
            Self::S360 => "360x360",
            Self::S432 => "432x432",
            Self::S504 => "504x504",
            Self::S648 => "648x648",
        }
    }
}

/// A downloaded photo.
#[derive(Clone, PartialEq, Eq)]
pub struct Photo {
    /// MIME type from the response (`image/jpeg` usually).
    pub content_type: Option<String>,
    /// The image bytes (untrusted; decode with a bounded decoder).
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Photo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Photo").field("content_type", &self.content_type).field("len", &self.bytes.len()).finish()
    }
}

/// Builds `GET {user}/photo/$value` or `GET {user}/photos/{size}/$value`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid user id.
pub fn photo_request(user: &Mailbox, size: Option<PhotoSize>) -> Result<Request> {
    let rest = match size {
        None => "/photo/$value".to_owned(),
        Some(s) => format!("/photos/{}/$value", s.as_str()),
    };
    Ok(Request::get(user_path(user, &rest)?).family(ResourceFamily::Other).header("Accept", "image/*"))
}

// ---------------------------------------------------------------------------------------------
// Contacts.
// ---------------------------------------------------------------------------------------------

/// A personal contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contact {
    /// Contact id (not a directory id).
    pub id: String,
    /// Name, address, title, ... (`person.id` is always `None`).
    pub person: Person,
    /// Company.
    pub company_name: Option<Sensitive>,
}

/// Maps Graph contact JSON to a [`Contact`].
#[must_use]
pub fn contact_from_json(v: Value) -> Option<Contact> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        id: String,
        #[serde(default)]
        display_name: Sensitive,
        #[serde(default, deserialize_with = "null_default")]
        email_addresses: Vec<EmailAddress>,
        #[serde(default)]
        job_title: Option<Sensitive>,
        #[serde(default)]
        department: Option<Sensitive>,
        #[serde(default)]
        office_location: Option<Sensitive>,
        #[serde(default, deserialize_with = "null_default")]
        business_phones: Vec<Option<String>>,
        #[serde(default)]
        mobile_phone: Option<String>,
        #[serde(default)]
        company_name: Option<Sensitive>,
    }
    let w: Wire = serde_json::from_value(v).ok()?;
    if w.id.is_empty() || w.id.len() > quorum_model::MAX_ID_LEN {
        return None;
    }
    let mut phones = w.business_phones;
    phones.push(w.mobile_phone);
    Some(Contact {
        id: w.id,
        person: Person {
            id: None,
            display_name: w.display_name,
            email: w.email_addresses.into_iter().find_map(|e| e.address),
            job_title: non_empty(w.job_title),
            department: non_empty(w.department),
            office_location: non_empty(w.office_location),
            phone: first_phone(phones),
        },
        company_name: non_empty(w.company_name),
    })
}

/// Builds `GET {mailbox}/contacts`.
///
/// # Errors
/// [`ErrorKind::Config`] for an invalid mailbox.
pub fn list_contacts_request(mailbox: &Mailbox, top: Option<u32>) -> Result<Request> {
    Ok(mailbox
        .request(Method::Get, "/contacts", ResourceFamily::Mail)?
        .query("$select", CONTACT_SELECT)
        .query("$top", top.unwrap_or(100).clamp(1, 1000).to_string()))
}

/// A contact to create.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NewContact {
    /// Given name.
    pub given_name: Option<Sensitive>,
    /// Surname.
    pub surname: Option<Sensitive>,
    /// Display name (Graph derives one from the names when absent).
    pub display_name: Option<Sensitive>,
    /// Addresses.
    pub email_addresses: Vec<EmailAddress>,
    /// Business phone numbers.
    pub business_phones: Vec<Sensitive>,
    /// Mobile number.
    pub mobile_phone: Option<Sensitive>,
    /// Job title.
    pub job_title: Option<Sensitive>,
    /// Company.
    pub company_name: Option<Sensitive>,
}

impl NewContact {
    /// The Graph contact JSON.
    ///
    /// # Errors
    /// [`ErrorKind::Config`] when the contact has no name and no address.
    pub fn to_json(&self) -> Result<Value> {
        let has_name = [&self.given_name, &self.surname, &self.display_name]
            .iter()
            .any(|n| n.as_ref().is_some_and(|n| !n.is_empty()));
        let addresses: Vec<Value> = self
            .email_addresses
            .iter()
            .filter_map(|e| {
                e.address.as_ref().map(|a| {
                    let mut m = Map::new();
                    m.insert("address".into(), Value::String(a.as_str().to_owned()));
                    if !e.name.is_empty() {
                        m.insert("name".into(), Value::String(e.name.as_str().to_owned()));
                    }
                    Value::Object(m)
                })
            })
            .collect();
        if !has_name && addresses.is_empty() {
            return Err(config("a contact needs a name or an address"));
        }
        let mut m = Map::new();
        let mut put = |key: &str, v: &Option<Sensitive>| {
            if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                m.insert(key.into(), Value::String(v.as_str().to_owned()));
            }
        };
        put("givenName", &self.given_name);
        put("surname", &self.surname);
        put("displayName", &self.display_name);
        put("mobilePhone", &self.mobile_phone);
        put("jobTitle", &self.job_title);
        put("companyName", &self.company_name);
        if !addresses.is_empty() {
            m.insert("emailAddresses".into(), Value::Array(addresses));
        }
        if !self.business_phones.is_empty() {
            let phones: Vec<&str> = self.business_phones.iter().map(Sensitive::as_str).collect();
            m.insert("businessPhones".into(), json!(phones));
        }
        Ok(Value::Object(m))
    }
}

/// Builds `POST {mailbox}/contacts`.
///
/// # Errors
/// As [`NewContact::to_json`].
pub fn create_contact_request(mailbox: &Mailbox, contact: &NewContact) -> Result<Request> {
    Ok(mailbox.request(Method::Post, "/contacts", ResourceFamily::Mail)?.json_value(contact.to_json()?))
}

impl Client {
    /// The signed-in user (`/me`) or a user by id or UPN.
    ///
    /// # Errors
    /// As [`Client::send_json`]; [`ErrorKind::Malformed`] if it does not map.
    pub async fn get_user(&self, user: &Mailbox) -> Result<Person> {
        let v: Value = self.send_json(get_user_request(user)?).await?;
        person_from_user_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "user did not parse"))
    }

    /// The signed-in user.
    ///
    /// # Errors
    /// As [`Client::get_user`].
    pub async fn me(&self) -> Result<Person> {
        self.get_user(&Mailbox::Me).await
    }

    /// A user's manager; `None` when they have none (Graph answers 404).
    ///
    /// # Errors
    /// As [`Client::send_json`] (other than 404).
    pub async fn get_manager(&self, user: &Mailbox) -> Result<Option<Person>> {
        match self.send_json::<Value>(manager_request(user)?).await {
            Ok(v) => Ok(person_from_user_json(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Parses any page of directory users (non-user members such as groups are skipped).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn users_page(&self, req: Request) -> Result<Page<Person>> {
        mapped_page(self, req, person_from_user_json).await
    }

    /// A user's direct reports (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_direct_reports(&self, user: &Mailbox) -> Result<Page<Person>> {
        self.users_page(direct_reports_request(user)?).await
    }

    /// Searches the directory by name or address (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn search_users(&self, query: &str, top: Option<u32>) -> Result<Page<Person>> {
        self.users_page(search_users_request(query, top)?).await
    }

    /// Directory users whose name or address starts with `prefix` (pickers; first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn users_starting_with(&self, prefix: &str, top: Option<u32>) -> Result<Page<Person>> {
        self.users_page(users_starting_with_request(prefix, top)?).await
    }

    /// Relevance-ranked people for pickers (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_people(&self, search: Option<&str>, top: Option<u32>) -> Result<Page<Person>> {
        mapped_page(self, people_request(search, top)?, person_from_people_json).await
    }

    /// Members of a Microsoft 365 group or distribution list (first page). Members that are not
    /// users (nested groups, devices) are counted in `skipped`.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_group_members(&self, group_id: &str) -> Result<Page<Person>> {
        self.users_page(group_members_request(group_id)?).await
    }

    /// A user's photo; `None` when they have none (404).
    ///
    /// # Errors
    /// As [`Client::send_raw`] (other than 404).
    pub async fn get_photo(&self, user: &Mailbox, size: Option<PhotoSize>) -> Result<Option<Photo>> {
        match self.send_raw(photo_request(user, size)?).await {
            Ok(resp) => {
                Ok(Some(Photo { content_type: resp.header("content-type").map(str::to_owned), bytes: resp.body }))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Parses any page of contacts.
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn contacts_page(&self, req: Request) -> Result<Page<Contact>> {
        mapped_page(self, req, contact_from_json).await
    }

    /// Personal contacts (first page).
    ///
    /// # Errors
    /// As [`Client::page`].
    pub async fn list_contacts(&self, mailbox: &Mailbox, top: Option<u32>) -> Result<Page<Contact>> {
        self.contacts_page(list_contacts_request(mailbox, top)?).await
    }

    /// Creates a contact.
    ///
    /// # Errors
    /// As [`Client::send_json`].
    pub async fn create_contact(&self, mailbox: &Mailbox, contact: &NewContact) -> Result<Contact> {
        let v: Value = self.send_json(create_contact_request(mailbox, contact)?).await?;
        contact_from_json(v).ok_or_else(|| Error::new(ErrorKind::Malformed, "contact did not parse"))
    }
}
