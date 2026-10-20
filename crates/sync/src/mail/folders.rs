//! The folder tree: `mailFolders/delta`, well-known roles, mailbox settings, categories.

use std::collections::BTreeMap;

use futures::future::BoxFuture;
use quorum_graph::ErrorKind as GraphKind;
use quorum_graph::api::mail::{Change, FolderRef, Mailbox, delta_link_request, folders_delta_request};
use quorum_graph::{DeltaOutcome, Request};
use quorum_model::{GraphEnum, MailFolder, MailFolderId};
use quorum_store::DeltaState;

use super::{
    CATEGORIES_SETTING, FOLDERS_RESOURCE, MAILBOX_SETTINGS_SETTING, RESOLVED_FOLDERS, WELL_KNOWN_SETTING, well_known,
};
use crate::{Cadence, Ctx, Outcome, Resource, Result};

/// Pages of a folder delta followed in one pass.
const MAX_FOLDER_PAGES: usize = 50;

/// The folder tree resource (`mail.folders`).
#[derive(Debug, Clone, Default)]
pub struct FoldersResource;

impl FoldersResource {
    /// The resource.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Resource for FoldersResource {
    fn key(&self) -> String {
        FOLDERS_RESOURCE.to_owned()
    }

    fn area(&self) -> &'static str {
        "mail"
    }

    fn cadence(&self) -> Cadence {
        Cadence::secs(600, 600, 1800)
    }

    fn run<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(async move {
            let roles = resolve_roles(ctx).await?;
            let before = folder_ids(ctx).await?;
            let state = ctx.store.delta_state(FOLDERS_RESOURCE.to_owned()).await?;
            let mut state = state.unwrap_or_else(|| DeltaState::new(FOLDERS_RESOURCE));
            match folder_round(ctx, &mut state, &roles).await {
                Err(e) if super::delta_expired(&e) => {
                    tracing::info!(name: "sync.mail.folders_resync", "folder delta expired; resyncing");
                    state.delta_link = None;
                    state.next_link = None;
                    folder_round(ctx, &mut state, &roles).await?;
                }
                other => other?,
            }
            refresh_extras(ctx).await;
            // A folder appeared or went away: rediscover now so it syncs (or stops) without
            // waiting for the periodic rediscovery.
            if folder_ids(ctx).await? != before { Ok(Outcome::Rediscover) } else { Ok(Outcome::CaughtUp) }
        })
    }
}

/// The ids of the stored folders.
async fn folder_ids(ctx: &Ctx) -> Result<std::collections::BTreeSet<MailFolderId>> {
    Ok(ctx.store.mail_folders().await?.items.into_iter().map(|f| f.id).collect())
}

/// Looks up the ids of the well-known folders once (kept in a setting).
async fn resolve_roles(ctx: &Ctx) -> Result<BTreeMap<String, MailFolderId>> {
    let mut roles = well_known(&ctx.store).await?;
    if roles.contains_key(WellKnownFolderName::INBOX) {
        return Ok(roles);
    }
    let mut complete = true;
    let mut found = Vec::new();
    for name in RESOLVED_FOLDERS {
        match ctx.graph.get_mail_folder(&Mailbox::Me, &FolderRef::WellKnown(name.clone())).await {
            Ok(f) => {
                roles.insert(name.as_graph().to_owned(), f.id.clone());
                found.push(f);
            }
            Err(e) if matches!(e.kind(), GraphKind::NotFound | GraphKind::Forbidden | GraphKind::BadRequest) => {}
            Err(e) => {
                tracing::debug!(name: "sync.mail.role_lookup_failed", kind = ?e.kind(), "well-known folder lookup failed");
                complete = false;
            }
        }
    }
    let save = complete && roles.contains_key(WellKnownFolderName::INBOX);
    let to_store = roles.clone();
    ctx.store
        .write(move |tx| {
            tx.upsert_mail_folders(&found)?;
            if save {
                tx.set_setting(WELL_KNOWN_SETTING, &to_store)?;
            }
            Ok(())
        })
        .await?;
    Ok(roles)
}

struct WellKnownFolderName;
impl WellKnownFolderName {
    const INBOX: &'static str = "inbox";
}

/// Runs the folder delta from the stored position to the end (bounded).
async fn folder_round(ctx: &Ctx, state: &mut DeltaState, roles: &BTreeMap<String, MailFolderId>) -> Result<()> {
    let first = match state.next_link.as_ref().or(state.delta_link.as_ref()) {
        Some(link) => delta_link_request(&Mailbox::Me, link.clone(), None),
        None => folders_delta_request(&Mailbox::Me)?,
    };
    let mut req: Request = first;
    for _ in 0..MAX_FOLDER_PAGES {
        let page = ctx.graph.mail_folders_delta_page(req.clone()).await?;
        let mut upserts: Vec<MailFolder> = Vec::new();
        let mut removed: Vec<MailFolderId> = Vec::new();
        for change in page.items {
            match change {
                Change::Upsert(mut f) => {
                    if f.well_known_name.is_none()
                        && let Some((name, _)) = roles.iter().find(|(_, id)| **id == f.id)
                    {
                        f.well_known_name = Some(quorum_model::WellKnownFolder::from_graph(name));
                    }
                    upserts.push(f);
                }
                Change::Removed(id) => removed.push(id),
            }
        }
        let done = match &page.outcome {
            DeltaOutcome::Done(link) => {
                state.delta_link = Some(link.clone());
                state.next_link = None;
                true
            }
            DeltaOutcome::Next(link) => {
                state.next_link = Some(link.clone());
                false
            }
        };
        let st = state.clone();
        ctx.store
            .write(move |tx| {
                tx.upsert_mail_folders(&upserts)?;
                tx.delete_mail_folders(&removed)?;
                tx.set_delta_state(&st)
            })
            .await?;
        if done {
            return Ok(());
        }
        if let Some(link) = state.next_link.clone() {
            req = delta_link_request(&Mailbox::Me, link, None);
        }
    }
    Ok(())
}

/// Mailbox settings (automatic replies) and master categories; failures are not fatal.
async fn refresh_extras(ctx: &Ctx) {
    if let Ok(settings) = ctx.graph.get_mailbox_settings(&Mailbox::Me).await {
        let _ = ctx.store.set_setting(MAILBOX_SETTINGS_SETTING.to_owned(), settings).await;
    }
    if let Ok(cats) = ctx.graph.list_master_categories(&Mailbox::Me).await {
        let _ = ctx.store.set_setting(CATEGORIES_SETTING.to_owned(), cats.items).await;
    }
}
