//! Checks of the agent's notes: pages that are old, pages that nothing names, and pairs of pages
//! that may say different things. A check only reports. It never changes a page.

use crate::search::query_terms;
use crate::wiki::WikiText;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The most pairs of pages one check sends to the LLM.
pub const MAX_CONFLICT_PAIRS: usize = 10;
/// The most characters of the reason the LLM gives for a conflict.
pub const MAX_DETAIL_CHARS: usize = 300;
const DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// Nobody changed the page for more than `stale_days` days.
    Stale,
    /// No other page names the page, and the agent did not read it for more than `stale_days` days.
    Orphan,
    /// The LLM found that two pages say different things.
    Conflict,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingKind::Stale => "stale",
            FindingKind::Orphan => "orphan",
            FindingKind::Conflict => "conflict",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [FindingKind::Stale, FindingKind::Orphan, FindingKind::Conflict]
            .into_iter()
            .find(|kind| kind.as_str() == text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingState {
    New,
    /// The owner dismissed it. A later check does not report the same kind for the same pages again.
    Dismissed,
}

impl FindingState {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingState::New => "new",
            FindingState::Dismissed => "dismissed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [FindingState::New, FindingState::Dismissed]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

/// What a check found, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub kind: FindingKind,
    /// One page, or the two pages of a conflict in slug order.
    pub slugs: Vec<String>,
    pub detail: String,
}

/// A stored finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    pub id: i64,
    /// Unix seconds of the check that found it.
    pub run_at: i64,
    pub kind: FindingKind,
    pub slugs: Vec<String>,
    pub detail: String,
    pub state: FindingState,
}

/// A page as the checks see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintPage {
    pub slug: String,
    pub text: WikiText,
    pub updated_at: i64,
    /// When the agent last read the page. `None` if it never did.
    pub last_read_at: Option<i64>,
}

/// True if `body` holds `slug` as a whole word: no letter, digit or `-` touches it on either side.
pub fn names_page(body: &str, slug: &str) -> bool {
    let is_part = |c: char| c.is_ascii_alphanumeric() || c == '-';
    body.match_indices(slug).any(|(start, _)| {
        let before = body[..start].chars().next_back();
        let after = body[start + slug.len()..].chars().next();
        !before.is_some_and(is_part) && !after.is_some_and(is_part)
    })
}

/// The stale and orphan pages. A page is stale if nobody changed it for more than `stale_days` days.
/// It is an orphan if no other page names it in its text and the agent did not read it for more than
/// `stale_days` days. A page that was never read counts from its last change.
pub fn stale_and_orphans(pages: &[LintPage], stale_days: u32, now: i64) -> Vec<Found> {
    let limit = i64::from(stale_days) * DAY;
    let mut found = Vec::new();
    for page in pages {
        let unchanged = now - page.updated_at;
        if unchanged > limit {
            found.push(Found {
                kind: FindingKind::Stale,
                slugs: vec![page.slug.clone()],
                detail: format!("nobody changed it for {} day(s)", unchanged / DAY),
            });
        }
        let named = pages.iter().any(|other| {
            other.slug != page.slug
                && [&other.text.title, &other.text.summary, &other.text.body]
                    .iter()
                    .any(|text| names_page(text, &page.slug))
        });
        let unread = now - page.last_read_at.unwrap_or(page.updated_at);
        if !named && unread > limit {
            let read = match page.last_read_at {
                Some(_) => format!("the agent did not read it for {} day(s)", unread / DAY),
                None => "the agent never read it".to_owned(),
            };
            found.push(Found {
                kind: FindingKind::Orphan,
                slugs: vec![page.slug.clone()],
                detail: format!("no other page names it, and {read}"),
            });
        }
    }
    found
}

/// The pairs of pages that share two or more rare search terms, most shared terms first, at most
/// `MAX_CONFLICT_PAIRS`. A term is rare if at most `max(2, pages / 4)` pages have it, as in
/// `search::rank`. A pair in `skip` is left out. Each pair is in slug order.
pub fn conflict_pairs(pages: &[LintPage], skip: &BTreeSet<(String, String)>) -> Vec<(String, String)> {
    let terms: Vec<BTreeSet<String>> = pages
        .iter()
        .map(|page| {
            let text = format!("{}\n{}\n{}", page.text.title, page.text.summary, page.text.body);
            query_terms(&text).into_iter().collect()
        })
        .collect();
    let mut df: BTreeMap<&str, usize> = BTreeMap::new();
    for term in terms.iter().flatten() {
        *df.entry(term).or_default() += 1;
    }
    let rare_limit = (pages.len() / 4).max(2);
    let mut pairs = Vec::new();
    for (i, a) in pages.iter().enumerate() {
        for (j, b) in pages.iter().enumerate().skip(i + 1) {
            let shared = terms[i]
                .intersection(&terms[j])
                .filter(|term| df[term.as_str()] <= rare_limit)
                .count();
            let pair = ordered(&a.slug, &b.slug);
            if shared >= 2 && !skip.contains(&pair) {
                pairs.push((shared, pair));
            }
        }
    }
    pairs.sort_by(|(a_shared, a), (b_shared, b)| b_shared.cmp(a_shared).then_with(|| a.cmp(b)));
    pairs
        .into_iter()
        .take(MAX_CONFLICT_PAIRS)
        .map(|(_, pair)| pair)
        .collect()
}

fn ordered(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

/// The reply form of the conflict check.
pub fn conflicts_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["conflicts"],
        "properties": {
            "conflicts": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["a", "b", "detail"],
                    "properties": {
                        "a": {"type": "string"},
                        "b": {"type": "string"},
                        "detail": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// The LLM's reply to the conflict check.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConflictReply {
    pub conflicts: Vec<ConflictClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ConflictClaim {
    pub a: String,
    pub b: String,
    pub detail: String,
}

/// The conflicts of the reply that name a pair that was sent, once per pair. A claim about any other
/// pair is dropped: the LLM cannot add pages to the check. The detail is cut to `MAX_DETAIL_CHARS`.
pub fn accept_conflicts(reply: ConflictReply, sent: &[(String, String)]) -> Vec<Found> {
    let mut found: Vec<Found> = Vec::new();
    for claim in reply.conflicts {
        let (a, b) = ordered(claim.a.trim(), claim.b.trim());
        let known = sent.iter().any(|pair| pair.0 == a && pair.1 == b);
        let repeated = found.iter().any(|f| f.slugs[0] == a && f.slugs[1] == b);
        if !known || repeated {
            continue;
        }
        found.push(Found {
            kind: FindingKind::Conflict,
            slugs: vec![a, b],
            detail: claim.detail.trim().chars().take(MAX_DETAIL_CHARS).collect(),
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(slug: &str, body: &str, updated_at: i64, last_read_at: Option<i64>) -> LintPage {
        LintPage {
            slug: slug.into(),
            text: WikiText {
                title: slug.to_uppercase(),
                summary: "s".into(),
                body: body.into(),
            },
            updated_at,
            last_read_at,
        }
    }

    fn kinds(found: &[Found]) -> Vec<(FindingKind, &str)> {
        found.iter().map(|f| (f.kind, f.slugs[0].as_str())).collect()
    }

    #[test]
    fn a_slug_counts_only_as_a_whole_word() {
        assert!(names_page("See deploy for the times.", "deploy"));
        assert!(names_page("deploy", "deploy"));
        assert!(names_page("(deploy)", "deploy"));
        assert!(!names_page("See deploy-schedule.", "deploy"));
        assert!(!names_page("redeploy", "deploy"));
        assert!(!names_page("deploys", "deploy"));
        // A later match counts after an earlier one that is part of a longer word.
        assert!(names_page("deploys and deploy", "deploy"));
        assert!(names_page("배포 일정은 deploy 쪽", "deploy"));
    }

    #[test]
    fn a_page_is_stale_only_after_more_than_the_days() {
        let now = 100 * DAY;
        let limit = 10 * DAY;
        let pages = [
            page("at-limit", "", now - limit, Some(now)),
            page("past-limit", "", now - limit - 1, Some(now)),
        ];
        let found = stale_and_orphans(&pages, 10, now);
        assert_eq!(kinds(&found), [(FindingKind::Stale, "past-limit")]);
        assert_eq!(found[0].detail, "nobody changed it for 10 day(s)");
    }

    #[test]
    fn an_orphan_is_named_by_no_other_page_and_not_read_for_the_days() {
        let now = 100 * DAY;
        let old = now - 11 * DAY;
        let pages = [
            // Named by `hub`, so not an orphan, though nobody read it.
            page("named", "", now, None),
            // Names itself only.
            page("self", "see self", now, Some(old)),
            page("hub", "see named", now, Some(now)),
            // Read at the limit: not yet an orphan.
            page("read-at-limit", "", now, Some(now - 10 * DAY)),
            // Never read: it counts from its last change.
            page("never-read-old", "", old, None),
            page("never-read-new", "", now, None),
        ];
        let found = stale_and_orphans(&pages, 10, now);
        assert_eq!(
            kinds(&found),
            [
                (FindingKind::Orphan, "self"),
                (FindingKind::Stale, "never-read-old"),
                (FindingKind::Orphan, "never-read-old"),
            ]
        );
        assert!(found[0].detail.contains("did not read it for 11 day(s)"));
        assert!(found[2].detail.contains("never read it"));
    }

    #[test]
    fn pairs_share_two_rare_terms_and_skip_the_dismissed_ones() {
        // With 12 pages a term is rare on at most 3 pages. `deploy` is on every page, so it is common.
        let mut pages: Vec<LintPage> = (0..12)
            .map(|n| page(&format!("p{n:02}"), &format!("deploy topic{n}"), 0, None))
            .collect();
        pages[1].text.body = "deploy friday release".into();
        pages[5].text.body = "deploy friday release window".into();
        pages[6].text.body = "deploy friday release window".into();
        pages[7].text.body = "deploy window".into();
        let pairs = conflict_pairs(&pages, &BTreeSet::new());
        let pair = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        // p05 and p06 share three rare terms, p01 shares two with each. p07 shares only `window`.
        assert_eq!(pairs, [pair("p05", "p06"), pair("p01", "p05"), pair("p01", "p06")]);
        let skip = BTreeSet::from([pair("p05", "p06")]);
        assert_eq!(conflict_pairs(&pages, &skip), [pair("p01", "p05"), pair("p01", "p06")]);
    }

    #[test]
    fn at_most_ten_pairs_are_sent() {
        let pages: Vec<LintPage> = (0..6)
            .map(|n| page(&format!("p{n}"), "alpha bravo charlie", 0, None))
            .collect();
        // Six pages share the terms, but with six pages a term on all six is common.
        assert!(conflict_pairs(&pages, &BTreeSet::new()).is_empty());
        let mut many: Vec<LintPage> = (0..40)
            .map(|n| page(&format!("p{n:02}"), &format!("unique{n}"), 0, None))
            .collect();
        for page in many.iter_mut().take(6) {
            page.text.body.push_str(" alpha bravo");
        }
        assert_eq!(conflict_pairs(&many, &BTreeSet::new()).len(), MAX_CONFLICT_PAIRS);
    }

    #[test]
    fn only_conflicts_about_sent_pairs_are_accepted() {
        let sent = vec![("a".to_owned(), "b".to_owned())];
        let claim = |a: &str, b: &str, detail: &str| ConflictClaim {
            a: a.into(),
            b: b.into(),
            detail: detail.into(),
        };
        let reply = ConflictReply {
            conflicts: vec![
                claim("b", "a", &"x".repeat(400)),
                claim("a", "b", "twice"),
                claim("a", "c", "not sent"),
                claim("c", "d", "not sent"),
            ],
        };
        let found = accept_conflicts(reply, &sent);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slugs, ["a", "b"]);
        assert_eq!(found[0].detail.chars().count(), MAX_DETAIL_CHARS);
    }
}
