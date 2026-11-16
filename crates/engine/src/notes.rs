//! The agent's notes (wiki): the index and the built-in tools in a prompt, proposed changes,
//! their approval, and the owner's own edits.

use crate::{Ctx, Engine, store_error};
use bunsin_api::{ApiError, ApiResult, Outcome, WikiPageUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::context::ToolHint;
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::records::{InboxBody, InboxItem, ReadNote};
use bunsin_core::search::query_terms;
use bunsin_core::wiki::{
    MAX_PAGES, MAX_SEARCH_RESULTS, WIKI_READ_TOOL, WIKI_SEARCH_TOOL, WikiAuthor, WikiPage, WikiSource, WikiSummary,
    WikiUpdate, index_text, is_notes_tool, page_text, prompt_index, valid_slug,
};
use bunsin_store::{ProfileStore, WikiWrite};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// The built-in notes tools as the LLM sees them.
pub(crate) fn notes_tools() -> [ToolHint; 2] {
    let one_string =
        |name: &str| json!({"type": "object", "required": [name], "properties": {name: {"type": "string"}}});
    [
        ToolHint {
            tool_id: WIKI_READ_TOOL.to_owned(),
            description: "Read one page of your notes.".to_owned(),
            input_schema: one_string("slug"),
        },
        ToolHint {
            tool_id: WIKI_SEARCH_TOOL.to_owned(),
            description: format!(
                "Find pages of your notes by words. Gives up to {MAX_SEARCH_RESULTS} pages: slug, title and summary."
            ),
            input_schema: one_string("query"),
        },
    ]
}

impl Engine {
    /// The index of the notes for the prompt of an event: the pages related to the event first.
    pub(crate) fn notes_index(&self, store: &ProfileStore, event: &InboundEvent) -> ApiResult<String> {
        let ranked = store.search_wiki(&query_terms(&event.text)).map_err(store_error)?;
        let pages = store.wiki_index().map_err(store_error)?;
        Ok(prompt_index(&pages, &ranked, self.now()))
    }

    /// Runs a built-in notes tool. Returns its result as filtered text, and false if the page
    /// to read does not exist. A page that is read joins `pages_read`.
    pub(crate) fn run_notes_tool(
        &self,
        ctx: &Ctx,
        tool: &str,
        args: &Map<String, Value>,
        pages_read: &mut BTreeSet<String>,
    ) -> ApiResult<(String, bool)> {
        let store = &ctx.store;
        let filter = |text: &str| ctx.redactor.redact(text, &mut RedactionCounts::new());
        let arg = |name: &str| args.get(name).and_then(Value::as_str).unwrap_or_default();
        if tool == WIKI_SEARCH_TOOL {
            let found = store.search_wiki(&query_terms(arg("query"))).map_err(store_error)?;
            let index = store.wiki_index().map_err(store_error)?;
            let pages: Vec<WikiSummary> = found
                .iter()
                .take(MAX_SEARCH_RESULTS)
                .filter_map(|slug| index.iter().find(|page| &page.slug == slug).cloned())
                .collect();
            if pages.is_empty() {
                return Ok(("No page of your notes matches the query.".to_owned(), true));
            }
            return Ok((filter(&index_text(&pages, self.now())), true));
        }
        let slug = arg("slug");
        match store.wiki_page(slug).map_err(store_error)? {
            Some(page) => {
                pages_read.insert(slug.to_owned());
                store.mark_wiki_read(slug, self.now()).map_err(store_error)?;
                Ok((filter(&page_text(&page, self.now())), true))
            }
            None => Ok((format!("Your notes have no page `{slug}`."), false)),
        }
    }

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
        // A fact must come from outside the notes. A read or a search of the notes is not a source,
        // or the notes could cite themselves.
        let mut sources: Vec<WikiSource> = reads
            .iter()
            .filter(|read| !is_notes_tool(&read.tool))
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
        // An event from outside can be written to steer the agent, so its changes always wait for the owner.
        let may_apply = ctx.config.notes.auto_apply
            && event.channel == Channel::Owner
            && !store.kill_switch().map_err(store_error)?;
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
            // The owner's own text is never replaced without the owner.
            let apply = may_apply && existing.as_ref().is_none_or(|page| page.author == WikiAuthor::Agent);
            let body = InboxBody::WikiChange {
                slug: update.slug.clone(),
                text,
                sources: sources.clone(),
                base_revision: existing.as_ref().map_or(0, |page| page.revision),
                previous: existing.map(|page| page.text),
            };
            let rationale = "a fact the agent read in this event and wants to keep in its notes";
            let item = store
                .add_inbox_item(key, &body, rationale, reads, ApprovalState::Pending, self.now())
                .map_err(store_error)?;
            self.audit(store, "wiki_change_proposed", Some(key), json!({"slug": update.slug}))?;
            // A change that cannot be stored now stays in the inbox for the owner.
            if apply && let Err(error) = self.approve_wiki_change(store, &item, false, true) {
                let detail = json!({"inbox_id": item.id, "error": error.to_string()});
                self.audit(store, "wiki_change_auto_apply_failed", Some(key), detail)?;
            }
        }
        Ok(())
    }

    /// Stores an approved page change. The page write and the inbox move are one step.
    /// `auto` is true if the setting `notes.auto_apply` approves it, not the owner.
    pub(crate) fn approve_wiki_change(
        &self,
        store: &ProfileStore,
        item: &InboxItem,
        edited: bool,
        auto: bool,
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
                let detail = json!({"inbox_id": item.id, "slug": slug, "revision": page.revision, "auto": auto});
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
        let current = store.wiki_page(slug).map_err(store_error)?;
        let (revision, sources) = current.map_or((0, vec![]), |page| (page.revision, page.sources));
        let base = update.expected_revision.unwrap_or(revision);
        // The sources of the page stay: an edit by the owner does not make its facts come from nowhere.
        // The write is refused if the page is not at `base`, so these are the sources of the replaced text.
        let written = store
            .write_wiki_page(slug, &update.text, &sources, WikiAuthor::Owner, base, None, self.now())
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
