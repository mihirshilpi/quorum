//! Built-in commands, one module per area.

mod account;
mod app;
mod calendar;
mod channels;
mod chat;
mod files;
mod mail;
mod people;
mod presence;
mod search;
pub mod settings;
mod sites;
mod sync;
mod view;

use crate::command::CommandSpec;

/// Every built-in command.
pub(crate) fn all() -> Vec<CommandSpec> {
    let mut v = Vec::new();
    v.extend(account::commands());
    v.extend(app::commands());
    v.extend(calendar::commands());
    v.extend(channels::commands());
    v.extend(chat::commands());
    v.extend(files::commands());
    v.extend(sites::commands());
    v.extend(mail::commands());
    v.extend(people::commands());
    v.extend(presence::commands());
    v.extend(search::commands());
    v.extend(settings::commands());
    v.extend(sync::commands());
    v.extend(view::commands());
    v
}

/// Every built-in query.
pub(crate) fn queries() -> Vec<CommandSpec> {
    let mut v = Vec::new();
    v.extend(account::queries());
    v.extend(app::queries());
    v.extend(calendar::queries());
    v.extend(channels::queries());
    v.extend(chat::queries());
    v.extend(files::queries());
    v.extend(sites::queries());
    v.extend(mail::queries());
    v.extend(people::queries());
    v.extend(presence::queries());
    v.extend(search::queries());
    v.extend(settings::queries());
    v.extend(sync::queries());
    v.extend(view::queries());
    v
}
