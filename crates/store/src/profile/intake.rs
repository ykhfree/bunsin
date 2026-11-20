//! Events that arrived from outside and wait for the agent: the questions of inbound callers and the
//! deliveries of webhooks.
//! The caller gets its answer at once; a worker hands each row to the agent later.

use super::{INBOX_COLUMNS, ProfileStore, inbox_from_row, inbox_item};
use crate::Result;
use bunsin_core::event::InboundEvent;
use bunsin_core::records::InboxItem;
use rusqlite::{Connection, OptionalExtension};

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
    context_id TEXT,
    message_id TEXT,
    canceled_at INTEGER,
    UNIQUE (channel, delivery_id)
);
CREATE INDEX IF NOT EXISTS intake_caller ON intake (caller_id, received_at);
CREATE INDEX IF NOT EXISTS intake_event ON intake (event_key);
";

/// Adds the columns of A2A to a table from before them, then the index that needs them.
/// It runs after `SCHEMA`, which leaves an old table as it is.
pub(super) fn migrate(db: &Connection) -> Result<()> {
    let has_message: bool = db.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_table_info('intake') WHERE name = 'message_id')",
        [],
        |row| row.get(0),
    )?;
    if !has_message {
        // Older rows have no message ID and no context, and none was canceled.
        db.execute_batch(
            "BEGIN;
             ALTER TABLE intake ADD COLUMN context_id TEXT;
             ALTER TABLE intake ADD COLUMN message_id TEXT;
             ALTER TABLE intake ADD COLUMN canceled_at INTEGER;
             COMMIT;",
        )?;
    }
    // A caller's message is stored once, so a repeated send finds the first request.
    db.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS intake_message ON intake (caller_id, message_id)
         WHERE message_id IS NOT NULL;",
    )?;
    Ok(())
}

/// One stored event that waits for the agent, or was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeRow {
    pub id: i64,
    /// The request ID of a question, or the delivery ID of a webhook.
    pub delivery_id: String,
    /// The stored form of the event: after the privacy filter.
    pub event: InboundEvent,
    pub event_key: String,
    pub received_at: i64,
    /// How many times the agent was given the event.
    pub attempts: u32,
    /// `None` until the handling ends.
    pub outcome: Option<String>,
    /// The context that an A2A caller gave the question. `None` for other rows.
    pub context_id: Option<String>,
    /// When the caller canceled the question. A canceled row is not handed to the agent again.
    pub canceled_at: Option<i64>,
}

const INTAKE_COLUMNS: &str =
    "id, delivery_id, event, event_key, received_at, attempts, outcome, context_id, canceled_at";

type RawIntake = (
    i64,
    String,
    String,
    String,
    i64,
    u32,
    Option<String>,
    Option<String>,
    Option<i64>,
);

fn intake_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawIntake> {
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
    ))
}

fn intake(raw: RawIntake) -> Result<IntakeRow> {
    let (id, delivery_id, event, event_key, received_at, attempts, outcome, context_id, canceled_at) = raw;
    Ok(IntakeRow {
        id,
        delivery_id,
        event: serde_json::from_str(&event)?,
        event_key,
        received_at,
        attempts,
        outcome,
        context_id,
        canceled_at,
    })
}

/// The A2A message that a question came in, if it came in one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntakeMessage<'a> {
    pub message_id: Option<&'a str>,
    pub context_id: Option<&'a str>,
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
        self.add_message_intake(channel, delivery_id, caller_id, IntakeMessage::default(), event, now)
    }

    /// Like `add_intake`, with the IDs of the caller's message. It also returns false if the caller
    /// already sent a message with the same message ID.
    pub fn add_message_intake(
        &self,
        channel: &str,
        delivery_id: &str,
        caller_id: Option<i64>,
        message: IntakeMessage<'_>,
        event: &InboundEvent,
        now: i64,
    ) -> Result<bool> {
        let inserted = self.db().execute(
            "INSERT OR IGNORE INTO intake (channel, delivery_id, caller_id, event, event_key, received_at, next_try_at,
                                           message_id, context_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
            (
                channel,
                delivery_id,
                caller_id,
                serde_json::to_string(event)?,
                event.dedupe_key(),
                now,
                message.message_id,
                message.context_id,
            ),
        )?;
        Ok(inserted == 1)
    }

    /// How many events the caller sent after `after`, canceled ones included. Rows are kept, so this
    /// survives a restart.
    pub fn intake_count_by_caller(&self, caller_id: i64, after: i64) -> Result<u32> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM intake WHERE caller_id = ?1 AND received_at > ?2",
            (caller_id, after),
            |row| row.get(0),
        )?)
    }

    /// How many events of `channel` wait for the agent. A canceled event does not wait.
    pub fn open_intake_count(&self, channel: &str) -> Result<u32> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM intake WHERE channel = ?1 AND outcome IS NULL AND canceled_at IS NULL",
            [channel],
            |row| row.get(0),
        )?)
    }

    /// Deletes the handled rows of `channel` that arrived before `before`. A delivery repeated after
    /// that is stored again; the event key still keeps it from being handled twice.
    pub fn prune_intake(&self, channel: &str, before: i64) -> Result<usize> {
        Ok(self.db().execute(
            "DELETE FROM intake WHERE channel = ?1 AND outcome IS NOT NULL AND received_at < ?2",
            (channel, before),
        )?)
    }

    /// The events that wait and whose next try is due, oldest first, at most `limit`.
    /// A canceled event is not due.
    pub fn due_intake(&self, now: i64, limit: u32) -> Result<Vec<IntakeRow>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {INTAKE_COLUMNS} FROM intake
             WHERE outcome IS NULL AND canceled_at IS NULL AND next_try_at <= ?1 ORDER BY id LIMIT ?2"
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

    /// Marks the row as canceled at `now`. Returns false if it was canceled before; the first time stays.
    pub fn cancel_intake(&self, id: i64, now: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE intake SET canceled_at = ?1 WHERE id = ?2 AND canceled_at IS NULL",
            (now, id),
        )?;
        Ok(changed == 1)
    }

    /// The row of `delivery_id` in `channel`, only if `caller_id` sent it. Another caller's row and
    /// a row that does not exist give the same `None`.
    pub fn caller_intake(&self, channel: &str, delivery_id: &str, caller_id: i64) -> Result<Option<IntakeRow>> {
        self.one_intake(
            "channel = ?1 AND delivery_id = ?2 AND caller_id = ?3",
            (channel, delivery_id, caller_id),
        )
    }

    /// The row that `caller_id` stored with the message ID `message_id` in `channel`.
    pub fn caller_message_intake(&self, channel: &str, caller_id: i64, message_id: &str) -> Result<Option<IntakeRow>> {
        self.one_intake(
            "channel = ?1 AND caller_id = ?2 AND message_id = ?3",
            (channel, caller_id, message_id),
        )
    }

    /// The row of `channel` for the event with this key.
    pub fn intake_for_event(&self, channel: &str, event_key: &str) -> Result<Option<IntakeRow>> {
        self.one_intake("channel = ?1 AND event_key = ?2", (channel, event_key))
    }

    fn one_intake(&self, filter: &str, params: impl rusqlite::Params) -> Result<Option<IntakeRow>> {
        let raw = self
            .db()
            .query_row(
                &format!("SELECT {INTAKE_COLUMNS} FROM intake WHERE {filter}"),
                params,
                intake_row,
            )
            .optional()?;
        raw.map(intake).transpose()
    }

    /// The newest rows that `caller_id` stored in `channel`, newest first, at most `limit`.
    pub fn caller_intakes(&self, channel: &str, caller_id: i64, limit: u32) -> Result<Vec<IntakeRow>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {INTAKE_COLUMNS} FROM intake WHERE channel = ?1 AND caller_id = ?2
             ORDER BY received_at DESC, id DESC LIMIT ?3"
        ))?;
        let rows = statement
            .query_map((channel, caller_id, limit), intake_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(intake).collect()
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
    use super::IntakeMessage;
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

    fn message<'a>(message_id: &'a str, context_id: Option<&'a str>) -> IntakeMessage<'a> {
        IntakeMessage {
            message_id: Some(message_id),
            context_id,
        }
    }

    #[test]
    fn a_canceled_row_is_neither_due_nor_open() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        for id in ["req_1", "req_2"] {
            store
                .add_intake("bunsin.inbound", id, Some(7), &event(id), 100)
                .unwrap();
        }
        let first = store.caller_intake("bunsin.inbound", "req_1", 7).unwrap().unwrap();
        assert_eq!(first.canceled_at, None);
        assert!(store.cancel_intake(first.id, 150).unwrap());
        // A second cancel keeps the first time.
        assert!(!store.cancel_intake(first.id, 160).unwrap());
        let row = store.caller_intake("bunsin.inbound", "req_1", 7).unwrap().unwrap();
        assert_eq!(row.canceled_at, Some(150));
        let due: Vec<String> = store
            .due_intake(1_000, 10)
            .unwrap()
            .into_iter()
            .map(|row| row.delivery_id)
            .collect();
        assert_eq!(due, ["req_2"]);
        assert_eq!(store.open_intake_count("bunsin.inbound").unwrap(), 1);
        // The ask limit still counts it: a cancel does not give the caller a new question.
        assert_eq!(store.intake_count_by_caller(7, 0).unwrap(), 2);
        // A canceled row that the agent already handled is not open either.
        let second = store.caller_intake("bunsin.inbound", "req_2", 7).unwrap().unwrap();
        store.finish_intake(second.id, "pending_approval").unwrap();
        assert_eq!(store.open_intake_count("bunsin.inbound").unwrap(), 0);
        assert!(store.due_intake(1_000, 10).unwrap().is_empty());
    }

    #[test]
    fn a_message_id_is_stored_once_per_caller() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        let add = |id: &str, caller, msg| {
            store
                .add_message_intake("bunsin.inbound", id, Some(caller), msg, &event(id), 100)
                .unwrap()
        };
        assert!(add("req_1", 7, message("m-1", Some("ctx"))));
        // The same message ID of the same caller is refused, under any request ID.
        assert!(!add("req_2", 7, message("m-1", None)));
        // Another caller may use it, and rows without a message ID never clash.
        assert!(add("req_3", 8, message("m-1", None)));
        assert!(add("req_4", 7, IntakeMessage::default()));
        assert!(add("req_5", 7, IntakeMessage::default()));
        let row = store
            .caller_message_intake("bunsin.inbound", 7, "m-1")
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.delivery_id.as_str(), row.context_id.as_deref()),
            ("req_1", Some("ctx"))
        );
        assert!(
            store
                .caller_message_intake("bunsin.inbound", 7, "m-2")
                .unwrap()
                .is_none()
        );
        assert!(store.caller_message_intake("hook", 7, "m-1").unwrap().is_none());
        // The newest first.
        let ids: Vec<String> = store
            .caller_intakes("bunsin.inbound", 7, 2)
            .unwrap()
            .into_iter()
            .map(|row| row.delivery_id)
            .collect();
        assert_eq!(ids, ["req_5", "req_4"]);
        let found = store
            .intake_for_event("bunsin.inbound", &event("req_3").dedupe_key())
            .unwrap()
            .unwrap();
        assert_eq!(found.delivery_id, "req_3");
    }

    #[test]
    fn old_intake_tables_get_the_new_columns() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let path = app.open_profile("kim").unwrap().dir.join("agent.db");
        // The table as an older version wrote it, with one stored question.
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(
            "DROP INDEX intake_message;
             DROP TABLE intake;
             CREATE TABLE intake (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, channel TEXT NOT NULL, delivery_id TEXT NOT NULL,
                 caller_id INTEGER, event TEXT NOT NULL, event_key TEXT NOT NULL, received_at INTEGER NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0, next_try_at INTEGER NOT NULL, outcome TEXT,
                 UNIQUE (channel, delivery_id));",
        )
        .unwrap();
        old.execute(
            "INSERT INTO intake (channel, delivery_id, caller_id, event, event_key, received_at, next_try_at)
             VALUES ('bunsin.inbound', 'req_old', 7, ?1, 'k', 50, 50)",
            [serde_json::to_string(&event("req_old")).unwrap()],
        )
        .unwrap();
        drop(old);

        let store = app.open_profile("kim").unwrap();
        let row = store.caller_intake("bunsin.inbound", "req_old", 7).unwrap().unwrap();
        assert_eq!((row.context_id, row.canceled_at), (None, None));
        assert_eq!(store.due_intake(100, 10).unwrap().len(), 1);
        assert!(
            store
                .add_message_intake(
                    "bunsin.inbound",
                    "req_new",
                    Some(7),
                    message("m-1", None),
                    &event("n"),
                    60
                )
                .unwrap()
        );
        // The unique index exists after the change.
        assert!(
            !store
                .add_message_intake(
                    "bunsin.inbound",
                    "req_x",
                    Some(7),
                    message("m-1", None),
                    &event("x"),
                    61
                )
                .unwrap()
        );
        // A second open finds the columns and changes nothing.
        drop(store);
        let store = app.open_profile("kim").unwrap();
        assert!(
            store
                .caller_message_intake("bunsin.inbound", 7, "m-1")
                .unwrap()
                .is_some()
        );
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
        assert_eq!(store.open_intake_count("bunsin.inbound").unwrap(), 1);
        assert_eq!(store.open_intake_count("hook").unwrap(), 1);
        assert!(store.due_intake(10_000, 10).unwrap().iter().all(|due| due.id != row.id));

        // Only the handled rows of the channel go, and only the old ones.
        assert_eq!(store.prune_intake("hook", 10_000).unwrap(), 0);
        assert_eq!(store.prune_intake("bunsin.inbound", 100).unwrap(), 0);
        assert_eq!(store.prune_intake("bunsin.inbound", 101).unwrap(), 1);
        assert!(store.caller_intake("bunsin.inbound", "req_1", 7).unwrap().is_none());
        assert!(store.caller_intake("bunsin.inbound", "req_2", 7).unwrap().is_some());
    }
}
