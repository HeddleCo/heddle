// SPDX-License-Identifier: Apache-2.0
//! Context state attachments.
//!
//! Shared by the CLI context verbs (set/rm/supersede/...) and the hosted
//! context sync path; both read and write the same per-state Context root.

use anyhow::Result;
use chrono::Utc;
use objects::{
    object::{
        ContentHash, State, StateAttachment, StateAttachmentBody, StateAttachmentKind, Tree,
    },
    store::ObjectStore,
};
use repo::Repository;

use crate::attribution::resolve_attribution;

pub fn context_root_for_state(
    repo: &Repository,
    state: &State,
) -> Result<Option<ContentHash>> {
    Ok(repo
        .latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?
        .and_then(|attachment| match attachment.body {
            StateAttachmentBody::Context(root) => Some(root),
            _ => None,
        }))
}

pub fn put_context_attachment(
    repo: &Repository,
    state: &State,
    new_context_root: Option<ContentHash>,
) -> Result<ContentHash> {
    let root = match new_context_root {
        Some(root) => root,
        None => repo.store().put_tree(&Tree::new())?,
    };
    let prior = repo.latest_state_attachment(&state.state_id, StateAttachmentKind::Context)?;
    let user_config = config::UserConfig::load_default()?;
    let attribution = resolve_attribution(repo, &user_config)?;
    let created_at = prior
        .as_ref()
        .map(|attachment| attachment.created_at + chrono::Duration::nanoseconds(1))
        .map_or_else(Utc::now, |minimum| minimum.max(Utc::now()));
    repo.put_state_attachment(&StateAttachment {
        state_id: state.state_id,
        body: StateAttachmentBody::Context(root),
        attribution,
        created_at,
        supersedes: prior.map(|attachment| attachment.id()),
    })?;
    Ok(root)
}
