//! The index of the ARD registry in `<data>/registry.db` (ARCHITECTURE 18).
//!
//! A source is a place that entries come from. Its kind and scope decide which listener sees
//! its entries. A successful read of a source replaces all its entries and refusals at once.

use crate::{BUSY_TIMEOUT, Result, StoreError, term_hits};
use bunsin_core::inbound::InboundScope;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    scope TEXT NOT NULL,
    next_fetch_at INTEGER NOT NULL DEFAULT 0,
    last_ok_at INTEGER,
    failures INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id INTEGER NOT NULL,
    identifier TEXT NOT NULL,
    on_internal INTEGER NOT NULL,
    on_public INTEGER NOT NULL,
    publisher TEXT NOT NULL,
    display_name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    entry TEXT NOT NULL,
    terms TEXT NOT NULL,
    search_text TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    changed_at INTEGER NOT NULL,
    UNIQUE (source_id, identifier)
);
CREATE TABLE IF NOT EXISTS refusals (
    source_id INTEGER NOT NULL,
    identifier TEXT NOT NULL,
    reason TEXT NOT NULL
);
";

/// The full-text index of `entries.search_text`. It reads the text from `entries` by `id`.
const ENTRIES_FTS: &str = "
CREATE VIRTUAL TABLE entries_fts USING fts5(
    search_text, content = 'entries', content_rowid = 'id', tokenize = 'trigram'
);
CREATE TRIGGER entries_fts_insert AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts (rowid, search_text) VALUES (new.id, new.search_text);
END;
CREATE TRIGGER entries_fts_delete AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
END;
CREATE TRIGGER entries_fts_update AFTER UPDATE OF search_text ON entries BEGIN
    INSERT INTO entries_fts (entries_fts, rowid, search_text) VALUES ('delete', old.id, old.search_text);
    INSERT INTO entries_fts (rowid, search_text) VALUES (new.id, new.search_text);
END;
INSERT INTO entries_fts (entries_fts) VALUES ('rebuild');
";

/// Where the entries of a source come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// The operator named it with `--ard-registry-source` or `--ard-registry-public-source`.
    Seed,
    /// The catalog of the server's own listener.
    SelfCatalog,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Seed => "seed",
            SourceKind::SelfCatalog => "self",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "seed" => Ok(SourceKind::Seed),
            "self" => Ok(SourceKind::SelfCatalog),
            other => Err(StoreError::BadState(other.to_owned())),
        }
    }
}

fn scope_text(scope: InboundScope) -> &'static str {
    match scope {
        InboundScope::Internal => "internal",
        InboundScope::Public => "public",
    }
}

fn parse_scope(text: &str) -> Result<InboundScope> {
    match text {
        "internal" => Ok(InboundScope::Internal),
        "public" => Ok(InboundScope::Public),
        other => Err(StoreError::BadState(other.to_owned())),
    }
}

/// The column that says whether the listener of `listener` sees a row.
fn visible_column(listener: InboundScope) -> &'static str {
    match listener {
        InboundScope::Internal => "on_internal",
        InboundScope::Public => "on_public",
    }
}

/// One entry to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEntry {
    pub identifier: String,
    /// The `<publisher>` of the identifier, in lower case.
    pub publisher: String,
    pub display_name: String,
    pub media_type: String,
    /// The entry as the source gave it.
    pub entry: Value,
    /// The top-level terms by their IRI, with their values as the source gave them.
    pub terms: Map<String, Value>,
    /// The text that the full-text index holds.
    pub search_text: String,
}

/// An entry that the registry did not store, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub identifier: String,
    /// A short fixed word, for example `invalid`. The status counts refusals by it.
    pub reason: String,
}

/// One stored entry, without its body.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedRow {
    pub id: i64,
    pub identifier: String,
    pub publisher: String,
    pub display_name: String,
    pub media_type: String,
    pub terms: Map<String, Value>,
    /// When the registry first stored this identifier from this source. Unix seconds.
    pub first_seen_at: i64,
    /// When the stored entry last changed. Unix seconds.
    pub changed_at: i64,
}

/// One source as the operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceStatus {
    pub url: String,
    pub kind: SourceKind,
    pub scope: InboundScope,
    pub entries: u64,
    pub last_ok_at: Option<i64>,
    /// When the crawl reads the source next. Unix seconds. A self catalog is read on every tick.
    pub next_fetch_at: i64,
    pub failures: u32,
    pub last_error: Option<String>,
    /// Each reason of refusal with its count, by reason.
    pub refusals: Vec<(String, u64)>,
}

/// The registry index.
pub struct RegistryStore {
    db: Mutex<Connection>,
}

impl RegistryStore {
    /// Opens `registry.db` in the data directory `root`, and creates what is missing.
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let mut db = Connection::open(root.join("registry.db"))?;
        // The server and the operator command `registry-status` are separate processes.
        db.busy_timeout(BUSY_TIMEOUT)?;
        db.execute_batch(SCHEMA)?;
        // One transaction holds the check and the fill, so the index is never half made.
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'entries_fts')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            tx.execute_batch(ENTRIES_FTS)?;
        }
        tx.commit()?;
        Ok(Self { db: Mutex::new(db) })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().expect("registry lock")
    }

    /// The ID of the source at `url`. A new source is added with `kind` and `scope`.
    pub fn source(&self, url: &str, kind: SourceKind, scope: InboundScope) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT OR IGNORE INTO sources (url, kind, scope) VALUES (?1, ?2, ?3)",
            (url, kind.as_str(), scope_text(scope)),
        )?;
        Ok(db.query_row("SELECT id FROM sources WHERE url = ?1", [url], |row| row.get(0))?)
    }

    /// How many entries the seed sources other than `source` hold. Self catalogs are not counted.
    pub fn seed_entries_outside(&self, source: i64) -> Result<usize> {
        let count: i64 = self.db().query_row(
            "SELECT COUNT(*) FROM entries e JOIN sources s ON s.id = e.source_id
             WHERE s.kind = 'seed' AND e.source_id != ?1",
            [source],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Replaces the entries and the refusals of `source`, and records a successful read at `now`.
    ///
    /// An identifier that the source had before keeps its `first_seen_at`, and keeps its
    /// `changed_at` unless its entry changed. The kind and scope of the source decide who sees the
    /// rows: a public source is seen on the public listener, and every source except the public
    /// self catalog on the internal one. The internal self catalog lists public profiles itself.
    pub fn replace(&self, source: i64, entries: &[NewEntry], refusals: &[Refusal], now: i64) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (kind, scope): (String, String) = tx
            .query_row("SELECT kind, scope FROM sources WHERE id = ?1", [source], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?
            .ok_or(StoreError::NotFound("registry source", source))?;
        let public = parse_scope(&scope)? == InboundScope::Public;
        let on_internal = !(public && SourceKind::parse(&kind)? == SourceKind::SelfCatalog);
        let mut known: HashMap<String, (i64, i64, String)> = HashMap::new();
        {
            let mut statement =
                tx.prepare("SELECT identifier, first_seen_at, changed_at, entry FROM entries WHERE source_id = ?1")?;
            let rows = statement.query_map([source], |row| {
                Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?, row.get(3)?)))
            })?;
            for row in rows {
                let (identifier, old) = row?;
                known.insert(identifier, old);
            }
        }
        tx.execute("DELETE FROM entries WHERE source_id = ?1", [source])?;
        tx.execute("DELETE FROM refusals WHERE source_id = ?1", [source])?;
        for entry in entries {
            let body = serde_json::to_string(&entry.entry)?;
            let (first_seen_at, changed_at) = match known.get(&entry.identifier) {
                Some((first, changed, old)) if *old == body => (*first, *changed),
                Some((first, _, _)) => (*first, now),
                None => (now, now),
            };
            tx.execute(
                "INSERT INTO entries (source_id, identifier, on_internal, on_public, publisher, display_name,
                     media_type, entry, terms, search_text, first_seen_at, changed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    source,
                    entry.identifier,
                    on_internal,
                    public,
                    entry.publisher,
                    entry.display_name,
                    entry.media_type,
                    body,
                    serde_json::to_string(&entry.terms)?,
                    entry.search_text,
                    first_seen_at,
                    changed_at,
                ],
            )?;
        }
        for refusal in refusals {
            tx.execute(
                "INSERT INTO refusals (source_id, identifier, reason) VALUES (?1, ?2, ?3)",
                (source, &refusal.identifier, &refusal.reason),
            )?;
        }
        tx.execute(
            "UPDATE sources SET last_ok_at = ?1, failures = 0, last_error = NULL WHERE id = ?2",
            (now, source),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records a failed read of `source`. Its entries stay. `error` is Bunsin text and an HTTP status only.
    /// Returns the count of failures since the last success, and the time of that success.
    pub fn record_failure(&self, source: i64, error: &str) -> Result<(u32, Option<i64>)> {
        Ok(self.db().query_row(
            "UPDATE sources SET failures = failures + 1, last_error = ?1 WHERE id = ?2
             RETURNING failures, last_ok_at",
            (error, source),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }

    /// Sets when the crawl reads `source` next.
    pub fn set_next_fetch(&self, source: i64, at: i64) -> Result<()> {
        self.db()
            .execute("UPDATE sources SET next_fetch_at = ?1 WHERE id = ?2", (at, source))?;
        Ok(())
    }

    /// Removes the entries of `source`. Its refusals and its read times stay.
    pub fn clear_entries(&self, source: i64) -> Result<()> {
        self.db()
            .execute("DELETE FROM entries WHERE source_id = ?1", [source])?;
        Ok(())
    }

    /// Makes the seed sources the ones in `seeds`, each a seed text and a scope. A seed that is
    /// gone, or that changed its scope, loses its row, its entries and its refusals. A new seed is
    /// due at once.
    pub fn set_seeds(&self, seeds: &[(String, InboundScope)]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let known: Vec<(i64, String, String)> = tx
            .prepare("SELECT id, url, scope FROM sources WHERE kind = 'seed'")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, url, scope) in known {
            let kept = seeds
                .iter()
                .any(|(seed, seed_scope)| *seed == url && scope_text(*seed_scope) == scope);
            if !kept {
                tx.execute("DELETE FROM entries WHERE source_id = ?1", [id])?;
                tx.execute("DELETE FROM refusals WHERE source_id = ?1", [id])?;
                tx.execute("DELETE FROM sources WHERE id = ?1", [id])?;
            }
        }
        for (seed, scope) in seeds {
            tx.execute(
                "INSERT OR IGNORE INTO sources (url, kind, scope) VALUES (?1, 'seed', ?2)",
                (seed, scope_text(*scope)),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The seed source that is due first at `now`: its ID and its seed text. `None` if none is due.
    pub fn due_seed(&self, now: i64) -> Result<Option<(i64, String)>> {
        Ok(self
            .db()
            .query_row(
                "SELECT id, url FROM sources WHERE kind = 'seed' AND next_fetch_at <= ?1
                 ORDER BY next_fetch_at, id LIMIT 1",
                [now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    /// Starts a read that holds the lock of the index until it is dropped. The rows do not change
    /// during the read, so the row IDs of one call are still there in the next one.
    pub fn read(&self) -> RegistryRead<'_> {
        RegistryRead { db: self.db() }
    }

    /// Every source with its counts, by ID.
    pub fn status(&self) -> Result<Vec<SourceStatus>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT s.id, s.url, s.kind, s.scope, s.last_ok_at, s.failures, s.last_error,
                    (SELECT COUNT(*) FROM entries e WHERE e.source_id = s.id), s.next_fetch_at
             FROM sources s ORDER BY s.id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut reasons =
            db.prepare("SELECT reason, COUNT(*) FROM refusals WHERE source_id = ?1 GROUP BY reason ORDER BY reason")?;
        rows.into_iter()
            .map(
                |(id, url, kind, scope, last_ok_at, failures, last_error, entries, next_fetch_at)| {
                    let refusals = reasons
                        .query_map([id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(SourceStatus {
                        url,
                        kind: SourceKind::parse(&kind)?,
                        scope: parse_scope(&scope)?,
                        entries: entries as u64,
                        last_ok_at,
                        next_fetch_at,
                        failures,
                        last_error,
                        refusals,
                    })
                },
            )
            .collect()
    }
}

/// A read of the index under one hold of its lock (see [`RegistryStore::read`]).
pub struct RegistryRead<'a> {
    db: MutexGuard<'a, Connection>,
}

impl RegistryRead<'_> {
    /// The rows that the listener of `listener` sees, one per identifier, by identifier.
    /// Of rows with the same identifier, the one of a self catalog wins, then the one of the
    /// smallest source ID.
    pub fn visible(&self, listener: InboundScope) -> Result<Vec<IndexedRow>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT e.id, e.identifier, e.publisher, e.display_name, e.media_type, e.terms,
                    e.first_seen_at, e.changed_at
             FROM entries e JOIN sources s ON s.id = e.source_id
             WHERE e.{} = 1
             ORDER BY e.identifier, s.kind = 'self' DESC, e.source_id",
            visible_column(listener)
        ))?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        let mut out: Vec<IndexedRow> = Vec::new();
        for row in rows {
            let (id, identifier, publisher, display_name, media_type, terms, first_seen_at, changed_at) = row?;
            if out.last().is_some_and(|last| last.identifier == identifier) {
                continue;
            }
            out.push(IndexedRow {
                id,
                identifier,
                publisher,
                display_name,
                media_type,
                terms: serde_json::from_str(&terms)?,
                first_seen_at,
                changed_at,
            });
        }
        Ok(out)
    }

    /// For each term of `search::query_terms`, the IDs of the rows that the listener of
    /// `listener` sees and whose search text holds the term.
    pub fn term_hits(&self, listener: InboundScope, terms: &[String]) -> Result<Vec<BTreeSet<i64>>> {
        let column = visible_column(listener);
        term_hits(
            &self.db,
            terms,
            &format!(
                "SELECT e.id FROM entries_fts f JOIN entries e ON e.id = f.rowid
                 WHERE entries_fts MATCH ?1 AND e.{column} = 1"
            ),
            &format!("SELECT id FROM entries WHERE search_text LIKE ?1 AND {column} = 1"),
        )
    }

    /// The stored entries of the rows `ids`, in the same order.
    pub fn bodies(&self, ids: &[i64]) -> Result<Vec<Value>> {
        let mut statement = self.db.prepare_cached("SELECT entry FROM entries WHERE id = ?1")?;
        ids.iter()
            .map(|id| {
                let text: String = statement.query_row([id], |row| row.get(0))?;
                Ok(serde_json::from_str(&text)?)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(identifier: &str, text: &str) -> NewEntry {
        NewEntry {
            identifier: identifier.into(),
            publisher: "acme.com".into(),
            display_name: identifier.into(),
            media_type: "application/mcp-server-card+json".into(),
            entry: json!({"identifier": identifier, "description": text}),
            terms: Map::new(),
            search_text: text.into(),
        }
    }

    fn identifiers(store: &RegistryStore, listener: InboundScope) -> Vec<String> {
        store
            .read()
            .visible(listener)
            .unwrap()
            .into_iter()
            .map(|row| row.identifier)
            .collect()
    }

    #[test]
    fn a_new_read_keeps_the_first_time_and_moves_the_change_time_only_on_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        let source = store
            .source("self:internal", SourceKind::SelfCatalog, InboundScope::Internal)
            .unwrap();
        store
            .replace(
                source,
                &[
                    entry("urn:air:acme.com:a", "deploy"),
                    entry("urn:air:acme.com:b", "review"),
                ],
                &[],
                100,
            )
            .unwrap();
        store
            .replace(
                source,
                &[
                    entry("urn:air:acme.com:a", "deploy"),
                    entry("urn:air:acme.com:b", "changed"),
                ],
                &[],
                200,
            )
            .unwrap();
        let rows = store.read().visible(InboundScope::Internal).unwrap();
        let times: Vec<(i64, i64)> = rows.iter().map(|row| (row.first_seen_at, row.changed_at)).collect();
        assert_eq!(times, [(100, 100), (100, 200)]);
        // An identifier that leaves the source leaves the index; one that comes back is new.
        store
            .replace(source, &[entry("urn:air:acme.com:b", "changed")], &[], 300)
            .unwrap();
        assert_eq!(identifiers(&store, InboundScope::Internal), ["urn:air:acme.com:b"]);
        store
            .replace(
                source,
                &[
                    entry("urn:air:acme.com:a", "deploy"),
                    entry("urn:air:acme.com:b", "changed"),
                ],
                &[],
                400,
            )
            .unwrap();
        let rows = store.read().visible(InboundScope::Internal).unwrap();
        assert_eq!((rows[0].first_seen_at, rows[0].changed_at), (400, 400));
        assert_eq!((rows[1].first_seen_at, rows[1].changed_at), (100, 200));
    }

    #[test]
    fn the_kind_and_scope_of_a_source_decide_which_listener_sees_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        for (url, kind, scope) in [
            ("self:internal", SourceKind::SelfCatalog, InboundScope::Internal),
            ("self:public", SourceKind::SelfCatalog, InboundScope::Public),
            ("inner.example", SourceKind::Seed, InboundScope::Internal),
            ("outer.example", SourceKind::Seed, InboundScope::Public),
        ] {
            let id = store.source(url, kind, scope).unwrap();
            store
                .replace(
                    id,
                    &[entry(&format!("urn:air:acme.com:{}", url.replace(':', "-")), "x")],
                    &[],
                    1,
                )
                .unwrap();
        }
        assert_eq!(
            identifiers(&store, InboundScope::Internal),
            [
                "urn:air:acme.com:inner.example",
                "urn:air:acme.com:outer.example",
                "urn:air:acme.com:self-internal"
            ]
        );
        assert_eq!(
            identifiers(&store, InboundScope::Public),
            ["urn:air:acme.com:outer.example", "urn:air:acme.com:self-public"]
        );
    }

    #[test]
    fn of_one_identifier_the_self_row_wins_then_the_oldest_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        let first = store
            .source("a.example", SourceKind::Seed, InboundScope::Public)
            .unwrap();
        let second = store
            .source("b.example", SourceKind::Seed, InboundScope::Public)
            .unwrap();
        let own = store
            .source("self:public", SourceKind::SelfCatalog, InboundScope::Public)
            .unwrap();
        let same = |text: &str| entry("urn:air:acme.com:kim", text);
        store.replace(second, &[same("second")], &[], 1).unwrap();
        store.replace(first, &[same("first")], &[], 1).unwrap();
        let shown = |store: &RegistryStore| {
            let rows = store.read().visible(InboundScope::Public).unwrap();
            assert_eq!(rows.len(), 1);
            store.read().bodies(&[rows[0].id]).unwrap()[0]["description"].clone()
        };
        assert_eq!(shown(&store), "first");
        store.replace(own, &[same("own")], &[], 1).unwrap();
        assert_eq!(shown(&store), "own");
    }

    #[test]
    fn term_hits_find_only_rows_the_listener_sees() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        let inner = store
            .source("self:internal", SourceKind::SelfCatalog, InboundScope::Internal)
            .unwrap();
        let outer = store
            .source("self:public", SourceKind::SelfCatalog, InboundScope::Public)
            .unwrap();
        store
            .replace(inner, &[entry("urn:air:acme.com:a", "Payments refund 배포")], &[], 1)
            .unwrap();
        store
            .replace(outer, &[entry("urn:air:acme.com:b", "payments only")], &[], 1)
            .unwrap();
        let terms = ["payments".to_owned(), "refund".to_owned(), "배포".to_owned()];
        let ids = |listener| -> Vec<Vec<String>> {
            let rows = store.read().visible(listener).unwrap();
            store
                .read()
                .term_hits(listener, &terms)
                .unwrap()
                .into_iter()
                .map(|hits| {
                    rows.iter()
                        .filter(|row| hits.contains(&row.id))
                        .map(|row| row.identifier.clone())
                        .collect()
                })
                .collect()
        };
        // The trigram index finds the long terms, LIKE the two-syllable one, ignoring case.
        assert_eq!(
            ids(InboundScope::Internal),
            [
                vec!["urn:air:acme.com:a"],
                vec!["urn:air:acme.com:a"],
                vec!["urn:air:acme.com:a"]
            ]
        );
        assert_eq!(ids(InboundScope::Public), [vec!["urn:air:acme.com:b"], vec![], vec![]]);
    }

    #[test]
    fn the_status_counts_entries_and_refusals_by_reason_and_keeps_entries_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        let own = store
            .source("self:internal", SourceKind::SelfCatalog, InboundScope::Internal)
            .unwrap();
        let seed = store
            .source("acme.com", SourceKind::Seed, InboundScope::Public)
            .unwrap();
        let refusal = |identifier: &str, reason: &str| Refusal {
            identifier: identifier.into(),
            reason: reason.into(),
        };
        store
            .replace(
                seed,
                &[entry("urn:air:acme.com:a", "x"), entry("urn:air:acme.com:b", "y")],
                &[
                    refusal("urn:air:evil.com:x", "other_publisher"),
                    refusal("bad", "invalid"),
                    refusal("urn:air:evil.com:y", "other_publisher"),
                ],
                50,
            )
            .unwrap();
        assert_eq!(store.record_failure(seed, "HTTP 503").unwrap(), (1, Some(50)));
        assert_eq!(store.record_failure(seed, "HTTP 504").unwrap(), (2, Some(50)));
        store.set_next_fetch(seed, 7_250).unwrap();
        assert_eq!(store.seed_entries_outside(own).unwrap(), 2);
        assert_eq!(store.seed_entries_outside(seed).unwrap(), 0);
        let status = store.status().unwrap();
        assert_eq!(
            status,
            [
                SourceStatus {
                    url: "self:internal".into(),
                    kind: SourceKind::SelfCatalog,
                    scope: InboundScope::Internal,
                    entries: 0,
                    last_ok_at: None,
                    next_fetch_at: 0,
                    failures: 0,
                    last_error: None,
                    refusals: vec![],
                },
                SourceStatus {
                    url: "acme.com".into(),
                    kind: SourceKind::Seed,
                    scope: InboundScope::Public,
                    entries: 2,
                    last_ok_at: Some(50),
                    next_fetch_at: 7_250,
                    failures: 2,
                    last_error: Some("HTTP 504".into()),
                    refusals: vec![("invalid".into(), 1), ("other_publisher".into(), 2)],
                },
            ]
        );
        // A success clears the failures.
        store.replace(seed, &[], &[], 60).unwrap();
        let after = &store.status().unwrap()[1];
        assert_eq!((after.entries, after.failures, after.last_error.clone()), (0, 0, None));
        assert_eq!(after.refusals, vec![]);
        // Clearing removes the entries and keeps the source.
        store
            .replace(seed, &[entry("urn:air:acme.com:a", "x")], &[], 70)
            .unwrap();
        store.clear_entries(seed).unwrap();
        let after = &store.status().unwrap()[1];
        assert_eq!((after.entries, after.last_ok_at), (0, Some(70)));
        // The rows of a self catalog do not count against the seeds.
        store
            .replace(own, &[entry("urn:air:acme.com:own", "x")], &[], 80)
            .unwrap();
        let other = store
            .source("beta.com", SourceKind::Seed, InboundScope::Public)
            .unwrap();
        store
            .replace(other, &[entry("urn:air:beta.com:a", "x")], &[], 80)
            .unwrap();
        assert_eq!(store.seed_entries_outside(seed).unwrap(), 1);
        assert_eq!(store.seed_entries_outside(other).unwrap(), 0);
    }

    #[test]
    fn the_seeds_follow_the_operator_list_and_the_first_due_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::open(dir.path()).unwrap();
        let seeds = |list: &[(&str, InboundScope)]| -> Vec<(String, InboundScope)> {
            list.iter().map(|(seed, scope)| ((*seed).to_owned(), *scope)).collect()
        };
        store
            .set_seeds(&seeds(&[
                ("a.example", InboundScope::Internal),
                ("b.example", InboundScope::Public),
            ]))
            .unwrap();
        let own = store
            .source("self:internal", SourceKind::SelfCatalog, InboundScope::Internal)
            .unwrap();
        // New seeds are due at once, in ID order. A self catalog is never a due seed.
        let (a, text) = store.due_seed(0).unwrap().unwrap();
        assert_eq!(text, "a.example");
        store.set_next_fetch(a, 100).unwrap();
        let (b, text) = store.due_seed(0).unwrap().unwrap();
        assert_eq!(text, "b.example");
        store.set_next_fetch(b, 50).unwrap();
        assert_eq!(store.due_seed(10).unwrap(), None);
        assert_eq!(store.due_seed(60).unwrap(), Some((b, "b.example".to_owned())));
        assert_eq!(store.due_seed(200).unwrap(), Some((b, "b.example".to_owned())));
        store.replace(a, &[entry("urn:air:a.example:x", "x")], &[], 1).unwrap();
        store.replace(b, &[entry("urn:air:b.example:x", "x")], &[], 1).unwrap();
        store
            .replace(own, &[entry("urn:air:acme.com:own", "x")], &[], 1)
            .unwrap();

        // A seed that is gone loses its rows. A seed that changed its scope starts again.
        store
            .set_seeds(&seeds(&[
                ("b.example", InboundScope::Internal),
                ("c.example", InboundScope::Public),
            ]))
            .unwrap();
        let urls: Vec<(String, u64, i64)> = store
            .status()
            .unwrap()
            .into_iter()
            .map(|source| (source.url, source.entries, source.next_fetch_at))
            .collect();
        assert_eq!(
            urls,
            [
                ("self:internal".to_owned(), 1, 0),
                ("b.example".to_owned(), 0, 0),
                ("c.example".to_owned(), 0, 0),
            ]
        );
        assert_eq!(identifiers(&store, InboundScope::Internal), ["urn:air:acme.com:own"]);
    }

    #[test]
    fn a_reopened_index_keeps_its_rows_and_its_search() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = RegistryStore::open(dir.path()).unwrap();
            let id = store
                .source("self:public", SourceKind::SelfCatalog, InboundScope::Public)
                .unwrap();
            store
                .replace(id, &[entry("urn:air:acme.com:a", "refund desk")], &[], 1)
                .unwrap();
        }
        let store = RegistryStore::open(dir.path()).unwrap();
        assert_eq!(
            store
                .source("self:public", SourceKind::SelfCatalog, InboundScope::Public)
                .unwrap(),
            1
        );
        let hits = store
            .read()
            .term_hits(InboundScope::Public, &["refund".to_owned()])
            .unwrap();
        assert_eq!(hits[0].len(), 1);
    }
}
