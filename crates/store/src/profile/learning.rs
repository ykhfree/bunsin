//! The tables of replay evaluation.

use super::ProfileStore;
use crate::{Result, StoreError};
use bunsin_core::eval::{CaseScore, DraftKind, EvalResult, EvalRun, Expect, ReplayCaseRecord, RunState};
use bunsin_core::event::InboundEvent;
use rusqlite::{OptionalExtension, Row, TransactionBehavior};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS replay_cases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event TEXT NOT NULL,
    actual_reply TEXT NOT NULL,
    expect TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS eval_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    state TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE TABLE IF NOT EXISTS eval_results (
    run_id INTEGER NOT NULL,
    case_id INTEGER NOT NULL,
    expect TEXT NOT NULL,
    kind TEXT,
    kind_ok INTEGER NOT NULL,
    overlap_permille INTEGER,
    draft TEXT NOT NULL,
    error TEXT,
    judge TEXT,
    PRIMARY KEY (run_id, case_id)
);
";

fn bad(value: String) -> StoreError {
    StoreError::BadState(value)
}

fn case_from_row(row: &Row<'_>) -> rusqlite::Result<(i64, String, String, String, i64)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
}

fn case(raw: (i64, String, String, String, i64)) -> Result<ReplayCaseRecord> {
    let (id, event, actual_reply, expect, created_at) = raw;
    Ok(ReplayCaseRecord {
        id,
        event: serde_json::from_str(&event)?,
        actual_reply,
        expect: Expect::parse(&expect).ok_or_else(|| bad(expect))?,
        created_at,
    })
}

const CASE_COLUMNS: &str = "id, event, actual_reply, expect, created_at";

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<(i64, String, i64, Option<i64>)> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn run(raw: (i64, String, i64, Option<i64>)) -> Result<EvalRun> {
    let (id, state, started_at, finished_at) = raw;
    let state = match state.as_str() {
        "running" => RunState::Running,
        "done" => RunState::Done,
        _ => return Err(bad(state)),
    };
    Ok(EvalRun {
        id,
        state,
        started_at,
        finished_at,
    })
}

impl ProfileStore {
    /// Stores a replay case and returns its ID. The caller filters the texts first.
    pub fn add_replay_case(&self, event: &InboundEvent, actual_reply: &str, expect: Expect, now: i64) -> Result<i64> {
        let db = self.db();
        db.execute(
            "INSERT INTO replay_cases (event, actual_reply, expect, created_at) VALUES (?1, ?2, ?3, ?4)",
            (serde_json::to_string(event)?, actual_reply, expect.as_str(), now),
        )?;
        Ok(db.last_insert_rowid())
    }

    /// The stored cases, oldest first, at most `limit`.
    pub fn replay_cases(&self, limit: u32) -> Result<Vec<ReplayCaseRecord>> {
        let db = self.db();
        let mut statement = db.prepare(&format!("SELECT {CASE_COLUMNS} FROM replay_cases ORDER BY id LIMIT ?1"))?;
        let rows = statement
            .query_map([limit], case_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(case).collect()
    }

    /// Deletes a case. The results of earlier runs stay.
    pub fn delete_replay_case(&self, id: i64) -> Result<()> {
        match self.db().execute("DELETE FROM replay_cases WHERE id = ?1", [id])? {
            0 => Err(StoreError::NotFound("replay case", id)),
            _ => Ok(()),
        }
    }

    /// Starts a run. Returns `None` if a run is running already. One transaction holds the check
    /// and the insert, so two callers cannot both start one.
    pub fn start_eval_run(&self, now: i64) -> Result<Option<i64>> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let running: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM eval_runs WHERE state = 'running')",
            [],
            |row| row.get(0),
        )?;
        if running {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO eval_runs (state, started_at) VALUES ('running', ?1)",
            [now],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    /// The newest run, if there is one.
    pub fn latest_eval_run(&self) -> Result<Option<EvalRun>> {
        let raw = self
            .db()
            .query_row(
                "SELECT id, state, started_at, finished_at FROM eval_runs ORDER BY id DESC LIMIT 1",
                [],
                run_from_row,
            )
            .optional()?;
        raw.map(run).transpose()
    }

    /// The cases that have no result in run `run_id` yet, oldest first, at most `limit`.
    pub fn unscored_cases(&self, run_id: i64, limit: u32) -> Result<Vec<ReplayCaseRecord>> {
        let db = self.db();
        let mut statement = db.prepare(&format!(
            "SELECT {CASE_COLUMNS} FROM replay_cases c
             WHERE NOT EXISTS (SELECT 1 FROM eval_results r WHERE r.run_id = ?1 AND r.case_id = c.id)
             ORDER BY id LIMIT ?2"
        ))?;
        let rows = statement
            .query_map((run_id, limit), case_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(case).collect()
    }

    /// Stores the result of a case. A second result of the same case in the same run is ignored.
    pub fn add_eval_result(&self, run_id: i64, result: &EvalResult) -> Result<()> {
        let score = result.score.as_ref();
        self.db().execute(
            "INSERT OR IGNORE INTO eval_results
                 (run_id, case_id, expect, kind, kind_ok, overlap_permille, draft, error, judge)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            (
                run_id,
                result.case_id,
                result.expect.as_str(),
                score.map(|score| score.kind.as_str()),
                score.is_some_and(|score| score.kind_ok),
                score.and_then(|score| score.overlap_permille),
                &result.draft,
                &result.error,
                result.judge.as_ref().map(serde_json::to_string).transpose()?,
            ),
        )?;
        Ok(())
    }

    /// The results of a run, in the order of the case IDs.
    pub fn eval_results(&self, run_id: i64) -> Result<Vec<EvalResult>> {
        let db = self.db();
        let mut statement = db.prepare(
            "SELECT case_id, expect, kind, kind_ok, overlap_permille, draft, error, judge
             FROM eval_results WHERE run_id = ?1 ORDER BY case_id",
        )?;
        type ResultRow = (
            i64,
            String,
            Option<String>,
            bool,
            Option<u32>,
            String,
            Option<String>,
            Option<String>,
        );
        let rows = statement
            .query_map([run_id], |row| {
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
            })?
            .collect::<rusqlite::Result<Vec<ResultRow>>>()?;
        rows.into_iter()
            .map(
                |(case_id, expect, kind, kind_ok, overlap_permille, draft, error, judge)| {
                    let score = match kind {
                        Some(kind) => Some(CaseScore {
                            kind: DraftKind::parse(&kind).ok_or_else(|| bad(kind))?,
                            kind_ok,
                            overlap_permille,
                        }),
                        None => None,
                    };
                    Ok(EvalResult {
                        case_id,
                        expect: Expect::parse(&expect).ok_or_else(|| bad(expect))?,
                        score,
                        draft,
                        error,
                        judge: judge.map(|text| serde_json::from_str(&text)).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// Marks a run done. Returns false if it was not running.
    pub fn finish_eval_run(&self, run_id: i64, now: i64) -> Result<bool> {
        let changed = self.db().execute(
            "UPDATE eval_runs SET state = 'done', finished_at = ?1 WHERE id = ?2 AND state = 'running'",
            (now, run_id),
        )?;
        Ok(changed == 1)
    }
}

#[cfg(test)]
mod tests {
    use crate::AppStore;
    use bunsin_core::eval::{CaseScore, DraftKind, EvalResult, Expect, Judgment, RunState, Verdict};
    use bunsin_core::event::InboundEvent;

    fn event(text: &str) -> InboundEvent {
        InboundEvent {
            source: "slack".into(),
            external_id: text.into(),
            thread: None,
            author: "lee".into(),
            text: text.into(),
        }
    }

    #[test]
    fn a_run_finds_the_cases_without_a_result_and_keeps_results_of_deleted_cases() {
        let dir = tempfile::tempdir().unwrap();
        let app = AppStore::open(dir.path()).unwrap();
        app.create_profile("kim", "Kim", 1).unwrap();
        let store = app.open_profile("kim").unwrap();
        let first = store
            .add_replay_case(&event("when?"), "At 3.", Expect::Reply, 5)
            .unwrap();
        let second = store.add_replay_case(&event("pay?"), "", Expect::Escalate, 6).unwrap();
        let cases = store.replay_cases(10).unwrap();
        assert_eq!(cases.iter().map(|c| c.id).collect::<Vec<_>>(), vec![first, second]);
        assert_eq!(
            (cases[1].expect, cases[1].event.text.as_str()),
            (Expect::Escalate, "pay?")
        );

        assert_eq!(store.latest_eval_run().unwrap(), None);
        let run = store.start_eval_run(10).unwrap().unwrap();
        // One run at a time.
        assert_eq!(store.start_eval_run(11).unwrap(), None);
        let result = EvalResult {
            case_id: first,
            expect: Expect::Reply,
            score: Some(CaseScore {
                kind: DraftKind::Reply,
                kind_ok: true,
                overlap_permille: Some(800),
            }),
            draft: "At 3 pm.".into(),
            error: None,
            judge: Some(Judgment {
                verdict: Verdict::Partial,
                reason: "a later time".into(),
            }),
        };
        store.add_eval_result(run, &result).unwrap();
        // A second result of the same case is ignored.
        store
            .add_eval_result(
                run,
                &EvalResult {
                    draft: "other".into(),
                    ..result.clone()
                },
            )
            .unwrap();
        let left = store.unscored_cases(run, 10).unwrap();
        assert_eq!(left.iter().map(|c| c.id).collect::<Vec<_>>(), vec![second]);
        let failed = EvalResult {
            case_id: second,
            expect: Expect::Escalate,
            score: None,
            draft: String::new(),
            error: Some("the LLM cannot be used".into()),
            judge: None,
        };
        store.add_eval_result(run, &failed).unwrap();
        assert!(store.unscored_cases(run, 10).unwrap().is_empty());
        assert!(store.finish_eval_run(run, 20).unwrap());
        assert!(!store.finish_eval_run(run, 21).unwrap());
        let latest = store.latest_eval_run().unwrap().unwrap();
        assert_eq!(
            (latest.id, latest.state, latest.finished_at),
            (run, RunState::Done, Some(20))
        );

        store.delete_replay_case(first).unwrap();
        assert!(store.delete_replay_case(first).is_err());
        assert_eq!(store.eval_results(run).unwrap(), vec![result, failed]);
        // A new run starts after the last one is done, and sees only the remaining case.
        let next = store.start_eval_run(30).unwrap().unwrap();
        assert_eq!(store.unscored_cases(next, 10).unwrap().len(), 1);
    }
}
