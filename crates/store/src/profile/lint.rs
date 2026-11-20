//! The findings of the checks of the notes.

use super::ProfileStore;
use crate::{Result, StoreError};
use bunsin_core::lint::{FindingKind, FindingState, Found, LintFinding, LintPage};
use bunsin_core::wiki::WikiText;
use rusqlite::Row;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS lint_findings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    slugs TEXT NOT NULL,
    detail TEXT NOT NULL,
    state TEXT NOT NULL
);
";

type FindingRow = (i64, i64, String, String, String, String);

fn finding_from_row(row: &Row<'_>) -> rusqlite::Result<FindingRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn finding(raw: FindingRow) -> Result<LintFinding> {
    let (id, run_at, kind, slugs, detail, state) = raw;
    Ok(LintFinding {
        id,
        run_at,
        kind: FindingKind::parse(&kind).ok_or(StoreError::BadState(kind))?,
        slugs: serde_json::from_str(&slugs)?,
        detail,
        state: FindingState::parse(&state).ok_or(StoreError::BadState(state))?,
    })
}

impl ProfileStore {
    /// Every page with the times the checks need, in slug order.
    pub fn lint_pages(&self) -> Result<Vec<LintPage>> {
        let db = self.db();
        let mut statement =
            db.prepare("SELECT slug, title, summary, body, updated_at, last_read_at FROM wiki_pages ORDER BY slug")?;
        let rows = statement.query_map([], |row| {
            Ok(LintPage {
                slug: row.get(0)?,
                text: WikiText {
                    title: row.get(1)?,
                    summary: row.get(2)?,
                    body: row.get(3)?,
                },
                updated_at: row.get(4)?,
                last_read_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the new findings of earlier checks with `found`, in one transaction.
    /// Dismissed findings stay. Returns the stored findings in the order of `found`.
    pub fn replace_lint_findings(&self, run_at: i64, found: &[Found]) -> Result<Vec<LintFinding>> {
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM lint_findings WHERE state = 'new'", [])?;
        let mut stored = Vec::with_capacity(found.len());
        for item in found {
            tx.execute(
                "INSERT INTO lint_findings (run_at, kind, slugs, detail, state) VALUES (?1, ?2, ?3, ?4, 'new')",
                (
                    run_at,
                    item.kind.as_str(),
                    serde_json::to_string(&item.slugs)?,
                    &item.detail,
                ),
            )?;
            stored.push(LintFinding {
                id: tx.last_insert_rowid(),
                run_at,
                kind: item.kind,
                slugs: item.slugs.clone(),
                detail: item.detail.clone(),
                state: FindingState::New,
            });
        }
        tx.commit()?;
        Ok(stored)
    }

    /// Every stored finding, in the order the checks found them.
    pub fn lint_findings(&self) -> Result<Vec<LintFinding>> {
        let db = self.db();
        let mut statement =
            db.prepare("SELECT id, run_at, kind, slugs, detail, state FROM lint_findings ORDER BY id")?;
        let rows = statement
            .query_map([], finding_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(finding).collect()
    }

    /// Moves a new finding to dismissed. Returns false if it is not new.
    pub fn dismiss_lint_finding(&self, id: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE lint_findings SET state = 'dismissed' WHERE id = ?1 AND state = 'new'",
            [id],
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use crate::AppStore;
    use bunsin_core::lint::{FindingKind, FindingState, Found};
    use bunsin_core::wiki::{WikiAuthor, WikiText};

    fn found(kind: FindingKind, slugs: &[&str]) -> Found {
        Found {
            kind,
            slugs: slugs.iter().map(|s| (*s).to_owned()).collect(),
            detail: format!("{} detail", kind.as_str()),
        }
    }

    #[test]
    fn a_check_replaces_the_new_findings_and_keeps_the_dismissed_ones() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        let first = store
            .replace_lint_findings(
                10,
                &[
                    found(FindingKind::Stale, &["a"]),
                    found(FindingKind::Conflict, &["a", "b"]),
                ],
            )
            .unwrap();
        assert_eq!(first[1].slugs, ["a", "b"]);
        assert!(store.dismiss_lint_finding(first[0].id).unwrap());
        assert!(!store.dismiss_lint_finding(first[0].id).unwrap());
        assert!(!store.dismiss_lint_finding(999).unwrap());

        let second = store
            .replace_lint_findings(20, &[found(FindingKind::Orphan, &["c"])])
            .unwrap();
        let all = store.lint_findings().unwrap();
        // The new conflict of the first check is gone. The dismissed finding stays.
        assert_eq!(
            all.iter()
                .map(|f| (f.id, f.kind, f.state, f.run_at))
                .collect::<Vec<_>>(),
            [
                (first[0].id, FindingKind::Stale, FindingState::Dismissed, 10),
                (second[0].id, FindingKind::Orphan, FindingState::New, 20),
            ]
        );
    }

    #[test]
    fn the_checks_see_when_a_page_was_read() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        let text = WikiText {
            title: "T".into(),
            summary: "S".into(),
            body: "B".into(),
        };
        for slug in ["b", "a"] {
            store
                .write_wiki_page(slug, &text, &[], WikiAuthor::Owner, 0, None, 5)
                .unwrap();
        }
        store.mark_wiki_read("b", 9).unwrap();
        let pages = store.lint_pages().unwrap();
        assert_eq!(
            pages
                .iter()
                .map(|p| (p.slug.as_str(), p.updated_at, p.last_read_at))
                .collect::<Vec<_>>(),
            [("a", 5, None), ("b", 5, Some(9))]
        );
        assert_eq!(pages[0].text, text);
    }
}
