mod intake;

pub use intake::IntakeRow;
mod learning;
mod lint;

use crate::{Result, StoreError};
use bunsin_core::approval::ApprovalState;
use bunsin_core::context::Correction;
use bunsin_core::event::InboundEvent;
use bunsin_core::records::{
    ArtifactMeta, AuditEntry, CorrectionRecord, DecisionCounts, DecisionKind, InboxBody, InboxItem, LlmUsage, ReadNote,
};
use bunsin_core::search::{TermQuery, rank, term_query};
use bunsin_core::sha256_hex;
use bunsin_core::wiki::{WikiAuthor, WikiPage, WikiSource, WikiSummary, WikiText};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    dedupe_key TEXT PRIMARY KEY,
    received_at INTEGER NOT NULL,
    payload TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'done',
    attempts INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS inbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_key TEXT NOT NULL,
    body TEXT NOT NULL,
    rationale TEXT NOT NULL,
    basis TEXT NOT NULL DEFAULT '[]',
    state TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    autonomous INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER
);
CREATE TABLE IF NOT EXISTS corrections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    inbox_id INTEGER NOT NULL,
    original TEXT NOT NULL,
    edited TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    tool TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT '',
    task TEXT NOT NULL DEFAULT '',
    retired_at INTEGER
);
CREATE TABLE IF NOT EXISTS artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    event_key TEXT,
    inbox_id INTEGER
);
CREATE TABLE IF NOT EXISTS secrets (
    key TEXT PRIMARY KEY,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    origin TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS wiki_pages (
    slug TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    summary TEXT NOT NULL,
    body TEXT NOT NULL,
    sources TEXT NOT NULL,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    author TEXT NOT NULL,
    last_read_at INTEGER
);
CREATE TABLE IF NOT EXISTS wiki_revisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    slug TEXT NOT NULL,
    revision INTEGER NOT NULL,
    page TEXT,
    inbox_id INTEGER,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    event_key TEXT,
    detail TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS llm_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    job TEXT,
    purpose TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    outcome TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    inbox_id INTEGER,
    tool TEXT NOT NULL,
    kind TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS decisions_tool ON decisions (tool, kind);
CREATE TRIGGER IF NOT EXISTS decisions_no_update BEFORE UPDATE ON decisions
BEGIN SELECT RAISE(ABORT, 'the decisions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS decisions_no_delete BEFORE DELETE ON decisions
BEGIN SELECT RAISE(ABORT, 'the decisions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_update BEFORE UPDATE ON audit
BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_delete BEFORE DELETE ON audit
BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
";

/// The full-text index of the corrections. It reads its text from `corrections` by `id`.
/// A correction is never changed or deleted: retiring it sets `retired_at`, which is not indexed.
const CORRECTIONS_FTS: &str = "
CREATE VIRTUAL TABLE corrections_fts USING fts5(
    task, original, edited, reason, content = 'corrections', content_rowid = 'id', tokenize = 'trigram'
);
CREATE TRIGGER corrections_fts_insert AFTER INSERT ON corrections BEGIN
    INSERT INTO corrections_fts (rowid, task, original, edited, reason)
    VALUES (new.id, new.task, new.original, new.edited, new.reason);
END;
INSERT INTO corrections_fts (corrections_fts) VALUES ('rebuild');
";

/// The full-text index of the notes. It holds its own copy of the text, keyed by slug, because
/// an external-content index needs an integer key and `wiki_pages` is keyed by slug.
/// The triggers keep it in step with inserts, UPSERTs and deletes.
const WIKI_FTS: &str = "
CREATE VIRTUAL TABLE wiki_fts USING fts5(slug UNINDEXED, title, summary, body, tokenize = 'trigram');
CREATE TRIGGER wiki_fts_insert AFTER INSERT ON wiki_pages BEGIN
    INSERT INTO wiki_fts (slug, title, summary, body) VALUES (new.slug, new.title, new.summary, new.body);
END;
CREATE TRIGGER wiki_fts_update AFTER UPDATE OF title, summary, body ON wiki_pages BEGIN
    DELETE FROM wiki_fts WHERE slug = old.slug;
    INSERT INTO wiki_fts (slug, title, summary, body) VALUES (new.slug, new.title, new.summary, new.body);
END;
CREATE TRIGGER wiki_fts_delete AFTER DELETE ON wiki_pages BEGIN
    DELETE FROM wiki_fts WHERE slug = old.slug;
END;
INSERT INTO wiki_fts (slug, title, summary, body) SELECT slug, title, summary, body FROM wiki_pages;
";

const KILL_SWITCH: &str = "kill_switch";

fn insert_decision(db: &Connection, at: i64, inbox_id: Option<i64>, tool: &str, kind: DecisionKind) -> Result<()> {
    db.execute(
        "INSERT INTO decisions (at, inbox_id, tool, kind) VALUES (?1, ?2, ?3, ?4)",
        (at, inbox_id, tool, kind.as_str()),
    )?;
    Ok(())
}

/// True if the last hold or clearing of the tool is a hold.
fn is_held(db: &Connection, tool: &str) -> Result<bool> {
    let last: Option<String> = db
        .query_row(
            "SELECT kind FROM decisions WHERE tool = ?1 AND kind IN ('demoted', 'hold_cleared')
             ORDER BY id DESC LIMIT 1",
            [tool],
            |row| row.get(0),
        )
        .optional()?;
    Ok(last.as_deref() == Some(DecisionKind::Demoted.as_str()))
}

/// Creates each full-text index that is missing and fills it from the rows that exist.
/// One transaction holds the check and the fill, so an index is never half made.
fn create_search_indexes(db: &mut Connection) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for (name, sql) in [("corrections_fts", CORRECTIONS_FTS), ("wiki_fts", WIKI_FTS)] {
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )?;
        if !exists {
            tx.execute_batch(sql)?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// For each term, the keys of the rows that match it. `match_sql` and `like_sql` take the
/// argument of `search::term_query` as `?1`.
fn term_hits<K: FromSql + Ord>(
    db: &Connection,
    terms: &[String],
    match_sql: &str,
    like_sql: &str,
) -> Result<Vec<BTreeSet<K>>> {
    terms
        .iter()
        .map(|term| {
            let (sql, arg) = match term_query(term) {
                TermQuery::Match(phrase) => (match_sql, phrase),
                TermQuery::Like(pattern) => (like_sql, pattern),
            };
            let mut statement = db.prepare_cached(sql)?;
            let rows = statement.query_map([arg], |row| row.get(0))?;
            Ok(rows.collect::<rusqlite::Result<BTreeSet<K>>>()?)
        })
        .collect()
}

/// A secret as it is stored: encrypted, with the address it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSecret {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    /// The scheme, host and port the secret was entered for.
    pub origin: String,
}

/// One call to the LLM. It holds no text of the request or the reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCall {
    /// Unix seconds.
    pub at: i64,
    /// The work the call was for, for example the key of an event. `None` for a connection test.
    pub job: Option<String>,
    pub purpose: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    /// How the call ended: `end`, `max_tokens`, `refusal`, or the kind of error.
    pub outcome: String,
}

/// What a change to a page did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WikiWrite {
    Stored(WikiPage),
    /// The page is not at the revision the change was written against. Nothing was stored.
    RevisionMismatch {
        current: u64,
    },
    /// The inbox item of the change is not pending any more. Nothing was stored.
    ItemHandled,
}

fn author_name(author: WikiAuthor) -> &'static str {
    match author {
        WikiAuthor::Agent => "agent",
        WikiAuthor::Owner => "owner",
    }
}

type PageRow = (String, String, String, String, String, i64, i64, String);

fn page_from_row(row: &Row<'_>) -> rusqlite::Result<PageRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn page(raw: PageRow) -> Result<WikiPage> {
    let (slug, title, summary, body, sources, revision, updated_at, author) = raw;
    Ok(WikiPage {
        slug,
        text: WikiText { title, summary, body },
        sources: serde_json::from_str(&sources)?,
        // SQLite stores signed integers. A revision is never negative.
        revision: revision as u64,
        updated_at,
        author: if author == "owner" {
            WikiAuthor::Owner
        } else {
            WikiAuthor::Agent
        },
    })
}

const PAGE_COLUMNS: &str = "slug, title, summary, body, sources, revision, updated_at, author";

/// What `begin_event` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStart {
    /// The event is new. This is its first try.
    New,
    /// An earlier try did not finish. This is try number `attempt`.
    Retry { attempt: u32 },
    /// The event was handled before.
    Done,
    /// The event used all its tries. It is closed now.
    GaveUp,
}

/// The private store of one profile.
pub struct ProfileStore {
    dir: PathBuf,
    db: Mutex<Connection>,
}

type InboxRow = (i64, String, String, String, String, i64, i64, String, bool, Option<i64>);

fn inbox_from_row(row: &Row<'_>) -> rusqlite::Result<InboxRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
    ))
}

fn inbox_item(raw: InboxRow) -> Result<InboxItem> {
    let (id, event_key, body, rationale, state, created_at, updated_at, basis, autonomous, retry_of) = raw;
    Ok(InboxItem {
        id,
        event_key,
        body: serde_json::from_str(&body)?,
        rationale,
        basis: serde_json::from_str(&basis)?,
        state: ApprovalState::parse(&state).ok_or(StoreError::BadState(state))?,
        created_at,
        updated_at,
        autonomous,
        retry_of,
    })
}

const INBOX_COLUMNS: &str =
    "id, event_key, body, rationale, state, created_at, updated_at, basis, autonomous, retry_of";

impl ProfileStore {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir.join("artifacts").join("sha256"))?;
        let mut db = Connection::open(dir.join("agent.db"))?;
        db.busy_timeout(crate::BUSY_TIMEOUT)?;
        db.execute_batch(SCHEMA)?;
        db.execute_batch(learning::SCHEMA)?;
        db.execute_batch(lint::SCHEMA)?;
        db.execute_batch(intake::SCHEMA)?;
        // A database from before events could be retried lacks two columns.
        let has_status: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('events') WHERE name = 'status')",
            [],
            |row| row.get(0),
        )?;
        let has_basis: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('inbox') WHERE name = 'basis')",
            [],
            |row| row.get(0),
        )?;
        if !has_basis {
            db.execute_batch("ALTER TABLE inbox ADD COLUMN basis TEXT NOT NULL DEFAULT '[]';")?;
        }
        if !has_status {
            db.execute_batch(
                "ALTER TABLE events ADD COLUMN status TEXT NOT NULL DEFAULT 'done';
                 ALTER TABLE events ADD COLUMN attempts INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
        let has_autonomous: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('inbox') WHERE name = 'autonomous')",
            [],
            |row| row.get(0),
        )?;
        if !has_autonomous {
            // An older database does not say who approved an item. An approved item from it counts
            // as approved by the policy, so a resume runs it only if the policy still allows that
            // without the owner. Otherwise the owner is asked again.
            db.execute_batch(
                "BEGIN;
                 ALTER TABLE inbox ADD COLUMN autonomous INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE inbox ADD COLUMN retry_of INTEGER;
                 UPDATE inbox SET autonomous = 1 WHERE state = 'approved';
                 COMMIT;",
            )?;
        }
        let has_task: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('corrections') WHERE name = 'task')",
            [],
            |row| row.get(0),
        )?;
        if !has_task {
            // An older correction has no tool, source or task. It is still found by its texts.
            db.execute_batch(
                "BEGIN;
                 ALTER TABLE corrections ADD COLUMN tool TEXT NOT NULL DEFAULT '';
                 ALTER TABLE corrections ADD COLUMN source TEXT NOT NULL DEFAULT '';
                 ALTER TABLE corrections ADD COLUMN task TEXT NOT NULL DEFAULT '';
                 ALTER TABLE corrections ADD COLUMN retired_at INTEGER;
                 COMMIT;",
            )?;
        }
        let has_last_read: bool = db.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('wiki_pages') WHERE name = 'last_read_at')",
            [],
            |row| row.get(0),
        )?;
        if !has_last_read {
            db.execute_batch("ALTER TABLE wiki_pages ADD COLUMN last_read_at INTEGER;")?;
        }
        create_search_indexes(&mut db)?;
        Ok(Self {
            dir: dir.to_owned(),
            db: Mutex::new(db),
        })
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().expect("store lock")
    }

    // --- settings ---

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| row.get(0))
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// Replaces a setting only if it still has the value `old`. Returns false otherwise,
    /// so two changes that started from the same value cannot both be stored.
    pub fn replace_setting_if(&self, key: &str, old: &str, new: &str) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE settings SET value = ?1 WHERE key = ?2 AND value = ?3",
            (new, key, old),
        )?;
        Ok(changed == 1)
    }

    /// The kill switch is stored, so it stays on after a restart.
    pub fn kill_switch(&self) -> Result<bool> {
        Ok(self.setting(KILL_SWITCH)?.as_deref() == Some("on"))
    }

    pub fn set_kill_switch(&self, on: bool) -> Result<()> {
        self.set_setting(KILL_SWITCH, if on { "on" } else { "off" })
    }

    // --- events ---

    /// Starts the handling of an event and says which try this is.
    ///
    /// An event stays open until `finish_event`. An open event may be started again,
    /// up to `max_attempts` times in total. After that it is closed and `GaveUp` is
    /// returned once.
    pub fn begin_event(&self, dedupe_key: &str, payload: &Value, now: i64, max_attempts: u32) -> Result<EventStart> {
        let db = self.db();
        let inserted = db.execute(
            "INSERT OR IGNORE INTO events (dedupe_key, received_at, payload, status, attempts)
             VALUES (?1, ?2, ?3, 'open', 1)",
            (dedupe_key, now, payload.to_string()),
        )?;
        if inserted == 1 {
            return Ok(EventStart::New);
        }
        let (status, attempts): (String, u32) = db.query_row(
            "SELECT status, attempts FROM events WHERE dedupe_key = ?1",
            [dedupe_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if status != "open" {
            return Ok(EventStart::Done);
        }
        if attempts >= max_attempts {
            db.execute("UPDATE events SET status = 'done' WHERE dedupe_key = ?1", [dedupe_key])?;
            return Ok(EventStart::GaveUp);
        }
        db.execute(
            "UPDATE events SET attempts = attempts + 1 WHERE dedupe_key = ?1",
            [dedupe_key],
        )?;
        Ok(EventStart::Retry { attempt: attempts + 1 })
    }

    /// The stored form of an event: after the privacy filter. `None` if no event has this key.
    pub fn event(&self, dedupe_key: &str) -> Result<Option<InboundEvent>> {
        let payload: Option<String> = self
            .db()
            .query_row(
                "SELECT payload FROM events WHERE dedupe_key = ?1",
                [dedupe_key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(payload.map(|text| serde_json::from_str(&text)).transpose()?)
    }

    /// Closes an event. It is not handled again.
    pub fn finish_event(&self, dedupe_key: &str) -> Result<()> {
        self.db()
            .execute("UPDATE events SET status = 'done' WHERE dedupe_key = ?1", [dedupe_key])?;
        Ok(())
    }

    /// Records an event as already handled, without handling it.
    /// Returns false if an event with this key was recorded before.
    pub fn mark_event_seen(&self, dedupe_key: &str, payload: &Value, now: i64) -> Result<bool> {
        let inserted = self.db().execute(
            "INSERT OR IGNORE INTO events (dedupe_key, received_at, payload, status, attempts)
             VALUES (?1, ?2, ?3, 'done', 0)",
            (dedupe_key, now, payload.to_string()),
        )?;
        Ok(inserted == 1)
    }

    // --- secrets ---

    /// Stores an encrypted secret, replacing an earlier one with the same key.
    pub fn put_secret(&self, key: &str, secret: &SealedSecret, now: i64) -> Result<()> {
        self.db().execute(
            "INSERT INTO secrets (key, nonce, ciphertext, origin, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (key) DO UPDATE SET nonce = excluded.nonce, ciphertext = excluded.ciphertext,
                 origin = excluded.origin, updated_at = excluded.updated_at",
            (key, &secret.nonce, &secret.ciphertext, &secret.origin, now),
        )?;
        Ok(())
    }

    pub fn secret(&self, key: &str) -> Result<Option<SealedSecret>> {
        Ok(self
            .db()
            .query_row(
                "SELECT nonce, ciphertext, origin FROM secrets WHERE key = ?1",
                [key],
                |row| {
                    Ok(SealedSecret {
                        nonce: row.get(0)?,
                        ciphertext: row.get(1)?,
                        origin: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Returns true if a secret with this key existed.
    pub fn delete_secret(&self, key: &str) -> Result<bool> {
        Ok(self.db().execute("DELETE FROM secrets WHERE key = ?1", [key])? == 1)
    }

    pub fn secret_keys(&self) -> Result<Vec<String>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT key FROM secrets ORDER BY key")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- wiki ---

    /// One line per page, in slug order.
    pub fn wiki_index(&self) -> Result<Vec<WikiSummary>> {
        let db = self.db();
        let mut statement =
            db.prepare("SELECT slug, title, summary, revision, updated_at FROM wiki_pages ORDER BY slug")?;
        let rows = statement.query_map([], |row| {
            Ok(WikiSummary {
                slug: row.get(0)?,
                title: row.get(1)?,
                summary: row.get(2)?,
                revision: row.get::<_, i64>(3)? as u64,
                updated_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn wiki_page(&self, slug: &str) -> Result<Option<WikiPage>> {
        let raw = self
            .db()
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM wiki_pages WHERE slug = ?1"),
                [slug],
                page_from_row,
            )
            .optional()?;
        raw.map(page).transpose()
    }

    /// Stores a page if it is at `base_revision` (0 means "the page does not exist").
    ///
    /// With `inbox_id`, the inbox item moves from pending to executed in the same
    /// transaction, so an approved change is applied exactly once. Every stored change
    /// also writes a history row.
    #[allow(clippy::too_many_arguments)]
    pub fn write_wiki_page(
        &self,
        slug: &str,
        text: &WikiText,
        sources: &[WikiSource],
        author: WikiAuthor,
        base_revision: u64,
        inbox_id: Option<i64>,
        now: i64,
    ) -> Result<WikiWrite> {
        let mut db = self.db();
        let tx = db.transaction()?;
        if let Some(id) = inbox_id {
            let moved = tx.execute(
                "UPDATE inbox SET state = 'executed', updated_at = ?1 WHERE id = ?2 AND state = 'pending'",
                (now, id),
            )?;
            if moved != 1 {
                return Ok(WikiWrite::ItemHandled);
            }
        }
        let current = tx
            .query_row("SELECT revision FROM wiki_pages WHERE slug = ?1", [slug], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?
            .unwrap_or(0) as u64;
        if current != base_revision {
            // Dropping the transaction undoes the inbox move.
            return Ok(WikiWrite::RevisionMismatch { current });
        }
        let stored = WikiPage {
            slug: slug.to_owned(),
            text: text.clone(),
            sources: sources.to_vec(),
            revision: current + 1,
            updated_at: now,
            author,
        };
        tx.execute(
            "INSERT INTO wiki_pages (slug, title, summary, body, sources, revision, updated_at, author)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (slug) DO UPDATE SET title = excluded.title, summary = excluded.summary,
                 body = excluded.body, sources = excluded.sources, revision = excluded.revision,
                 updated_at = excluded.updated_at, author = excluded.author",
            (
                slug,
                &text.title,
                &text.summary,
                &text.body,
                serde_json::to_string(sources)?,
                stored.revision as i64,
                now,
                author_name(author),
            ),
        )?;
        tx.execute(
            "INSERT INTO wiki_revisions (slug, revision, page, inbox_id, at) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                slug,
                stored.revision as i64,
                serde_json::to_string(&stored)?,
                inbox_id,
                now,
            ),
        )?;
        tx.commit()?;
        Ok(WikiWrite::Stored(stored))
    }

    /// Deletes a page. The history keeps its earlier revisions and a row for the deletion.
    /// Returns false if there was no such page.
    pub fn delete_wiki_page(&self, slug: &str, now: i64) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let revision: Option<i64> = tx
            .query_row("SELECT revision FROM wiki_pages WHERE slug = ?1", [slug], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(revision) = revision else { return Ok(false) };
        tx.execute("DELETE FROM wiki_pages WHERE slug = ?1", [slug])?;
        tx.execute(
            "INSERT INTO wiki_revisions (slug, revision, page, inbox_id, at) VALUES (?1, ?2, NULL, NULL, ?3)",
            (slug, revision + 1, now),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The stored revisions of a page, oldest first. A deletion is `None`.
    pub fn wiki_history(&self, slug: &str) -> Result<Vec<Option<WikiPage>>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT page FROM wiki_revisions WHERE slug = ?1 ORDER BY id")?;
        let rows = statement
            .query_map([slug], |row| row.get::<_, Option<String>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|text| text.map(|t| Ok(serde_json::from_str(&t)?)).transpose())
            .collect()
    }

    /// Records that the agent read a page now. A change of the page keeps the time.
    pub fn mark_wiki_read(&self, slug: &str, now: i64) -> Result<()> {
        self.db()
            .execute("UPDATE wiki_pages SET last_read_at = ?1 WHERE slug = ?2", (now, slug))?;
        Ok(())
    }

    /// The slugs of the pages that match the terms, best first. The terms come from
    /// `search::query_terms`. Of two equal matches, the page changed last comes first.
    pub fn search_wiki(&self, terms: &[String]) -> Result<Vec<String>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT slug, updated_at FROM wiki_pages")?;
        let updated = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
        let hits: Vec<BTreeSet<String>> = term_hits(
            &db,
            terms,
            "SELECT slug FROM wiki_fts WHERE wiki_fts MATCH ?1",
            "SELECT slug FROM wiki_pages WHERE title LIKE ?1 OR summary LIKE ?1 OR body LIKE ?1",
        )?;
        Ok(rank(updated.len(), &hits, |slug| {
            (Reverse(updated[slug]), slug.clone())
        }))
    }

    /// True if a pending inbox item proposes a change to this page.
    pub fn has_pending_wiki_change(&self, slug: &str) -> Result<bool> {
        let db = self.db();
        let mut statement = db.prepare("SELECT body FROM inbox WHERE state = 'pending'")?;
        let bodies = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for body in bodies {
            if let InboxBody::WikiChange { slug: pending, .. } = serde_json::from_str(&body)?
                && pending == slug
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // --- inbox ---

    /// Adds an item. An item that starts `approved` was approved by the policy, not by the
    /// owner, so it is marked autonomous: the owner approves only by moving a pending item.
    pub fn add_inbox_item(
        &self,
        event_key: &str,
        body: &InboxBody,
        rationale: &str,
        basis: &[ReadNote],
        state: ApprovalState,
        now: i64,
    ) -> Result<InboxItem> {
        let autonomous = state == ApprovalState::Approved;
        let db = self.db();
        db.execute(
            "INSERT INTO inbox (event_key, body, rationale, basis, state, created_at, updated_at, autonomous)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            (
                event_key,
                serde_json::to_string(body)?,
                rationale,
                serde_json::to_string(basis)?,
                state.as_str(),
                now,
                autonomous,
            ),
        )?;
        Ok(InboxItem {
            id: db.last_insert_rowid(),
            event_key: event_key.to_owned(),
            body: body.clone(),
            rationale: rationale.to_owned(),
            basis: basis.to_vec(),
            state,
            created_at: now,
            updated_at: now,
            autonomous,
            retry_of: None,
        })
    }

    /// The items in `state`, oldest first.
    pub fn inbox_in_state(&self, state: ApprovalState) -> Result<Vec<InboxItem>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {INBOX_COLUMNS} FROM inbox WHERE state = ?1 ORDER BY id"
        ))?;
        let rows = statement
            .query_map([state.as_str()], inbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(inbox_item).collect()
    }

    /// Moves an item from `from` to `failed` and adds a pending copy of it that names it in
    /// `retry_of`, in one transaction. Returns the copy, or `None` if the item is not in `from`:
    /// of two callers, only the one that moves the item gets a copy.
    pub fn fail_with_pending_copy(&self, id: i64, from: ApprovalState, now: i64) -> Result<Option<InboxItem>> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let moved = tx.execute(
            "UPDATE inbox SET state = 'failed', updated_at = ?1 WHERE id = ?2 AND state = ?3",
            (now, id, from.as_str()),
        )?;
        if moved != 1 {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO inbox (event_key, body, rationale, basis, state, created_at, updated_at, autonomous, retry_of)
             SELECT event_key, body, rationale, basis, 'pending', ?1, ?1, 0, id FROM inbox WHERE id = ?2",
            (now, id),
        )?;
        let copy = tx.last_insert_rowid();
        let raw = tx.query_row(
            &format!("SELECT {INBOX_COLUMNS} FROM inbox WHERE id = ?1"),
            [copy],
            inbox_from_row,
        )?;
        tx.commit()?;
        inbox_item(raw).map(Some)
    }

    pub fn inbox_item(&self, id: i64) -> Result<InboxItem> {
        let raw = self
            .db()
            .query_row(
                &format!("SELECT {INBOX_COLUMNS} FROM inbox WHERE id = ?1"),
                [id],
                inbox_from_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound("inbox item", id))?;
        inbox_item(raw)
    }

    /// Newest first.
    pub fn list_inbox(&self, limit: u32) -> Result<Vec<InboxItem>> {
        let db = self.db();
        let mut statement = db.prepare(&format!("SELECT {INBOX_COLUMNS} FROM inbox ORDER BY id DESC LIMIT ?1"))?;
        let rows = statement
            .query_map([limit], inbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(inbox_item).collect()
    }

    /// Moves an item from `from` to `to`. Returns false if the item is not in `from`,
    /// so only one of two concurrent callers wins.
    pub fn move_inbox_state(&self, id: i64, from: ApprovalState, to: ApprovalState, now: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE inbox SET state = ?1, updated_at = ?2 WHERE id = ?3 AND state = ?4",
            (to.as_str(), now, id, from.as_str()),
        )?;
        Ok(changed == 1)
    }

    /// Replaces the body of an item that is still pending. Returns false otherwise.
    pub fn replace_pending_body(&self, id: i64, body: &InboxBody, now: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE inbox SET body = ?1, updated_at = ?2 WHERE id = ?3 AND state = 'pending'",
            (serde_json::to_string(body)?, now, id),
        )?;
        Ok(changed == 1)
    }

    // --- corrections ---

    /// Stores a correction and returns its ID.
    pub fn add_correction(&self, inbox_id: i64, correction: &Correction, now: i64) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO corrections (inbox_id, original, edited, reason, created_at, tool, source, task)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                inbox_id,
                &correction.original,
                &correction.edited,
                &correction.reason,
                now,
                &correction.tool,
                &correction.source,
                &correction.task,
            ),
        )?;
        Ok(db.last_insert_rowid())
    }

    /// The corrections that are not retired and match the terms best, at most `limit`, best first.
    /// The terms come from `search::query_terms`. Of two equal matches, a correction of an event
    /// from `source` comes first, then the newer one.
    pub fn similar_corrections(&self, terms: &[String], source: &str, limit: usize) -> Result<Vec<Correction>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT id, source FROM corrections WHERE retired_at IS NULL")?;
        let sources = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<i64, String>>>()?;
        let hits: Vec<BTreeSet<i64>> = term_hits(
            &db,
            terms,
            "SELECT c.id FROM corrections_fts f JOIN corrections c ON c.id = f.rowid
             WHERE corrections_fts MATCH ?1 AND c.retired_at IS NULL",
            "SELECT id FROM corrections WHERE retired_at IS NULL
             AND (task LIKE ?1 OR original LIKE ?1 OR edited LIKE ?1 OR reason LIKE ?1)",
        )?;
        let ranked = rank(sources.len(), &hits, |id| (sources[id] != source, Reverse(*id)));
        let mut statement =
            db.prepare("SELECT tool, source, task, original, edited, reason FROM corrections WHERE id = ?1")?;
        ranked
            .into_iter()
            .take(limit)
            .map(|id| {
                Ok(statement.query_row([id], |row| {
                    Ok(Correction {
                        tool: row.get(0)?,
                        source: row.get(1)?,
                        task: row.get(2)?,
                        original: row.get(3)?,
                        edited: row.get(4)?,
                        reason: row.get(5)?,
                    })
                })?)
            })
            .collect()
    }

    /// All corrections, also the retired ones. Newest first.
    pub fn corrections(&self, limit: u32) -> Result<Vec<CorrectionRecord>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, inbox_id, tool, source, task, original, edited, reason, created_at, retired_at
             FROM corrections ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(CorrectionRecord {
                id: row.get(0)?,
                inbox_id: row.get(1)?,
                correction: Correction {
                    tool: row.get(2)?,
                    source: row.get(3)?,
                    task: row.get(4)?,
                    original: row.get(5)?,
                    edited: row.get(6)?,
                    reason: row.get(7)?,
                },
                created_at: row.get(8)?,
                retired_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Retires a correction. Returns false if it was retired before.
    pub fn retire_correction(&self, id: i64, now: i64) -> Result<bool> {
        let db = self.db();
        let changed = db.execute(
            "UPDATE corrections SET retired_at = ?1 WHERE id = ?2 AND retired_at IS NULL",
            (now, id),
        )?;
        let exists: bool = db.query_row("SELECT EXISTS (SELECT 1 FROM corrections WHERE id = ?1)", [id], |row| {
            row.get(0)
        })?;
        if !exists {
            return Err(StoreError::NotFound("correction", id));
        }
        Ok(changed == 1)
    }

    // --- decisions ---

    pub fn add_decision(&self, at: i64, inbox_id: Option<i64>, tool: &str, kind: DecisionKind) -> Result<()> {
        insert_decision(&self.db(), at, inbox_id, tool, kind)
    }

    /// True if the tool is held at approval: its last hold was not cleared.
    pub fn is_held(&self, tool: &str) -> Result<bool> {
        is_held(&self.db(), tool)
    }

    /// The tools that are held at approval.
    pub fn held_tools(&self) -> Result<BTreeSet<String>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT d.tool FROM decisions d WHERE d.kind = 'demoted' AND d.id = (
                 SELECT MAX(h.id) FROM decisions h WHERE h.tool = d.tool AND h.kind IN ('demoted', 'hold_cleared'))",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes `kind` for the tool only if the tool's hold state is `held`, in one transaction.
    /// Returns false if the state was not `held`, so of two callers only one writes.
    fn decide_hold(&self, at: i64, inbox_id: Option<i64>, tool: &str, kind: DecisionKind, held: bool) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_held(&tx, tool)? != held {
            return Ok(false);
        }
        insert_decision(&tx, at, inbox_id, tool, kind)?;
        tx.commit()?;
        Ok(true)
    }

    /// Holds the tool at approval because of the item `inbox_id`. Returns false if it is held already.
    pub fn demote(&self, at: i64, inbox_id: i64, tool: &str) -> Result<bool> {
        self.decide_hold(at, Some(inbox_id), tool, DecisionKind::Demoted, false)
    }

    /// Clears the hold of the tool. Returns false if the tool is not held.
    pub fn clear_hold(&self, at: i64, tool: &str) -> Result<bool> {
        self.decide_hold(at, None, tool, DecisionKind::HoldCleared, true)
    }

    /// Records the owner's report of a problem with an item. Returns false if the item was reported before.
    pub fn add_report(&self, at: i64, inbox_id: i64, tool: &str) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reported: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM decisions WHERE inbox_id = ?1 AND kind = 'reported')",
            [inbox_id],
            |row| row.get(0),
        )?;
        if reported {
            return Ok(false);
        }
        insert_decision(&tx, at, Some(inbox_id), tool, DecisionKind::Reported)?;
        tx.commit()?;
        Ok(true)
    }

    /// The approvals, edits, rejections and autonomous runs of each tool from `since` (Unix seconds)
    /// on. Only the decisions after the tool's last hold count: a hold starts the counts again.
    /// An autonomous run failed if its item is `failed` and no copy of it was made. A copy means
    /// that a start or the owner did not let the call run.
    pub fn decision_counts(&self, since: i64) -> Result<BTreeMap<String, DecisionCounts>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT d.tool, d.kind, COUNT(*),
                    COALESCE(SUM(i.state = 'failed' AND NOT EXISTS (SELECT 1 FROM inbox c WHERE c.retry_of = i.id)), 0)
             FROM decisions d LEFT JOIN inbox i ON i.id = d.inbox_id
             WHERE d.at >= ?1 AND d.kind IN ('approved', 'edited', 'rejected', 'autonomous')
               AND d.id > (SELECT COALESCE(MAX(h.id), 0) FROM decisions h WHERE h.tool = d.tool AND h.kind = 'demoted')
             GROUP BY d.tool, d.kind",
        )?;
        let rows = statement.query_map([since], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
                row.get::<_, i64>(3)? as u32,
            ))
        })?;
        let mut counts: BTreeMap<String, DecisionCounts> = BTreeMap::new();
        for row in rows {
            let (tool, kind, count, failed) = row?;
            let entry = counts.entry(tool).or_default();
            match kind.as_str() {
                "approved" => entry.approved = count,
                "edited" => entry.edited = count,
                "rejected" => entry.rejected = count,
                // The query returns only the four kinds above, so this is `autonomous`.
                _ => {
                    entry.autonomous = count;
                    entry.autonomous_failed = failed;
                }
            }
        }
        Ok(counts)
    }

    // --- LLM calls ---

    pub fn record_llm_call(&self, call: &LlmCall) -> Result<()> {
        self.db().execute(
            "INSERT INTO llm_calls (at, job, purpose, input_tokens, output_tokens, latency_ms, outcome)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                call.at,
                &call.job,
                &call.purpose,
                call.input_tokens as i64,
                call.output_tokens as i64,
                call.latency_ms as i64,
                &call.outcome,
            ),
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn llm_calls(&self, limit: u32) -> Result<Vec<LlmCall>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT at, job, purpose, input_tokens, output_tokens, latency_ms, outcome
             FROM llm_calls ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(LlmCall {
                at: row.get(0)?,
                job: row.get(1)?,
                purpose: row.get(2)?,
                input_tokens: row.get::<_, i64>(3)? as u64,
                output_tokens: row.get::<_, i64>(4)? as u64,
                latency_ms: row.get::<_, i64>(5)? as u64,
                outcome: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The calls from `since` (Unix seconds) on, added up by UTC day and purpose. Newest day first.
    pub fn llm_usage(&self, since: i64) -> Result<Vec<LlmUsage>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT date(at, 'unixepoch') AS day, purpose, COUNT(*), SUM(outcome <> 'end'),
                    SUM(input_tokens), SUM(output_tokens), SUM(latency_ms)
             FROM llm_calls WHERE at >= ?1 GROUP BY day, purpose ORDER BY day DESC, purpose",
        )?;
        let rows = statement.query_map([since], |row| {
            Ok(LlmUsage {
                day: row.get(0)?,
                purpose: row.get(1)?,
                calls: row.get::<_, i64>(2)? as u64,
                failed: row.get::<_, i64>(3)? as u64,
                input_tokens: row.get::<_, i64>(4)? as u64,
                output_tokens: row.get::<_, i64>(5)? as u64,
                latency_ms: row.get::<_, i64>(6)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- artifacts ---

    fn artifact_path(&self, sha256: &str) -> PathBuf {
        self.dir.join("artifacts").join("sha256").join(sha256)
    }

    /// Saves a work product. The file name is the content hash.
    pub fn put_artifact(
        &self,
        kind: &str,
        content: &[u8],
        now: i64,
        event_key: Option<&str>,
        inbox_id: Option<i64>,
    ) -> Result<ArtifactMeta> {
        let sha256 = sha256_hex(content);
        let path = self.artifact_path(&sha256);
        if !path.exists() {
            // Write to a temporary name first, so a crash never leaves a partial artifact.
            let partial = path.with_extension("partial");
            std::fs::write(&partial, content)?;
            std::fs::rename(&partial, &path)?;
        }
        let db = self.db();
        db.execute(
            "INSERT INTO artifacts (kind, sha256, size, created_at, event_key, inbox_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (kind, &sha256, content.len() as i64, now, event_key, inbox_id),
        )?;
        Ok(ArtifactMeta {
            id: db.last_insert_rowid(),
            kind: kind.to_owned(),
            sha256,
            size: content.len() as u64,
            created_at: now,
            event_key: event_key.map(str::to_owned),
            inbox_id,
        })
    }

    /// Newest first.
    pub fn list_artifacts(&self, limit: u32) -> Result<Vec<ArtifactMeta>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT id, kind, sha256, size, created_at, event_key, inbox_id
             FROM artifacts ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(ArtifactMeta {
                id: row.get(0)?,
                kind: row.get(1)?,
                sha256: row.get(2)?,
                size: row.get::<_, i64>(3)? as u64,
                created_at: row.get(4)?,
                event_key: row.get(5)?,
                inbox_id: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Reads an artifact and checks its content against the recorded hash.
    pub fn read_artifact(&self, id: i64) -> Result<Vec<u8>> {
        let sha256: String = self
            .db()
            .query_row("SELECT sha256 FROM artifacts WHERE id = ?1", [id], |row| row.get(0))
            .optional()?
            .ok_or(StoreError::NotFound("artifact", id))?;
        let content = std::fs::read(self.artifact_path(&sha256))?;
        if sha256_hex(&content) != sha256 {
            return Err(StoreError::ArtifactDamaged { id });
        }
        Ok(content)
    }

    // --- audit ---

    pub fn append_audit(&self, now: i64, kind: &str, event_key: Option<&str>, detail: &Value) -> Result<()> {
        self.db().execute(
            "INSERT INTO audit (at, kind, event_key, detail) VALUES (?1, ?2, ?3, ?4)",
            (now, kind, event_key, detail.to_string()),
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn list_audit(&self, limit: u32) -> Result<Vec<AuditEntry>> {
        let db = self.db();
        let mut statement =
            db.prepare("SELECT id, at, kind, event_key, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = statement
            .query_map([limit], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<(i64, i64, String, Option<String>, String)>>>()?;
        rows.into_iter()
            .map(|(id, at, kind, event_key, detail)| {
                Ok(AuditEntry {
                    id,
                    at,
                    kind,
                    event_key,
                    detail: serde_json::from_str(&detail)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AppStore;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let profile = app.open_profile("kim").unwrap();
        (dir, profile)
    }

    fn tool_call(text: &str) -> InboxBody {
        InboxBody::ToolCall {
            tool: "slack/post".into(),
            args: json!({"text": text}).as_object().unwrap().clone(),
        }
    }

    #[test]
    fn a_finished_event_is_not_started_again() {
        let (_dir, s) = store();
        assert_eq!(
            s.begin_event("k1", &json!({"text": "a"}), 1, 3).unwrap(),
            EventStart::New
        );
        s.finish_event("k1").unwrap();
        assert_eq!(
            s.begin_event("k1", &json!({"text": "a again"}), 2, 3).unwrap(),
            EventStart::Done
        );
        assert_eq!(
            s.begin_event("k2", &json!({"text": "b"}), 3, 3).unwrap(),
            EventStart::New
        );
    }

    #[test]
    fn an_unfinished_event_is_retried_a_limited_number_of_times() {
        let (_dir, s) = store();
        let payload = json!({});
        assert_eq!(s.begin_event("k1", &payload, 1, 3).unwrap(), EventStart::New);
        assert_eq!(
            s.begin_event("k1", &payload, 2, 3).unwrap(),
            EventStart::Retry { attempt: 2 }
        );
        assert_eq!(
            s.begin_event("k1", &payload, 3, 3).unwrap(),
            EventStart::Retry { attempt: 3 }
        );
        // The tries are used up. This is reported once, then the event is closed.
        assert_eq!(s.begin_event("k1", &payload, 4, 3).unwrap(), EventStart::GaveUp);
        assert_eq!(s.begin_event("k1", &payload, 5, 3).unwrap(), EventStart::Done);
    }

    #[test]
    fn an_event_marked_as_seen_is_never_handled() {
        let (_dir, s) = store();
        assert!(s.mark_event_seen("k1", &json!({}), 1).unwrap());
        assert!(!s.mark_event_seen("k1", &json!({}), 2).unwrap());
        assert_eq!(s.begin_event("k1", &json!({}), 3, 3).unwrap(), EventStart::Done);
        // Marking does not close an event that is being handled.
        assert_eq!(s.begin_event("k2", &json!({}), 4, 3).unwrap(), EventStart::New);
        assert!(!s.mark_event_seen("k2", &json!({}), 5).unwrap());
        assert_eq!(
            s.begin_event("k2", &json!({}), 6, 3).unwrap(),
            EventStart::Retry { attempt: 2 }
        );
    }

    #[test]
    fn a_database_from_an_older_version_gets_the_new_event_columns() {
        let dir = tempfile::tempdir().unwrap();
        let profile_dir = dir.path().join("profiles/kim");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let old = Connection::open(profile_dir.join("agent.db")).unwrap();
        old.execute_batch(
            "CREATE TABLE events (dedupe_key TEXT PRIMARY KEY, received_at INTEGER NOT NULL, payload TEXT NOT NULL);
             INSERT INTO events VALUES ('old', 1, '{}');",
        )
        .unwrap();
        drop(old);
        let s = ProfileStore::open(&profile_dir).unwrap();
        // An event from the old database counts as handled.
        assert_eq!(s.begin_event("old", &json!({}), 2, 3).unwrap(), EventStart::Done);
        assert_eq!(s.begin_event("new", &json!({}), 3, 3).unwrap(), EventStart::New);
    }

    #[test]
    fn an_older_inbox_gets_the_resume_columns_and_its_approved_items_count_as_autonomous() {
        let dir = tempfile::tempdir().unwrap();
        let profile_dir = dir.path().join("profiles/kim");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let old = Connection::open(profile_dir.join("agent.db")).unwrap();
        old.execute_batch(
            r#"CREATE TABLE inbox (id INTEGER PRIMARY KEY AUTOINCREMENT, event_key TEXT NOT NULL,
                 body TEXT NOT NULL, rationale TEXT NOT NULL, basis TEXT NOT NULL DEFAULT '[]',
                 state TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
               INSERT INTO inbox (event_key, body, rationale, state, created_at, updated_at) VALUES
                 ('k1', '{"kind":"tool_call","tool":"slack/post","args":{}}', 'r', 'approved', 1, 1),
                 ('k2', '{"kind":"tool_call","tool":"slack/post","args":{}}', 'r', 'pending', 2, 2);"#,
        )
        .unwrap();
        drop(old);
        let s = ProfileStore::open(&profile_dir).unwrap();
        let items = s.list_inbox(10).unwrap();
        let flags: Vec<_> = items.iter().map(|i| (i.id, i.autonomous, i.retry_of)).collect();
        assert_eq!(flags, [(2, false, None), (1, true, None)]);
        // Opening again changes nothing.
        drop(s);
        let s = ProfileStore::open(&profile_dir).unwrap();
        assert!(s.inbox_item(1).unwrap().autonomous);
    }

    #[test]
    fn only_an_item_that_starts_approved_is_autonomous() {
        let (_dir, s) = store();
        let pending = s
            .add_inbox_item("k1", &tool_call("a"), "r", &[], ApprovalState::Pending, 1)
            .unwrap();
        let approved = s
            .add_inbox_item("k2", &tool_call("b"), "r", &[], ApprovalState::Approved, 2)
            .unwrap();
        assert!(!pending.autonomous && approved.autonomous);
        assert_eq!(s.inbox_item(approved.id).unwrap(), approved);
        // An owner's approval does not make an item autonomous.
        s.move_inbox_state(pending.id, ApprovalState::Pending, ApprovalState::Approved, 3)
            .unwrap();
        let approved_ids: Vec<_> = s
            .inbox_in_state(ApprovalState::Approved)
            .unwrap()
            .into_iter()
            .map(|i| (i.id, i.autonomous))
            .collect();
        assert_eq!(approved_ids, [(pending.id, false), (approved.id, true)]);
        assert!(s.inbox_in_state(ApprovalState::Unknown).unwrap().is_empty());
    }

    #[test]
    fn a_pending_copy_replaces_an_item_once_and_only_from_the_given_state() {
        let (_dir, s) = store();
        let basis = [ReadNote {
            tool: "slack/search".into(),
            args: json!({"query": "deploy"}).as_object().unwrap().clone(),
        }];
        let item = s
            .add_inbox_item("k1", &tool_call("hi"), "asked", &basis, ApprovalState::Approved, 1)
            .unwrap();
        // The item is not unknown: nothing changes.
        assert_eq!(
            s.fail_with_pending_copy(item.id, ApprovalState::Unknown, 2).unwrap(),
            None
        );
        assert_eq!(s.list_inbox(10).unwrap().len(), 1);

        let copy = s
            .fail_with_pending_copy(item.id, ApprovalState::Approved, 3)
            .unwrap()
            .unwrap();
        assert_ne!(copy.id, item.id);
        assert_eq!(
            (copy.state, copy.retry_of, copy.autonomous, copy.created_at),
            (ApprovalState::Pending, Some(item.id), false, 3)
        );
        assert_eq!(
            (&copy.event_key, &copy.body, &copy.rationale, &copy.basis),
            (&item.event_key, &item.body, &item.rationale, &item.basis)
        );
        assert_eq!(s.inbox_item(copy.id).unwrap(), copy);
        let original = s.inbox_item(item.id).unwrap();
        assert_eq!((original.state, original.updated_at), (ApprovalState::Failed, 3));
        // A second caller loses and makes no second copy.
        assert_eq!(
            s.fail_with_pending_copy(item.id, ApprovalState::Approved, 4).unwrap(),
            None
        );
        assert_eq!(s.list_inbox(10).unwrap().len(), 2);
    }

    #[test]
    fn kill_switch_defaults_to_off_and_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let first = app.open_profile("kim").unwrap();
        assert!(!first.kill_switch().unwrap());
        first.set_kill_switch(true).unwrap();
        drop(first);
        drop(app);
        let reopened = AppStore::open(dir.path()).unwrap().open_profile("kim").unwrap();
        assert!(reopened.kill_switch().unwrap());
        reopened.set_kill_switch(false).unwrap();
        assert!(!reopened.kill_switch().unwrap());
    }

    #[test]
    fn inbox_items_round_trip_newest_first() {
        let (_dir, s) = store();
        let a = s
            .add_inbox_item("k1", &tool_call("hi"), "asked", &[], ApprovalState::Pending, 10)
            .unwrap();
        let b = s
            .add_inbox_item(
                "k2",
                &InboxBody::Escalation {
                    summary: "decide".into(),
                },
                "unsure",
                &[],
                ApprovalState::Draft,
                11,
            )
            .unwrap();
        assert_eq!(s.inbox_item(a.id).unwrap(), a);
        assert_eq!(s.list_inbox(10).unwrap(), [b.clone(), a]);
        assert_eq!(s.list_inbox(1).unwrap(), [b]);
        assert!(matches!(
            s.inbox_item(999),
            Err(StoreError::NotFound("inbox item", 999))
        ));
    }

    #[test]
    fn a_state_move_happens_only_from_the_expected_state() {
        let (_dir, s) = store();
        let item = s
            .add_inbox_item("k1", &tool_call("hi"), "r", &[], ApprovalState::Pending, 10)
            .unwrap();
        assert!(
            s.move_inbox_state(item.id, ApprovalState::Pending, ApprovalState::Approved, 20)
                .unwrap()
        );
        // A second approval of the same item loses.
        assert!(
            !s.move_inbox_state(item.id, ApprovalState::Pending, ApprovalState::Approved, 21)
                .unwrap()
        );
        let now = s.inbox_item(item.id).unwrap();
        assert_eq!((now.state, now.updated_at), (ApprovalState::Approved, 20));
    }

    #[test]
    fn only_a_pending_body_can_be_replaced() {
        let (_dir, s) = store();
        let item = s
            .add_inbox_item("k1", &tool_call("hi"), "r", &[], ApprovalState::Pending, 10)
            .unwrap();
        assert!(s.replace_pending_body(item.id, &tool_call("hello"), 11).unwrap());
        assert_eq!(s.inbox_item(item.id).unwrap().body, tool_call("hello"));
        s.move_inbox_state(item.id, ApprovalState::Pending, ApprovalState::Approved, 12)
            .unwrap();
        assert!(
            !s.replace_pending_body(item.id, &tool_call("changed after approval"), 13)
                .unwrap()
        );
        assert_eq!(s.inbox_item(item.id).unwrap().body, tool_call("hello"));
    }

    fn correction(source: &str, task: &str, reason: &str) -> Correction {
        Correction {
            tool: "slack/post".into(),
            source: source.into(),
            task: task.into(),
            original: "Draft".into(),
            edited: "Edit".into(),
            reason: reason.into(),
        }
    }

    #[test]
    fn corrections_are_listed_newest_first_and_retired_once() {
        let (_dir, s) = store();
        let mut ids = Vec::new();
        for (i, reason) in ["too long", "wrong tone", "missing link"].iter().enumerate() {
            ids.push(
                s.add_correction(i as i64 + 7, &correction("slack", "t", reason), i as i64)
                    .unwrap(),
            );
        }
        let listed = s.corrections(2).unwrap();
        let rows: Vec<_> = listed
            .iter()
            .map(|r| {
                (
                    r.id,
                    r.inbox_id,
                    r.correction.reason.as_str(),
                    r.created_at,
                    r.retired_at,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [(ids[2], 9, "missing link", 2, None), (ids[1], 8, "wrong tone", 1, None)]
        );
        assert_eq!(listed[0].correction, correction("slack", "t", "missing link"));

        assert!(s.retire_correction(ids[1], 50).unwrap());
        // A second retire changes nothing, also not the time.
        assert!(!s.retire_correction(ids[1], 60).unwrap());
        assert_eq!(s.corrections(3).unwrap()[1].retired_at, Some(50));
        assert!(matches!(
            s.retire_correction(999, 70),
            Err(StoreError::NotFound("correction", 999))
        ));
    }

    #[test]
    fn a_stored_event_is_read_back_by_its_key() {
        let (_dir, s) = store();
        let event = InboundEvent {
            source: "slack".into(),
            external_id: "1".into(),
            thread: None,
            author: "lee".into(),
            text: "deploy?".into(),
            channel: Default::default(),
        };
        s.begin_event("k1", &serde_json::to_value(&event).unwrap(), 1, 3)
            .unwrap();
        assert_eq!(s.event("k1").unwrap(), Some(event));
        assert_eq!(s.event("k2").unwrap(), None);
    }

    fn terms(text: &str) -> Vec<String> {
        bunsin_core::search::query_terms(text)
    }

    fn reasons(found: Vec<Correction>) -> Vec<String> {
        found.into_iter().map(|c| c.reason).collect()
    }

    #[test]
    fn similar_corrections_are_found_by_rare_terms_and_skip_retired_ones() {
        let (_dir, s) = store();
        let tasks = [
            ("slack", "금요일 배포 일정을 알려 주세요", "deploy day"),
            ("jira", "the release notes for PROJ please", "release notes"),
            ("slack", "the lunch menu please", "lunch"),
            ("jira", "배포는 언제인가요", "deploy question"),
            ("slack", "the parking lot please", "parking"),
        ];
        for (i, (source, task, reason)) in tasks.iter().enumerate() {
            s.add_correction(i as i64 + 1, &correction(source, task, reason), i as i64)
                .unwrap();
        }
        // `배포` has two syllables and is found by LIKE in two tasks; `금요일` by the trigram index.
        let found = s
            .similar_corrections(&terms("금요일에 배포를 하나요?"), "slack", 5)
            .unwrap();
        assert_eq!(reasons(found), ["deploy day", "deploy question"]);
        // `the` and `please` are in three of five tasks: too common to find anything alone.
        assert!(
            s.similar_corrections(&terms("the please"), "slack", 5)
                .unwrap()
                .is_empty()
        );
        let found = s.similar_corrections(&terms("the release please"), "slack", 5).unwrap();
        assert_eq!(reasons(found), ["release notes"]);
        // A word of every correction finds none, also in the texts of the draft and the edit.
        assert!(
            s.similar_corrections(&terms("draft edit"), "slack", 5)
                .unwrap()
                .is_empty()
        );

        // Equal matches: the same source first, then the newest.
        let found = s.similar_corrections(&terms("배포"), "jira", 5).unwrap();
        assert_eq!(reasons(found), ["deploy question", "deploy day"]);
        let found = s.similar_corrections(&terms("배포"), "slack", 5).unwrap();
        assert_eq!(reasons(found), ["deploy day", "deploy question"]);
        let found = s.similar_corrections(&terms("배포"), "github", 1).unwrap();
        assert_eq!(reasons(found), ["deploy question"]);

        // A retired correction is neither found nor counted.
        s.db()
            .execute("UPDATE corrections SET retired_at = 9 WHERE reason = 'deploy day'", [])
            .unwrap();
        let found = s.similar_corrections(&terms("금요일 배포"), "slack", 5).unwrap();
        assert_eq!(reasons(found), ["deploy question"]);
        assert!(s.similar_corrections(&[], "slack", 5).unwrap().is_empty());
    }

    #[test]
    fn the_notes_index_follows_inserts_upserts_and_deletes() {
        let (_dir, s) = store();
        let write = |slug: &str, body: &str, base: u64, now: i64| {
            stored(
                s.write_wiki_page(slug, &wiki_text(body), &[], WikiAuthor::Owner, base, None, now)
                    .unwrap(),
            )
        };
        write("deploy", "금요일 배포. Rollback by hand.", 0, 10);
        write("oncall", "당번 순서표. Rollback needs the on-call.", 0, 20);
        write("lunch", "Noodles on Monday.", 0, 30);
        write("parking", "Level B2.", 0, 40);
        assert_eq!(s.search_wiki(&terms("금요일")).unwrap(), ["deploy"]);
        assert_eq!(s.search_wiki(&terms("배포")).unwrap(), ["deploy"]);
        // Both pages match: the one changed last comes first.
        assert_eq!(s.search_wiki(&terms("rollback")).unwrap(), ["oncall", "deploy"]);
        // A page that matches two terms beats one that matches one.
        assert_eq!(s.search_wiki(&terms("rollback 당번")).unwrap(), ["oncall", "deploy"]);
        assert_eq!(s.search_wiki(&terms("rollback 금요일")).unwrap(), ["deploy", "oncall"]);
        // The title of every page has `schedule`: too common to find a page.
        assert!(s.search_wiki(&terms("schedule")).unwrap().is_empty());

        // An UPSERT replaces the indexed text.
        write("deploy", "목요일 배포.", 1, 50);
        assert!(s.search_wiki(&terms("금요일")).unwrap().is_empty());
        assert_eq!(s.search_wiki(&terms("목요일")).unwrap(), ["deploy"]);
        assert_eq!(s.search_wiki(&terms("rollback")).unwrap(), ["oncall"]);
        // A read changes no indexed text.
        s.mark_wiki_read("oncall", 55).unwrap();
        assert_eq!(s.search_wiki(&terms("rollback")).unwrap(), ["oncall"]);
        // A delete removes it.
        assert!(s.delete_wiki_page("deploy", 60).unwrap());
        assert!(s.search_wiki(&terms("목요일")).unwrap().is_empty());
        let indexed: i64 = s
            .db()
            .query_row("SELECT count(*) FROM wiki_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, 3);
    }

    #[test]
    fn an_older_database_gets_filled_search_indexes_once() {
        let dir = tempfile::tempdir().unwrap();
        let profile_dir = dir.path().join("profiles/kim");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let old = Connection::open(profile_dir.join("agent.db")).unwrap();
        old.execute_batch(
            "CREATE TABLE corrections (id INTEGER PRIMARY KEY AUTOINCREMENT, inbox_id INTEGER NOT NULL,
                 original TEXT NOT NULL, edited TEXT NOT NULL, reason TEXT NOT NULL, created_at INTEGER NOT NULL);
             INSERT INTO corrections (inbox_id, original, edited, reason, created_at) VALUES
                 (1, 'Long formal draft', 'Short', 'too formal', 1),
                 (2, 'Lunch draft', 'Lunch', 'menu', 2),
                 (3, 'Parking draft', 'Parking', 'level', 3);
             CREATE TABLE wiki_pages (slug TEXT PRIMARY KEY, title TEXT NOT NULL, summary TEXT NOT NULL,
                 body TEXT NOT NULL, sources TEXT NOT NULL, revision INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL, author TEXT NOT NULL);
             INSERT INTO wiki_pages VALUES ('deploy', 'Deploy', 'When', '금요일 배포', '[]', 1, 1, 'owner'),
                 ('lunch', 'Lunch', 'Where', 'Noodles', '[]', 1, 2, 'owner');",
        )
        .unwrap();
        drop(old);
        let s = ProfileStore::open(&profile_dir).unwrap();
        let found = s.similar_corrections(&terms("formal"), "slack", 5).unwrap();
        assert_eq!(
            found,
            [Correction {
                tool: String::new(),
                source: String::new(),
                task: String::new(),
                original: "Long formal draft".into(),
                edited: "Short".into(),
                reason: "too formal".into(),
            }]
        );
        assert_eq!(s.search_wiki(&terms("금요일")).unwrap(), ["deploy"]);
        // A new correction is indexed too.
        s.add_correction(4, &correction("slack", "금요일 회의", "meeting"), 4)
            .unwrap();
        assert_eq!(
            reasons(s.similar_corrections(&terms("금요일"), "slack", 5).unwrap()),
            ["meeting"]
        );
        // Opening again fills nothing twice.
        drop(s);
        let s = ProfileStore::open(&profile_dir).unwrap();
        let count = |table: &str| -> i64 {
            s.db()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!((count("wiki_fts"), count("corrections_fts")), (2, 4));
        assert_eq!(s.search_wiki(&terms("noodles")).unwrap(), ["lunch"]);
    }

    fn llm_call(at: i64, purpose: &str, tokens: u64, latency_ms: u64, outcome: &str) -> LlmCall {
        LlmCall {
            at,
            job: Some("slack:1".into()),
            purpose: purpose.into(),
            input_tokens: tokens,
            output_tokens: tokens / 10,
            latency_ms,
            outcome: outcome.into(),
        }
    }

    #[test]
    fn llm_calls_are_added_up_by_utc_day_and_purpose() {
        let (_dir, s) = store();
        // 2026-11-16 23:59:59 UTC, then two calls on 2026-11-17 UTC.
        let day_one = 1_794_873_599;
        s.record_llm_call(&llm_call(day_one, "event", 100, 900, "end")).unwrap();
        s.record_llm_call(&llm_call(day_one + 1, "event", 200, 1_000, "end"))
            .unwrap();
        s.record_llm_call(&llm_call(day_one + 2, "event", 0, 50, "transport_error"))
            .unwrap();
        s.record_llm_call(&llm_call(day_one + 3, "replay", 300, 2_000, "max_tokens"))
            .unwrap();
        // Before `since`: not counted.
        s.record_llm_call(&llm_call(10, "event", 999, 1, "end")).unwrap();

        let usage = s.llm_usage(1_000).unwrap();
        let rows: Vec<_> = usage
            .iter()
            .map(|u| {
                (
                    u.day.as_str(),
                    u.purpose.as_str(),
                    u.calls,
                    u.failed,
                    u.input_tokens,
                    u.output_tokens,
                    u.latency_ms,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("2026-11-17", "event", 2, 1, 200, 20, 1_050),
                ("2026-11-17", "replay", 1, 1, 300, 30, 2_000),
                ("2026-11-16", "event", 1, 0, 100, 10, 900),
            ]
        );
        let calls = s.llm_calls(2).unwrap();
        assert_eq!(
            calls,
            [
                llm_call(10, "event", 999, 1, "end"),
                llm_call(day_one + 3, "replay", 300, 2_000, "max_tokens")
            ]
        );
        assert!(s.llm_usage(i64::MAX).unwrap().is_empty());
    }

    #[test]
    fn artifacts_round_trip_and_keep_their_links() {
        let (_dir, s) = store();
        let meta = s
            .put_artifact("proposal", "초안 본문".as_bytes(), 7, Some("k1"), Some(3))
            .unwrap();
        assert_eq!(meta.size, "초안 본문".len() as u64);
        assert_eq!(s.read_artifact(meta.id).unwrap(), "초안 본문".as_bytes());
        assert_eq!(s.list_artifacts(10).unwrap(), [meta]);
    }

    #[test]
    fn equal_content_is_stored_once_but_listed_twice() {
        let (dir, s) = store();
        let a = s.put_artifact("draft", b"same", 1, None, None).unwrap();
        let b = s.put_artifact("tool_result", b"same", 2, None, None).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_ne!(a.id, b.id);
        let files = std::fs::read_dir(dir.path().join("profiles/kim/artifacts/sha256"))
            .unwrap()
            .count();
        assert_eq!(files, 1);
    }

    #[test]
    fn a_changed_artifact_file_is_reported() {
        let (dir, s) = store();
        let meta = s.put_artifact("draft", b"original", 1, None, None).unwrap();
        std::fs::write(
            dir.path().join("profiles/kim/artifacts/sha256").join(&meta.sha256),
            b"tampered",
        )
        .unwrap();
        assert!(matches!(s.read_artifact(meta.id), Err(StoreError::ArtifactDamaged { id }) if id == meta.id));
    }

    #[test]
    fn the_audit_log_is_append_only() {
        let (_dir, s) = store();
        s.append_audit(1, "policy_decision", Some("k1"), &json!({"decision": "needs_approval"}))
            .unwrap();
        s.append_audit(2, "executed", Some("k1"), &json!({"ok": true})).unwrap();

        let update = s.db().execute("UPDATE audit SET kind = 'forged' WHERE id = 1", []);
        let delete = s.db().execute("DELETE FROM audit", []);
        assert!(update.unwrap_err().to_string().contains("append-only"));
        assert!(delete.unwrap_err().to_string().contains("append-only"));

        let entries = s.list_audit(10).unwrap();
        assert_eq!(
            entries.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            ["executed", "policy_decision"]
        );
        assert_eq!(entries[1].detail, json!({"decision": "needs_approval"}));
        assert_eq!(entries[1].event_key.as_deref(), Some("k1"));
    }

    #[test]
    fn sealed_secrets_round_trip_and_are_replaced_and_deleted() {
        let (_dir, s) = store();
        let first = SealedSecret {
            nonce: vec![1; 24],
            ciphertext: vec![9, 8, 7],
            origin: "https://a.example:443".into(),
        };
        s.put_secret("LLM", &first, 1).unwrap();
        assert_eq!(s.secret("LLM").unwrap(), Some(first));
        assert_eq!(s.secret("MCP_SLACK").unwrap(), None);

        let second = SealedSecret {
            nonce: vec![2; 24],
            ciphertext: vec![1],
            origin: "https://b.example:443".into(),
        };
        s.put_secret("LLM", &second, 2).unwrap();
        s.put_secret("MCP_SLACK", &second, 2).unwrap();
        assert_eq!(s.secret("LLM").unwrap(), Some(second));
        assert_eq!(s.secret_keys().unwrap(), ["LLM", "MCP_SLACK"]);

        assert!(s.delete_secret("LLM").unwrap());
        assert!(!s.delete_secret("LLM").unwrap());
        assert_eq!(s.secret_keys().unwrap(), ["MCP_SLACK"]);
    }

    #[test]
    fn a_setting_is_replaced_only_from_the_expected_value() {
        let (_dir, s) = store();
        s.set_setting("config", "v1").unwrap();
        assert!(s.replace_setting_if("config", "v1", "v2").unwrap());
        // A second change that also started from v1 loses.
        assert!(!s.replace_setting_if("config", "v1", "v3").unwrap());
        assert_eq!(s.setting("config").unwrap().as_deref(), Some("v2"));
        assert!(!s.replace_setting_if("missing", "", "x").unwrap());
    }

    #[test]
    fn an_inbox_item_keeps_the_reads_it_is_based_on() {
        let (_dir, s) = store();
        let basis = [ReadNote {
            tool: "slack/search".into(),
            args: json!({"query": "deploy"}).as_object().unwrap().clone(),
        }];
        let item = s
            .add_inbox_item("k1", &tool_call("hi"), "r", &basis, ApprovalState::Pending, 1)
            .unwrap();
        assert_eq!(s.inbox_item(item.id).unwrap().basis, basis);
        assert_eq!(s.list_inbox(1).unwrap()[0].basis, basis);
    }

    fn wiki_text(body: &str) -> WikiText {
        WikiText {
            title: "Deploy schedule".into(),
            summary: "When we deploy".into(),
            body: body.into(),
        }
    }

    fn source() -> Vec<WikiSource> {
        vec![WikiSource {
            origin: "slack/search".into(),
            detail: "{\"query\":\"deploy\"}".into(),
        }]
    }

    fn stored(write: WikiWrite) -> WikiPage {
        match write {
            WikiWrite::Stored(page) => page,
            other => panic!("expected a stored page, got {other:?}"),
        }
    }

    #[test]
    fn a_page_is_created_changed_listed_and_keeps_its_history() {
        let (_dir, s) = store();
        let first = stored(
            s.write_wiki_page(
                "deploy",
                &wiki_text("Fridays."),
                &source(),
                WikiAuthor::Agent,
                0,
                None,
                10,
            )
            .unwrap(),
        );
        assert_eq!(
            (first.revision, first.updated_at, first.author),
            (1, 10, WikiAuthor::Agent)
        );
        let second = stored(
            s.write_wiki_page("deploy", &wiki_text("Thursdays."), &[], WikiAuthor::Owner, 1, None, 20)
                .unwrap(),
        );
        assert_eq!((second.revision, second.author), (2, WikiAuthor::Owner));
        assert_eq!(s.wiki_page("deploy").unwrap(), Some(second.clone()));
        assert_eq!(s.wiki_page("missing").unwrap(), None);

        stored(
            s.write_wiki_page("alpha", &wiki_text("x"), &[], WikiAuthor::Owner, 0, None, 30)
                .unwrap(),
        );
        let index = s.wiki_index().unwrap();
        assert_eq!(
            index.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["alpha", "deploy"]
        );
        assert_eq!((index[1].revision, index[1].updated_at), (2, 20));

        assert_eq!(s.wiki_history("deploy").unwrap(), [Some(first), Some(second)]);
    }

    #[test]
    fn a_change_written_against_an_old_revision_is_refused() {
        let (_dir, s) = store();
        stored(
            s.write_wiki_page("deploy", &wiki_text("v1"), &[], WikiAuthor::Agent, 0, None, 1)
                .unwrap(),
        );
        stored(
            s.write_wiki_page("deploy", &wiki_text("v2"), &[], WikiAuthor::Owner, 1, None, 2)
                .unwrap(),
        );
        // A change that was written when the page was at revision 1.
        let stale = s
            .write_wiki_page("deploy", &wiki_text("stale"), &[], WikiAuthor::Agent, 1, None, 3)
            .unwrap();
        assert_eq!(stale, WikiWrite::RevisionMismatch { current: 2 });
        // Two proposals for a new page: the second does not replace the first.
        let again = s
            .write_wiki_page("deploy", &wiki_text("new"), &[], WikiAuthor::Agent, 0, None, 4)
            .unwrap();
        assert_eq!(again, WikiWrite::RevisionMismatch { current: 2 });
        assert_eq!(s.wiki_page("deploy").unwrap().unwrap().text.body, "v2");
    }

    #[test]
    fn approving_a_page_change_stores_it_once_together_with_the_inbox_move() {
        let (_dir, s) = store();
        let body = InboxBody::WikiChange {
            slug: "deploy".into(),
            text: wiki_text("Fridays."),
            sources: source(),
            base_revision: 0,
            previous: None,
        };
        let item = s
            .add_inbox_item("k1", &body, "learned from a search", &[], ApprovalState::Pending, 1)
            .unwrap();
        assert!(s.has_pending_wiki_change("deploy").unwrap());
        assert!(!s.has_pending_wiki_change("other").unwrap());

        let page = stored(
            s.write_wiki_page(
                "deploy",
                &wiki_text("Fridays."),
                &source(),
                WikiAuthor::Agent,
                0,
                Some(item.id),
                5,
            )
            .unwrap(),
        );
        assert_eq!(page.revision, 1);
        assert_eq!(s.inbox_item(item.id).unwrap().state, ApprovalState::Executed);
        assert!(!s.has_pending_wiki_change("deploy").unwrap());

        // A second approval of the same item stores nothing.
        let again = s
            .write_wiki_page(
                "deploy",
                &wiki_text("Fridays."),
                &source(),
                WikiAuthor::Agent,
                1,
                Some(item.id),
                6,
            )
            .unwrap();
        assert_eq!(again, WikiWrite::ItemHandled);
        assert_eq!(s.wiki_page("deploy").unwrap().unwrap().revision, 1);
    }

    #[test]
    fn a_refused_page_change_leaves_its_inbox_item_pending() {
        let (_dir, s) = store();
        stored(
            s.write_wiki_page("deploy", &wiki_text("v1"), &[], WikiAuthor::Owner, 0, None, 1)
                .unwrap(),
        );
        let body = InboxBody::WikiChange {
            slug: "deploy".into(),
            text: wiki_text("x"),
            sources: vec![],
            base_revision: 0,
            previous: None,
        };
        let item = s
            .add_inbox_item("k1", &body, "r", &[], ApprovalState::Pending, 2)
            .unwrap();
        let result = s
            .write_wiki_page("deploy", &wiki_text("x"), &[], WikiAuthor::Agent, 0, Some(item.id), 3)
            .unwrap();
        assert_eq!(result, WikiWrite::RevisionMismatch { current: 1 });
        // The inbox move was undone with the refused write.
        assert_eq!(s.inbox_item(item.id).unwrap().state, ApprovalState::Pending);
    }

    #[test]
    fn a_read_of_a_page_is_recorded_and_kept_through_a_change() {
        let (_dir, s) = store();
        let last_read = |slug: &str| -> Option<i64> {
            s.db()
                .query_row("SELECT last_read_at FROM wiki_pages WHERE slug = ?1", [slug], |row| {
                    row.get(0)
                })
                .unwrap()
        };
        stored(
            s.write_wiki_page("deploy", &wiki_text("v1"), &[], WikiAuthor::Owner, 0, None, 1)
                .unwrap(),
        );
        assert_eq!(last_read("deploy"), None);
        s.mark_wiki_read("deploy", 5).unwrap();
        assert_eq!(last_read("deploy"), Some(5));
        stored(
            s.write_wiki_page("deploy", &wiki_text("v2"), &[], WikiAuthor::Agent, 1, None, 6)
                .unwrap(),
        );
        assert_eq!(last_read("deploy"), Some(5));
        // A page that does not exist is not made.
        s.mark_wiki_read("missing", 7).unwrap();
        assert!(s.wiki_page("missing").unwrap().is_none());
    }

    #[test]
    fn a_deleted_page_is_gone_but_its_history_stays() {
        let (_dir, s) = store();
        let page = stored(
            s.write_wiki_page("deploy", &wiki_text("v1"), &[], WikiAuthor::Owner, 0, None, 1)
                .unwrap(),
        );
        assert!(s.delete_wiki_page("deploy", 2).unwrap());
        assert!(!s.delete_wiki_page("deploy", 3).unwrap());
        assert_eq!(s.wiki_page("deploy").unwrap(), None);
        assert!(s.wiki_index().unwrap().is_empty());
        assert_eq!(s.wiki_history("deploy").unwrap(), [Some(page), None]);
        // The page can be created again.
        assert_eq!(
            stored(
                s.write_wiki_page("deploy", &wiki_text("v2"), &[], WikiAuthor::Owner, 0, None, 4)
                    .unwrap()
            )
            .revision,
            1
        );
    }

    #[test]
    fn the_decisions_are_append_only() {
        let (_dir, s) = store();
        s.add_decision(1, Some(4), "slack/post", DecisionKind::Approved)
            .unwrap();
        assert!(s.db().execute("UPDATE decisions SET kind = 'rejected'", []).is_err());
        assert!(s.db().execute("DELETE FROM decisions", []).is_err());
        assert_eq!(s.decision_counts(0).unwrap()["slack/post"].approved, 1);
    }

    #[test]
    fn a_hold_is_written_once_and_cleared_once() {
        let (_dir, s) = store();
        assert!(!s.clear_hold(1, "slack/post").unwrap());
        assert!(s.demote(2, 7, "slack/post").unwrap());
        // A second hold of a held tool writes nothing.
        assert!(!s.demote(3, 8, "slack/post").unwrap());
        assert!(s.is_held("slack/post").unwrap());
        assert!(!s.is_held("slack/react").unwrap());
        assert_eq!(s.held_tools().unwrap(), BTreeSet::from(["slack/post".to_owned()]));
        // Other decisions after the hold do not end it.
        s.add_decision(4, Some(9), "slack/post", DecisionKind::Approved)
            .unwrap();
        assert!(s.is_held("slack/post").unwrap());
        assert!(s.clear_hold(5, "slack/post").unwrap());
        assert!(!s.clear_hold(6, "slack/post").unwrap());
        assert!(!s.is_held("slack/post").unwrap());
        assert!(s.held_tools().unwrap().is_empty());
        // A tool can be held again after its hold was cleared.
        assert!(s.demote(7, 10, "slack/post").unwrap());
        assert!(s.is_held("slack/post").unwrap());
        let holds: i64 = s
            .db()
            .query_row("SELECT COUNT(*) FROM decisions WHERE kind = 'demoted'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(holds, 2);
    }

    #[test]
    fn counts_start_after_the_last_hold_and_stay_in_the_window() {
        let (_dir, s) = store();
        let post = "slack/post";
        s.add_decision(10, Some(1), post, DecisionKind::Approved).unwrap();
        s.add_decision(11, Some(2), post, DecisionKind::Rejected).unwrap();
        assert!(s.demote(12, 2, post).unwrap());
        s.add_decision(20, Some(3), post, DecisionKind::Approved).unwrap();
        s.add_decision(30, Some(4), post, DecisionKind::Approved).unwrap();
        s.add_decision(31, Some(5), post, DecisionKind::Edited).unwrap();
        s.add_decision(32, Some(6), post, DecisionKind::Rejected).unwrap();
        // Reports and clearings are not reviews.
        assert!(s.add_report(33, 6, post).unwrap());
        assert!(s.clear_hold(34, post).unwrap());
        s.add_decision(35, Some(7), "slack/react", DecisionKind::Approved)
            .unwrap();
        let counts = s.decision_counts(0).unwrap();
        let expected = DecisionCounts {
            approved: 2,
            edited: 1,
            rejected: 1,
            ..Default::default()
        };
        assert_eq!(counts[post], expected);
        assert_eq!(counts[post].reviewed(), 4);
        assert_eq!(counts[post].unchanged_percent(), Some(50));
        assert_eq!(counts["slack/react"].approved, 1);
        // The window starts at `since`, also inside the time after the hold.
        let counts = s.decision_counts(30).unwrap();
        assert_eq!(counts[post].approved, 1);
        assert_eq!(counts[post].reviewed(), 3);
        assert!(s.decision_counts(36).unwrap().is_empty());
        assert_eq!(DecisionCounts::default().unchanged_percent(), None);
    }

    #[test]
    fn an_autonomous_run_fails_only_if_its_call_failed_without_a_copy() {
        let (_dir, s) = store();
        let auto = |key: &str| {
            let item = s
                .add_inbox_item(key, &tool_call("hi"), "asked", &[], ApprovalState::Approved, 1)
                .unwrap();
            s.add_decision(1, Some(item.id), "slack/post", DecisionKind::Autonomous)
                .unwrap();
            item.id
        };
        let ran = auto("k1");
        let failed = auto("k2");
        let not_run = auto("k3");
        for (id, to) in [(ran, ApprovalState::Executed), (failed, ApprovalState::Failed)] {
            assert!(
                s.move_inbox_state(id, ApprovalState::Approved, ApprovalState::Executing, 2)
                    .unwrap()
            );
            assert!(s.move_inbox_state(id, ApprovalState::Executing, to, 2).unwrap());
        }
        // A start did not let this call run: it failed with a pending copy.
        assert!(
            s.fail_with_pending_copy(not_run, ApprovalState::Approved, 3)
                .unwrap()
                .is_some()
        );
        let counts = s.decision_counts(0).unwrap()["slack/post"];
        assert_eq!((counts.autonomous, counts.autonomous_failed), (3, 1));
        assert_eq!(counts.reviewed(), 0);
    }

    #[test]
    fn an_item_is_reported_once() {
        let (_dir, s) = store();
        assert!(s.add_report(1, 4, "slack/post").unwrap());
        assert!(!s.add_report(2, 4, "slack/post").unwrap());
        assert!(s.add_report(3, 5, "slack/post").unwrap());
    }
}
