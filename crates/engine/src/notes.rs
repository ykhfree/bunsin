//! The agent's notes (wiki): proposed changes, their approval, and the owner's own edits.

use crate::{Ctx, Engine, store_error};
use bunsin_api::{ApiError, ApiResult, Outcome, WikiPageUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::InboundEvent;
use bunsin_core::records::{InboxBody, InboxItem, ReadNote};
use bunsin_core::wiki::{MAX_PAGES, WIKI_READ_TOOL, WikiAuthor, WikiPage, WikiSource, WikiUpdate, valid_slug};
use bunsin_store::{ProfileStore, WikiWrite};
use serde_json::{Value, json};
use std::collections::BTreeSet;

impl Engine {
    /// Puts the page changes of a reply into the inbox. Nothing is stored in the notes
    /// before the owner approves an item.
    pub(crate) fn file_wiki_changes(
        &self,
        ctx: &Ctx,
        key: &str,
        event: &InboundEvent,
        reads: &[ReadNote],
        pages_read: &BTreeSet<String>,
        updates: Vec<WikiUpdate>,
    ) -> ApiResult<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let store = &ctx.store;
        let dropped = |slug: &str, reason: &str| {
            self.audit(
                store,
                "wiki_change_dropped",
                Some(key),
                json!({"slug": slug, "reason": reason}),
            )
        };
        // A fact must come from outside the notes. A read of the notes is not a source,
        // or the notes could cite themselves.
        let mut sources: Vec<WikiSource> = reads
            .iter()
            .filter(|read| read.tool != WIKI_READ_TOOL)
            .map(|read| WikiSource {
                origin: read.tool.clone(),
                detail: Value::Object(read.args.clone()).to_string(),
            })
            .collect();
        if sources.is_empty() {
            for update in &updates {
                dropped(
                    &update.slug,
                    "no tool was read in this event, so the fact has no source",
                )?;
            }
            return Ok(());
        }
        sources.insert(
            0,
            WikiSource {
                origin: "event".to_owned(),
                detail: format!("{} message {} by {}", event.source, event.external_id, event.author),
            },
        );

        let page_count = store.wiki_index().map_err(store_error)?.len();
        for update in updates {
            let existing = store.wiki_page(&update.slug).map_err(store_error)?;
            // An update replaces the whole page, so the agent must have seen what it replaces.
            if existing.is_some() && !pages_read.contains(&update.slug) {
                dropped(&update.slug, "the page exists and was not read in this event")?;
                continue;
            }
            if store.has_pending_wiki_change(&update.slug).map_err(store_error)? {
                dropped(&update.slug, "a change to this page is already waiting for the owner")?;
                continue;
            }
            if existing.is_none() && page_count >= MAX_PAGES {
                dropped(&update.slug, "the notes are full")?;
                continue;
            }
            let text = update.text.filtered(&ctx.redactor);
            if let Err(error) = text.check() {
                dropped(&update.slug, &error)?;
                continue;
            }
            let body = InboxBody::WikiChange {
                slug: update.slug.clone(),
                text,
                sources: sources.clone(),
                base_revision: existing.as_ref().map_or(0, |page| page.revision),
                previous: existing.map(|page| page.text),
            };
            let rationale = "a fact the agent read in this event and wants to keep in its notes";
            store
                .add_inbox_item(key, &body, rationale, reads, ApprovalState::Pending, self.now())
                .map_err(store_error)?;
            self.audit(store, "wiki_change_proposed", Some(key), json!({"slug": update.slug}))?;
        }
        Ok(())
    }

    /// Stores an approved page change. The page write and the inbox move are one step.
    pub(crate) fn approve_wiki_change(
        &self,
        store: &ProfileStore,
        item: &InboxItem,
        edited: bool,
    ) -> ApiResult<Outcome> {
        let InboxBody::WikiChange {
            slug,
            text,
            sources,
            base_revision,
            ..
        } = &item.body
        else {
            return Err(ApiError::NotAllowed("the item is not a page change".into()));
        };
        if edited {
            return Err(ApiError::Invalid(
                "a page change is approved as it is; edit the page afterwards".into(),
            ));
        }
        let key = Some(item.event_key.as_str());
        let written = store
            .write_wiki_page(
                slug,
                text,
                sources,
                WikiAuthor::Agent,
                *base_revision,
                Some(item.id),
                self.now(),
            )
            .map_err(store_error)?;
        match written {
            WikiWrite::Stored(page) => {
                let detail = json!({"inbox_id": item.id, "slug": slug, "revision": page.revision});
                self.audit(store, "wiki_change_applied", key, detail)?;
                Ok(Outcome::Executed { inbox_id: item.id })
            }
            WikiWrite::RevisionMismatch { .. } => Err(ApiError::NotAllowed(format!(
                "the page `{slug}` changed after this proposal was written; dismiss the proposal"
            ))),
            WikiWrite::ItemHandled => Err(ApiError::NotAllowed(format!(
                "the item is {}, not pending",
                item.state.as_str()
            ))),
        }
    }

    pub(crate) fn read_wiki_page(&self, profile: &str, slug: &str) -> ApiResult<WikiPage> {
        let page = self.store(profile)?.wiki_page(slug).map_err(store_error)?;
        page.ok_or_else(|| ApiError::NotFound(format!("page `{slug}`")))
    }

    /// Stores the owner's own text for a page. It is not filtered here: it is the owner's
    /// text. It passes the privacy filter when it is quoted into a prompt.
    pub(crate) fn write_owner_page(&self, profile: &str, slug: &str, update: WikiPageUpdate) -> ApiResult<WikiPage> {
        if !valid_slug(slug) {
            return Err(ApiError::Invalid(format!(
                "the page name `{slug}` is invalid: use a-z, 0-9 and `-`"
            )));
        }
        update.text.check().map_err(ApiError::Invalid)?;
        let store = self.store(profile)?;
        let current = store
            .wiki_page(slug)
            .map_err(store_error)?
            .map_or(0, |page| page.revision);
        let base = update.expected_revision.unwrap_or(current);
        // The owner's sources are the owner. The earlier sources described the agent's text.
        let written = store
            .write_wiki_page(slug, &update.text, &[], WikiAuthor::Owner, base, None, self.now())
            .map_err(store_error)?;
        match written {
            WikiWrite::Stored(page) => {
                self.audit(
                    &store,
                    "wiki_page_edited",
                    None,
                    json!({"slug": slug, "revision": page.revision}),
                )?;
                Ok(page)
            }
            WikiWrite::RevisionMismatch { current } => Err(ApiError::Conflict(format!(
                "the page `{slug}` is at revision {current}, not at the revision this edit started from"
            ))),
            WikiWrite::ItemHandled => unreachable!("no inbox item was named"),
        }
    }

    pub(crate) fn delete_owner_page(&self, profile: &str, slug: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        if !store.delete_wiki_page(slug, self.now()).map_err(store_error)? {
            return Err(ApiError::NotFound(format!("page `{slug}`")));
        }
        self.audit(&store, "wiki_page_deleted", None, json!({"slug": slug}))
    }
}
