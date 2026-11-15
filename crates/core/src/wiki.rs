//! The agent's notes about the owner's work: pages of durable facts with their sources.
//!
//! A page is data. It is quoted into prompts like any other text from outside, and it
//! never changes what the agent is allowed to do.

use crate::privacy::{RedactionCounts, Redactor};
use serde::{Deserialize, Serialize};

/// The ID of the built-in tool that reads one page. A connector name cannot contain `.`,
/// so no connector tool can have this ID.
pub const WIKI_READ_TOOL: &str = "bunsin.wiki/read";

pub const MAX_SLUG_LEN: usize = 64;
pub const MAX_TITLE_CHARS: usize = 120;
pub const MAX_SUMMARY_CHARS: usize = 200;
pub const MAX_BODY_CHARS: usize = 8_000;
/// The most page changes one event may propose. More would flood the approval inbox.
pub const MAX_UPDATES_PER_EVENT: usize = 3;
/// The most pages a profile holds. The index of all pages goes into every prompt.
pub const MAX_PAGES: usize = 300;

/// Where a fact on a page came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiSource {
    /// A tool ID such as `slack/search`, or `event` for the message that was handled.
    pub origin: String,
    /// What identifies the source: the tool arguments, or the event's source, author and ID.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WikiAuthor {
    /// The agent proposed the text and the owner approved it.
    Agent,
    /// The owner wrote the text.
    Owner,
}

/// The text of a page.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiText {
    pub title: String,
    /// One line for the index.
    pub summary: String,
    /// Markdown.
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiPage {
    pub slug: String,
    #[serde(flatten)]
    pub text: WikiText,
    pub sources: Vec<WikiSource>,
    /// Starts at 1 and grows by 1 with each change.
    pub revision: u64,
    pub updated_at: i64,
    pub author: WikiAuthor,
}

/// One line of the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiSummary {
    pub slug: String,
    pub title: String,
    pub summary: String,
    pub revision: u64,
    pub updated_at: i64,
}

/// A page change the LLM proposes in its reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiUpdate {
    pub slug: String,
    #[serde(flatten)]
    pub text: WikiText,
}

/// True if `slug` can name a page: 1 to 64 of a-z, 0-9 and `-`.
pub fn valid_slug(slug: &str) -> bool {
    (1..=MAX_SLUG_LEN).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl WikiText {
    /// Checks the lengths and that the summary is one line. The error says what to change.
    pub fn check(&self) -> Result<(), String> {
        let chars = |text: &str| text.trim().chars().count();
        if !(1..=MAX_TITLE_CHARS).contains(&chars(&self.title)) {
            return Err(format!("the title must have 1 to {MAX_TITLE_CHARS} characters"));
        }
        if !(1..=MAX_SUMMARY_CHARS).contains(&chars(&self.summary)) || self.summary.trim().contains('\n') {
            return Err(format!(
                "the summary must be one line of 1 to {MAX_SUMMARY_CHARS} characters"
            ));
        }
        if !(1..=MAX_BODY_CHARS).contains(&chars(&self.body)) {
            return Err(format!("the body must have 1 to {MAX_BODY_CHARS} characters"));
        }
        Ok(())
    }

    /// The text with surrounding space removed and personal data replaced.
    pub fn filtered(&self, redactor: &Redactor) -> WikiText {
        let clean = |text: &str| redactor.redact(text.trim(), &mut RedactionCounts::new());
        WikiText {
            title: clean(&self.title),
            summary: clean(&self.summary),
            body: clean(&self.body),
        }
    }
}

/// Checks the page changes of one reply.
pub fn check_updates(updates: &[WikiUpdate]) -> Result<(), String> {
    if updates.len() > MAX_UPDATES_PER_EVENT {
        return Err(format!(
            "`wiki_updates` has more than {MAX_UPDATES_PER_EVENT} pages; keep the most durable facts"
        ));
    }
    for (index, update) in updates.iter().enumerate() {
        if !valid_slug(&update.slug) {
            return Err(format!(
                "the slug `{}` is invalid: use 1 to {MAX_SLUG_LEN} of a-z, 0-9 and `-`",
                update.slug
            ));
        }
        update
            .text
            .check()
            .map_err(|e| format!("page `{}`: {e}", update.slug))?;
        if updates[..index].iter().any(|earlier| earlier.slug == update.slug) {
            return Err(format!("`wiki_updates` names the page `{}` twice", update.slug));
        }
    }
    Ok(())
}

/// The index as text for a prompt: one line per page.
pub fn index_text(pages: &[WikiSummary], now: i64) -> String {
    pages
        .iter()
        .map(|page| {
            let days = (now - page.updated_at).max(0) / 86_400;
            format!(
                "{} — {}: {} (updated {days} day(s) ago)",
                page.slug, page.title, page.summary
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A page as text for a prompt.
pub fn page_text(page: &WikiPage, now: i64) -> String {
    let days = (now - page.updated_at).max(0) / 86_400;
    let sources: Vec<String> = page
        .sources
        .iter()
        .map(|s| format!("- {}: {}", s.origin, s.detail))
        .collect();
    format!(
        "# {}\n{}\n\n{}\n\nUpdated {days} day(s) ago. Sources:\n{}",
        page.text.title,
        page.text.summary,
        page.text.body,
        if sources.is_empty() {
            "- (none recorded)".to_owned()
        } else {
            sources.join("\n")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::PrivacyConfig;

    fn text(title: &str, summary: &str, body: &str) -> WikiText {
        WikiText {
            title: title.into(),
            summary: summary.into(),
            body: body.into(),
        }
    }

    fn update(slug: &str) -> WikiUpdate {
        WikiUpdate {
            slug: slug.into(),
            text: text("Deploy schedule", "When we deploy", "Fridays at 3 pm."),
        }
    }

    #[test]
    fn the_built_in_tool_id_cannot_be_a_connector_tool() {
        let (connector, tool) = crate::proposal::split_tool_id(WIKI_READ_TOOL).unwrap();
        assert_eq!(tool, "read");
        assert!(!crate::settings::valid_connector_name(connector));
        // A connector that is named `wiki` has another ID.
        assert_ne!(WIKI_READ_TOOL, "wiki/read");
    }

    #[test]
    fn slugs_are_short_lower_case_names() {
        for slug in ["deploy-schedule", "team", "proj-12"] {
            assert!(valid_slug(slug), "{slug}");
        }
        let long = "a".repeat(65);
        for slug in ["", "Deploy", "deploy schedule", "a/b", "../x", "a_b", long.as_str()] {
            assert!(!valid_slug(slug), "{slug:?}");
        }
    }

    #[test]
    fn page_text_has_length_limits_and_a_one_line_summary() {
        assert_eq!(text("T", "S", "B").check(), Ok(()));
        assert!(text(" ", "S", "B").check().unwrap_err().contains("title"));
        assert!(text(&"t".repeat(121), "S", "B").check().unwrap_err().contains("title"));
        assert!(
            text("T", "line one\nline two", "B")
                .check()
                .unwrap_err()
                .contains("one line")
        );
        assert!(
            text("T", &"s".repeat(201), "B")
                .check()
                .unwrap_err()
                .contains("summary")
        );
        assert!(text("T", "S", "").check().unwrap_err().contains("body"));
        assert!(
            text("T", "S", &"가".repeat(8_001))
                .check()
                .unwrap_err()
                .contains("body")
        );
        assert_eq!(text("T", "S", &"가".repeat(8_000)).check(), Ok(()));
    }

    #[test]
    fn a_reply_may_change_a_few_distinct_valid_pages() {
        assert_eq!(check_updates(&[]), Ok(()));
        assert_eq!(check_updates(&[update("a"), update("b"), update("c")]), Ok(()));
        let four = [update("a"), update("b"), update("c"), update("d")];
        assert!(check_updates(&four).unwrap_err().contains("more than 3"));
        assert!(
            check_updates(&[update("a"), update("a")])
                .unwrap_err()
                .contains("twice")
        );
        assert!(check_updates(&[update("Bad Slug")]).unwrap_err().contains("slug"));
        let mut empty_body = update("a");
        empty_body.text.body = " ".into();
        assert!(check_updates(&[empty_body]).unwrap_err().contains("page `a`"));
    }

    #[test]
    fn filtered_text_has_no_personal_data() {
        let redactor = Redactor::new(&PrivacyConfig::default()).unwrap();
        let page = text(
            "Contact kim@example.com",
            "Call 010-1234-5678",
            "  Ask lee@example.com.  ",
        );
        let clean = page.filtered(&redactor);
        assert_eq!(clean.title, "Contact [REDACTED:EMAIL]");
        assert_eq!(clean.summary, "Call [REDACTED:PHONE]");
        assert_eq!(clean.body, "Ask [REDACTED:EMAIL].");
    }

    #[test]
    fn the_index_and_a_page_show_their_age_and_sources() {
        let summary = WikiSummary {
            slug: "deploy".into(),
            title: "Deploy schedule".into(),
            summary: "When we deploy".into(),
            revision: 2,
            updated_at: 1_000,
        };
        let now = 1_000 + 3 * 86_400 + 5;
        assert_eq!(
            index_text(&[summary], now),
            "deploy — Deploy schedule: When we deploy (updated 3 day(s) ago)"
        );
        assert_eq!(index_text(&[], now), "");

        let page = WikiPage {
            slug: "deploy".into(),
            text: text("Deploy schedule", "When we deploy", "Fridays at 3 pm."),
            sources: vec![WikiSource {
                origin: "slack/search".into(),
                detail: "{\"query\":\"deploy\"}".into(),
            }],
            revision: 2,
            updated_at: 1_000,
            author: WikiAuthor::Agent,
        };
        let shown = page_text(&page, now);
        assert!(shown.contains("Fridays at 3 pm."));
        assert!(shown.contains("Updated 3 day(s) ago."));
        assert!(shown.contains("- slack/search: {\"query\":\"deploy\"}"));
    }

    #[test]
    fn a_page_is_one_flat_json_object() {
        let update: WikiUpdate =
            serde_json::from_str(r#"{"slug": "a", "title": "T", "summary": "S", "body": "B"}"#).unwrap();
        assert_eq!(update.text.title, "T");
        assert!(serde_json::from_str::<WikiUpdate>(r#"{"slug": "a", "title": "T", "summary": "S"}"#).is_err());
    }
}
