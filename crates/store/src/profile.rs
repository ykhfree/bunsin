use crate::{Result, StoreError};
use bunsin_core::approval::ApprovalState;
use bunsin_core::context::Correction;
use bunsin_core::records::{ArtifactMeta, AuditEntry, InboxBody, InboxItem, ReadNote};
use bunsin_core::sha256_hex;
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::Value;
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
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS corrections (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    inbox_id INTEGER NOT NULL,
    original TEXT NOT NULL,
    edited TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at INTEGER NOT NULL
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
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    event_key TEXT,
    detail TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS audit_no_update BEFORE UPDATE ON audit
BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS audit_no_delete BEFORE DELETE ON audit
BEGIN SELECT RAISE(ABORT, 'the audit log is append-only'); END;
";

const KILL_SWITCH: &str = "kill_switch";

/// A secret as it is stored: encrypted, with the address it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSecret {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    /// The scheme, host and port the secret was entered for.
    pub origin: String,
}

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

type InboxRow = (i64, String, String, String, String, i64, i64, String);

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
    ))
}

fn inbox_item(raw: InboxRow) -> Result<InboxItem> {
    let (id, event_key, body, rationale, state, created_at, updated_at, basis) = raw;
    Ok(InboxItem {
        id,
        event_key,
        body: serde_json::from_str(&body)?,
        rationale,
        basis: serde_json::from_str(&basis)?,
        state: ApprovalState::parse(&state).ok_or(StoreError::BadState(state))?,
        created_at,
        updated_at,
    })
}

const INBOX_COLUMNS: &str = "id, event_key, body, rationale, state, created_at, updated_at, basis";

impl ProfileStore {
    pub(crate) fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir.join("artifacts").join("sha256"))?;
        let db = Connection::open(dir.join("agent.db"))?;
        db.busy_timeout(crate::BUSY_TIMEOUT)?;
        db.execute_batch(SCHEMA)?;
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

    // --- inbox ---

    pub fn add_inbox_item(
        &self,
        event_key: &str,
        body: &InboxBody,
        rationale: &str,
        basis: &[ReadNote],
        state: ApprovalState,
        now: i64,
    ) -> Result<InboxItem> {
        let db = self.db();
        db.execute(
            "INSERT INTO inbox (event_key, body, rationale, basis, state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            (
                event_key,
                serde_json::to_string(body)?,
                rationale,
                serde_json::to_string(basis)?,
                state.as_str(),
                now,
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
        })
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

    pub fn add_correction(&self, inbox_id: i64, correction: &Correction, now: i64) -> Result<()> {
        self.db().execute(
            "INSERT INTO corrections (inbox_id, original, edited, reason, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                inbox_id,
                &correction.original,
                &correction.edited,
                &correction.reason,
                now,
            ),
        )?;
        Ok(())
    }

    /// Newest first.
    pub fn recent_corrections(&self, limit: u32) -> Result<Vec<Correction>> {
        let db = self.db();
        let mut statement = db.prepare("SELECT original, edited, reason FROM corrections ORDER BY id DESC LIMIT ?1")?;
        let rows = statement.query_map([limit], |row| {
            Ok(Correction {
                original: row.get(0)?,
                edited: row.get(1)?,
                reason: row.get(2)?,
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

    #[test]
    fn corrections_come_back_newest_first() {
        let (_dir, s) = store();
        for (i, reason) in ["too long", "wrong tone", "missing link"].iter().enumerate() {
            let c = Correction {
                original: "o".into(),
                edited: "e".into(),
                reason: (*reason).into(),
            };
            s.add_correction(1, &c, i as i64).unwrap();
        }
        let reasons: Vec<String> = s.recent_corrections(2).unwrap().into_iter().map(|c| c.reason).collect();
        assert_eq!(reasons, ["missing link", "wrong tone"]);
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
}
