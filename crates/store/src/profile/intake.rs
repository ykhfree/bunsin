//! Events that arrived from outside and wait for the agent: the questions of inbound callers.
//! The caller gets its answer at once; a worker hands each row to the agent later.

use super::{INBOX_COLUMNS, ProfileStore, inbox_from_row, inbox_item};
use crate::Result;
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxItem;
use rusqlite::OptionalExtension;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS intake (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    channel TEXT NOT NULL,
    delivery_id TEXT NOT NULL,
    caller_id INTEGER,
    event TEXT NOT NULL,
    event_key TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_try_at INTEGER NOT NULL,
    outcome TEXT,
    UNIQUE (channel, delivery_id)
);
CREATE INDEX IF NOT EXISTS intake_caller ON intake (caller_id, received_at);
CREATE INDEX IF NOT EXISTS intake_event ON intake (event_key);
";

/// One stored event that waits for the agent, or was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeRow {
    pub id: i64,
    /// The stored form of the event: after the privacy filter.
    pub event: InboundEvent,
    pub event_key: String,
    /// How many times the agent was given the event.
    pub attempts: u32,
    /// `None` until the handling ends.
    pub outcome: Option<String>,
}

const INTAKE_COLUMNS: &str = "id, event, event_key, attempts, outcome";

fn intake_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, String, u32, Option<String>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn intake(raw: (i64, String, String, u32, Option<String>)) -> Result<IntakeRow> {
    let (id, event, event_key, attempts, outcome) = raw;
    Ok(IntakeRow {
        id,
        event: serde_json::from_str(&event)?,
        event_key,
        attempts,
        outcome,
    })
}

impl ProfileStore {
    /// Stores an event of `channel` that waits for the agent. Returns false if the channel already
    /// has a row with `delivery_id`: a delivery is stored once.
    pub fn add_intake(
        &self,
        channel: &str,
        delivery_id: &str,
        caller_id: Option<i64>,
        event: &InboundEvent,
        now: i64,
    ) -> Result<bool> {
        let inserted = self.db().execute(
            "INSERT OR IGNORE INTO intake (channel, delivery_id, caller_id, event, event_key, received_at, next_try_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            (
                channel,
                delivery_id,
                caller_id,
                serde_json::to_string(event)?,
                event.dedupe_key(),
                now,
            ),
        )?;
        Ok(inserted == 1)
    }

    /// How many events the caller sent after `after`. Rows are kept, so this survives a restart.
    pub fn intake_count_by_caller(&self, caller_id: i64, after: i64) -> Result<u32> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM intake WHERE caller_id = ?1 AND received_at > ?2",
            (caller_id, after),
            |row| row.get(0),
        )?)
    }

    /// How many events wait for the agent.
    pub fn open_intake_count(&self) -> Result<u32> {
        Ok(self
            .db()
            .query_row("SELECT COUNT(*) FROM intake WHERE outcome IS NULL", [], |row| {
                row.get(0)
            })?)
    }

    /// The events that wait and whose next try is due, oldest first, at most `limit`.
    pub fn due_intake(&self, now: i64, limit: u32) -> Result<Vec<IntakeRow>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {INTAKE_COLUMNS} FROM intake WHERE outcome IS NULL AND next_try_at <= ?1 ORDER BY id LIMIT ?2"
        ))?;
        let rows = statement
            .query_map((now, limit), intake_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(intake).collect()
    }

    /// Counts a try that did not end the handling and sets when the next one is due.
    pub fn intake_tried(&self, id: i64, next_try_at: i64) -> Result<()> {
        self.db().execute(
            "UPDATE intake SET attempts = attempts + 1, next_try_at = ?1 WHERE id = ?2",
            (next_try_at, id),
        )?;
        Ok(())
    }

    /// Ends the handling of an event with `outcome`. It is not given to the agent again.
    pub fn finish_intake(&self, id: i64, outcome: &str) -> Result<()> {
        self.db().execute(
            "UPDATE intake SET attempts = attempts + 1, outcome = ?1 WHERE id = ?2",
            (outcome, id),
        )?;
        Ok(())
    }

    /// The row of `delivery_id` in `channel`, only if `caller_id` sent it. Another caller's row and
    /// a row that does not exist give the same `None`.
    pub fn caller_intake(&self, channel: &str, delivery_id: &str, caller_id: i64) -> Result<Option<IntakeRow>> {
        let raw = self
            .db()
            .query_row(
                &format!(
                    "SELECT {INTAKE_COLUMNS} FROM intake WHERE channel = ?1 AND delivery_id = ?2 AND caller_id = ?3"
                ),
                (channel, delivery_id, caller_id),
                intake_row,
            )
            .optional()?;
        raw.map(intake).transpose()
    }

    /// True if `channel` holds a row for the event with this key.
    pub fn has_intake(&self, channel: &str, event_key: &str) -> Result<bool> {
        Ok(self.db().query_row(
            "SELECT EXISTS (SELECT 1 FROM intake WHERE channel = ?1 AND event_key = ?2)",
            (channel, event_key),
            |row| row.get(0),
        )?)
    }

    /// The inbox items of one event, oldest first.
    pub fn inbox_for_event(&self, event_key: &str) -> Result<Vec<InboxItem>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {INBOX_COLUMNS} FROM inbox WHERE event_key = ?1 ORDER BY id"
        ))?;
        let rows = statement
            .query_map([event_key], inbox_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(inbox_item).collect()
    }
}

#[cfg(test)]
mod tests {
    use crate::AppStore;
    use bunsin_core::event::InboundEvent;

    fn event(id: &str) -> InboundEvent {
        InboundEvent {
            source: "bunsin.inbound".into(),
            external_id: id.into(),
            thread: None,
            author: "caller:bot".into(),
            text: "When do we deploy?".into(),
            channel: Default::default(),
        }
    }

    #[test]
    fn an_intake_row_is_stored_once_and_found_only_by_its_caller() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        assert!(
            store
                .add_intake("bunsin.inbound", "req_1", Some(7), &event("req_1"), 100)
                .unwrap()
        );
        assert!(
            !store
                .add_intake("bunsin.inbound", "req_1", Some(8), &event("req_1"), 101)
                .unwrap()
        );
        // Another channel may use the same delivery ID.
        assert!(store.add_intake("hook", "req_1", None, &event("h1"), 102).unwrap());
        assert!(
            store
                .add_intake("bunsin.inbound", "req_2", Some(7), &event("req_2"), 3_700)
                .unwrap()
        );

        assert_eq!(store.intake_count_by_caller(7, 0).unwrap(), 2);
        assert_eq!(store.intake_count_by_caller(7, 100).unwrap(), 1);
        assert_eq!(store.intake_count_by_caller(8, 0).unwrap(), 0);
        let row = store.caller_intake("bunsin.inbound", "req_1", 7).unwrap().unwrap();
        assert_eq!(row.event, event("req_1"));
        assert_eq!((row.attempts, row.outcome.as_deref()), (0, None));
        assert!(store.caller_intake("bunsin.inbound", "req_1", 8).unwrap().is_none());
        assert!(store.caller_intake("bunsin.inbound", "req_9", 7).unwrap().is_none());
        assert!(
            store
                .has_intake("bunsin.inbound", &event("req_1").dedupe_key())
                .unwrap()
        );
        assert!(!store.has_intake("bunsin.inbound", &event("h1").dedupe_key()).unwrap());

        // A try that failed waits until its next time. A finished row is not due again.
        assert_eq!(store.due_intake(200, 10).unwrap().len(), 2);
        store.intake_tried(row.id, 300).unwrap();
        assert_eq!(store.due_intake(200, 10).unwrap().len(), 1);
        assert_eq!(store.due_intake(300, 1).unwrap()[0].id, row.id);
        store.finish_intake(row.id, "ignored").unwrap();
        let row = store.caller_intake("bunsin.inbound", "req_1", 7).unwrap().unwrap();
        assert_eq!((row.attempts, row.outcome.as_deref()), (2, Some("ignored")));
        assert_eq!(store.open_intake_count().unwrap(), 2);
        assert!(store.due_intake(10_000, 10).unwrap().iter().all(|due| due.id != row.id));
    }
}
