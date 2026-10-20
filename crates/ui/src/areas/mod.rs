//! One module per app area. Each area is a component built from [`common::AreaLayout`]:
//! navigation column (optional) | list pane with command bar | reading pane.
//!
//! To add a view: define the row type (`Deserialize + Clone`, implement [`common::Row`]), bind
//! it with `state::use_query("<query.id>", params, &["<tables>"])`, render it with
//! [`common::QueryList`], and add the query to `mock.rs` so browser preview has data.

pub mod calendar;
pub mod calendar_date;
pub mod calendar_grid;
pub mod calendar_icons;
pub mod calendar_layout;
pub mod calendar_mock;
pub mod calendar_side;
pub mod chat;
pub mod chat_icons;
pub mod chat_logic;
pub mod chat_mock;
pub mod chat_view;
pub mod common;
pub mod files;
pub mod files_details;
pub mod files_dialogs;
pub mod files_icons;
pub mod files_lists;
pub mod files_mock;
pub mod mail;
pub mod mail_compose;
pub mod mail_icons;
pub mod mail_mock;
pub mod mail_read;
pub mod mail_thread;
pub mod mail_util;
pub mod people;
pub mod people_mock;
pub mod people_ui;
pub mod search;
pub mod settings;
pub mod teams;

use leptos::prelude::*;

use crate::state::Area;

/// The view for `area`.
pub fn view_for(area: Area) -> AnyView {
    match area {
        Area::Mail => view! { <mail::MailArea /> }.into_any(),
        Area::Calendar => view! { <calendar::CalendarArea /> }.into_any(),
        Area::Chat => view! { <chat::ChatArea /> }.into_any(),
        Area::Teams => view! { <teams::TeamsArea /> }.into_any(),
        Area::Files => view! { <files::FilesArea /> }.into_any(),
        Area::People => view! { <people::PeopleArea /> }.into_any(),
        Area::Search => view! { <search::SearchArea /> }.into_any(),
        Area::Settings => view! { <settings::SettingsArea /> }.into_any(),
    }
}
