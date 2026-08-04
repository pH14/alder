use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    domain::{Event, Head, ProjectState, Question},
    error::{AlderError, Result},
};

/// The one seam left by moving `AlderError` beneath this crate: a foreign
/// `From<rusqlite::Error>` impl is no longer possible, so database results are
/// mapped here with exactly the conversion that impl performed.
trait Db<T> {
    fn db(self) -> Result<T>;
}

impl<T> Db<T> for rusqlite::Result<T> {
    fn db(self) -> Result<T> {
        self.map_err(|error| AlderError::new("database_error", error.to_string()))
    }
}

#[derive(Debug, Clone)]
pub struct Projection {
    path: PathBuf,
}

impl Projection {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn sync(&self, head: &Head, events: &[Event], state: &ProjectState) -> Result<bool> {
        let mut connection = self.connection()?;
        let represented = represented_head(&connection)?;
        if represented.as_ref() == Some(head) {
            return Ok(false);
        }
        rebuild(&mut connection, head, events, state)?;
        Ok(true)
    }

    pub fn rebuild(&self, head: &Head, events: &[Event], state: &ProjectState) -> Result<()> {
        let mut connection = self.connection()?;
        rebuild(&mut connection, head, events, state)
    }

    pub fn verify(&self, head: &Head, state: &ProjectState) -> Result<Value> {
        let connection = self.connection()?;
        let represented = represented_head(&connection)?;
        let work_count: u64 = connection
            .query_row("SELECT count(*) FROM work_current", [], |row| row.get(0))
            .db()?;
        let attempt_count: u64 = connection
            .query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))
            .db()?;
        let valid = represented.as_ref() == Some(head)
            && work_count == state.work.len() as u64
            && attempt_count == state.attempts.len() as u64;
        if !valid {
            return Err(AlderError::with_context(
                "projection_mismatch",
                "the SQLite projection does not match the durable fold",
                json!({
                    "represented_head": represented,
                    "durable_head": head,
                    "work_rows": work_count,
                    "durable_work": state.work.len(),
                    "attempt_rows": attempt_count,
                    "durable_attempts": state.attempts.len(),
                }),
            ));
        }
        Ok(json!({
            "valid": true,
            "head": head.sequence(),
            "revision": head.revision(),
            "work_rows": work_count,
            "attempt_rows": attempt_count,
        }))
    }

    pub fn raw_query(&self, sql: &str) -> Result<Value> {
        if !self.path.exists() {
            return Err(AlderError::new(
                "database_missing",
                "the projection database does not exist",
            ));
        }
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY.union(OpenFlags::SQLITE_OPEN_NO_MUTEX),
        )
        .db()?;
        let mut statement = connection.prepare(sql).db()?;
        if !statement.readonly() {
            return Err(AlderError::new(
                "read_only_query",
                "debug query accepts read-only SQL only",
            ));
        }
        let columns: Vec<_> = statement
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let mut rows = statement.query([]).db()?;
        let mut values = Vec::new();
        while let Some(row) = rows.next().db()? {
            let mut object = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                object.insert(name.clone(), sql_value(row.get_ref(index).db()?));
            }
            values.push(Value::Object(object));
        }
        Ok(json!({"columns": columns, "rows": values}))
    }

    pub(crate) fn connection(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(&self.path).db()?;
        // Foreign keys are a connection setting, so they are set outside the
        // transaction that follows.
        connection.pragma_update(None, "foreign_keys", "ON").db()?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .db()?;
        // Opening drops and recreates every view, so two processes opening at
        // once would otherwise interleave those statements and one would find
        // a view the other had already recreated. One writer at a time makes
        // the whole setup a single step; the busy timeout makes the second
        // wait for it rather than fail.
        let setup = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .db()?;
        reset_if_schema_changed(&setup)?;
        create_schema(&setup)?;
        setup.commit().db()?;
        Ok(connection)
    }
}

/// Tables are created with `IF NOT EXISTS`, so a shape change alone would
/// leave an old database half-matching the code. Everything here is derived
/// from the log, so the honest response to a schema change is to drop it all
/// and let the next sync refill it.
const SCHEMA_VERSION: i64 = 6;

fn reset_if_schema_changed(connection: &Connection) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .db()?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    connection
        .execute_batch(
            "
        DROP TABLE IF EXISTS projection_meta;
        DROP VIEW IF EXISTS handoffs_submitted;
        DROP TABLE IF EXISTS events;
        DROP TABLE IF EXISTS work_current;
        -- Schema v3 materialized a retired inbox. Remove it during the
        -- rebuild so a projection never retains obsolete live state.
        DROP TABLE IF EXISTS handoffs;
        DROP TABLE IF EXISTS dependencies;
        DROP TABLE IF EXISTS work_checks;
        DROP TABLE IF EXISTS attempts;
        DROP TABLE IF EXISTS attempt_checks;
        DROP TABLE IF EXISTS questions;
        DROP TABLE IF EXISTS question_answers;
        DROP TABLE IF EXISTS passes;
        DROP TABLE IF EXISTS loop_control;
        -- Schema v5 kept a local observed-handle inventory and run records.
        -- Observations are durable log levels now, so the projection no
        -- longer stores either.
        DROP TABLE IF EXISTS observed_handles;
        DROP TABLE IF EXISTS observation_runs;
        ",
        )
        .db()?;
    connection
        .pragma_update(None, "user_version", SCHEMA_VERSION)
        .db()?;
    Ok(())
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            "
        CREATE TABLE IF NOT EXISTS projection_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            seq INTEGER PRIMARY KEY,
            id TEXT NOT NULL UNIQUE,
            at TEXT NOT NULL,
            actor TEXT NOT NULL,
            type TEXT NOT NULL,
            body_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS work_current (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            spec TEXT,
            priority INTEGER NOT NULL,
            state TEXT NOT NULL,
            block_reason TEXT,
            block_until TEXT,
            outcome TEXT,
            opened_seq INTEGER NOT NULL,
            changed_seq INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS dependencies (
            work_id TEXT NOT NULL,
            required_id TEXT NOT NULL,
            PRIMARY KEY (work_id, required_id)
        );
        CREATE TABLE IF NOT EXISTS work_checks (
            work_id TEXT NOT NULL,
            key TEXT NOT NULL,
            description TEXT NOT NULL,
            PRIMARY KEY (work_id, key)
        );
        CREATE TABLE IF NOT EXISTS attempts (
            id TEXT PRIMARY KEY,
            work_id TEXT NOT NULL,
            state TEXT NOT NULL,
            outcome TEXT,
            tier TEXT,
            handle TEXT,
            metadata TEXT NOT NULL,
            note TEXT,
            started_seq INTEGER NOT NULL,
            bound_seq INTEGER,
            updated_seq INTEGER NOT NULL,
            ended_seq INTEGER
        );
        -- A handle is exclusive to one LIVE attempt; respawns reuse the
        -- session name of an ended attempt.
        CREATE UNIQUE INDEX IF NOT EXISTS attempts_live_handle
            ON attempts(handle) WHERE handle IS NOT NULL AND ended_seq IS NULL;
        CREATE TABLE IF NOT EXISTS attempt_checks (
            attempt_id TEXT NOT NULL,
            key TEXT NOT NULL,
            status TEXT NOT NULL,
            evidence TEXT,
            updated_seq INTEGER,
            PRIMARY KEY (attempt_id, key)
        );
        CREATE TABLE IF NOT EXISTS questions (
            id TEXT PRIMARY KEY,
            work_id TEXT NOT NULL,
            text TEXT NOT NULL,
            answer TEXT,
            asked_seq INTEGER NOT NULL,
            answered_seq INTEGER,
            answered_by TEXT
        );
        CREATE TABLE IF NOT EXISTS question_answers (
            question_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            answer TEXT NOT NULL,
            actor TEXT NOT NULL,
            PRIMARY KEY (question_id, seq)
        );
        CREATE TABLE IF NOT EXISTS loop_control (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            paused INTEGER NOT NULL,
            pause_reason TEXT,
            engine TEXT,
            rotate_requested_seq INTEGER,
            nudge_requested_seq INTEGER
        );

        DROP VIEW IF EXISTS ready;
        CREATE VIEW ready AS
            SELECT w.*
            FROM work_current w
            WHERE w.state = 'open'
              AND NOT EXISTS (
                  SELECT 1 FROM attempts a
                  WHERE a.work_id = w.id AND a.state IN ('starting', 'active')
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM dependencies d
                  JOIN work_current required ON required.id = d.required_id
                  WHERE d.work_id = w.id AND required.state != 'done'
              );
        DROP VIEW IF EXISTS in_flight;
        CREATE VIEW in_flight AS
            SELECT a.*, w.title
            FROM attempts a JOIN work_current w ON w.id = a.work_id
            WHERE a.state IN ('starting', 'active');
        DROP VIEW IF EXISTS blocked;
        CREATE VIEW blocked AS
            SELECT * FROM work_current WHERE state = 'blocked';
        DROP VIEW IF EXISTS questions_open;
        CREATE VIEW questions_open AS
            SELECT q.*
            FROM questions q JOIN work_current w ON w.id = q.work_id
            WHERE q.answer IS NULL AND w.state NOT IN ('done', 'dropped');
        DROP VIEW IF EXISTS downstream;
        CREATE VIEW downstream AS
            WITH RECURSIVE graph(root_id, work_id) AS (
                SELECT required_id, work_id FROM dependencies
                UNION
                SELECT graph.root_id, dependencies.work_id
                FROM graph JOIN dependencies ON dependencies.required_id = graph.work_id
            )
            SELECT DISTINCT root_id, work_id FROM graph;
        ",
        )
        .db()?;
    Ok(())
}

fn represented_head(connection: &Connection) -> Result<Option<Head>> {
    let revision: Option<String> = connection
        .query_row(
            "SELECT value FROM projection_meta WHERE key = 'revision'",
            [],
            |row| row.get(0),
        )
        .optional()
        .db()?;
    let seq: Option<String> = connection
        .query_row(
            "SELECT value FROM projection_meta WHERE key = 'seq'",
            [],
            |row| row.get(0),
        )
        .optional()
        .db()?;
    match (revision, seq) {
        (None, None) => Ok(None),
        (Some(revision), Some(seq)) => Ok(Some(Head::try_from_parts(
            seq.parse().map_err(|_| {
                AlderError::new("database_error", "projection head sequence is invalid")
            })?,
            (!revision.is_empty()).then_some(revision),
        )?)),
        _ => Err(AlderError::new(
            "database_error",
            "projection head metadata is incomplete",
        )),
    }
}

fn rebuild(
    connection: &mut Connection,
    head: &Head,
    events: &[Event],
    state: &ProjectState,
) -> Result<()> {
    let transaction = connection.transaction().db()?;
    transaction
        .execute_batch(
            "
        DELETE FROM loop_control;
        DELETE FROM question_answers;
        DELETE FROM questions;
        DELETE FROM attempt_checks;
        DELETE FROM attempts;
        DELETE FROM work_checks;
        DELETE FROM dependencies;
        DELETE FROM work_current;
        DELETE FROM events;
        DELETE FROM projection_meta;
        ",
        )
        .db()?;
    insert_events(&transaction, events)?;
    insert_state(&transaction, state)?;
    transaction
        .execute(
            "INSERT INTO projection_meta(key, value) VALUES ('revision', ?1)",
            [head.revision().unwrap_or("")],
        )
        .db()?;
    transaction
        .execute(
            "INSERT INTO projection_meta(key, value) VALUES ('seq', ?1)",
            [head.sequence().to_string()],
        )
        .db()?;
    transaction.commit().db()?;
    Ok(())
}

fn insert_events(transaction: &Transaction<'_>, events: &[Event]) -> Result<()> {
    for event in events {
        transaction
            .execute(
                "INSERT INTO events(seq, id, at, actor, type, body_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    event.seq,
                    event.id,
                    event.at.to_rfc3339(),
                    event.actor,
                    event.payload.type_name(),
                    serde_json::to_string(&event.payload)?,
                ],
            )
            .db()?;
    }
    Ok(())
}

fn insert_state(transaction: &Transaction<'_>, state: &ProjectState) -> Result<()> {
    for work in state.work.values() {
        transaction
            .execute(
                "INSERT INTO work_current
             (id, title, spec, priority, state, block_reason, block_until, outcome,
              opened_seq, changed_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    work.id,
                    work.title,
                    work.spec,
                    work.priority,
                    enum_json(work.state)?,
                    work.block_reason,
                    work.block_until.map(|until| until.to_rfc3339()),
                    work.outcome,
                    work.opened_seq,
                    work.changed_seq,
                ],
            )
            .db()?;
        for required in &work.requires {
            transaction
                .execute(
                    "INSERT INTO dependencies(work_id, required_id) VALUES (?1, ?2)",
                    params![work.id, required],
                )
                .db()?;
        }
        for check in &work.checks {
            transaction
                .execute(
                    "INSERT INTO work_checks(work_id, key, description) VALUES (?1, ?2, ?3)",
                    params![work.id, check.key, check.description],
                )
                .db()?;
        }
    }
    for attempt in state.attempts.values() {
        transaction
            .execute(
                "INSERT INTO attempts
             (id, work_id, state, outcome, tier, handle, metadata, note, started_seq, bound_seq,
              updated_seq, ended_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    attempt.id,
                    attempt.work_id,
                    enum_json(attempt.state)?,
                    attempt.outcome.map(enum_json).transpose()?,
                    attempt.tier,
                    attempt.handle,
                    serde_json::to_string(&attempt.metadata)?,
                    attempt.note,
                    attempt.started_seq,
                    attempt.bound_seq,
                    attempt.updated_seq,
                    attempt.ended_seq,
                ],
            )
            .db()?;
        for check in attempt.checks.values() {
            transaction
                .execute(
                    "INSERT INTO attempt_checks(attempt_id, key, status, evidence, updated_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        attempt.id,
                        check.key,
                        enum_json(check.status)?,
                        check.evidence,
                        check.updated_seq,
                    ],
                )
                .db()?;
        }
    }
    for question in state.questions.values() {
        insert_question(transaction, question)?;
    }
    let control = &state.loop_control;
    transaction
        .execute(
            "INSERT INTO loop_control
         (id, paused, pause_reason, engine, rotate_requested_seq, nudge_requested_seq)
         VALUES (0, ?1, ?2, ?3, ?4, ?5)",
            params![
                control.paused as i64,
                control.pause_reason,
                control.engine,
                control.rotate_requested_seq,
                control.nudge_requested_seq,
            ],
        )
        .db()?;
    Ok(())
}

fn insert_question(transaction: &Transaction<'_>, question: &Question) -> Result<()> {
    transaction
        .execute(
            "INSERT INTO questions
         (id, work_id, text, answer, asked_seq, answered_seq, answered_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                question.id,
                question.work_id,
                question.text,
                question.answer,
                question.asked_seq,
                question.answered_seq,
                question.answered_by,
            ],
        )
        .db()?;
    for answer in &question.answers {
        transaction
            .execute(
                "INSERT INTO question_answers(question_id, seq, answer, actor)
             VALUES (?1, ?2, ?3, ?4)",
                params![question.id, answer.seq, answer.answer, answer.actor],
            )
            .db()?;
    }
    Ok(())
}

fn enum_json<T: Serialize>(value: T) -> Result<String> {
    let encoded = serde_json::to_string(&value)?;
    Ok(encoded.trim_matches('"').to_owned())
}

fn sql_value(value: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => Value::String(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => Value::String(hex(value)),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::domain::{
        AttemptOutcome, CheckDefinition, CheckStatus, CheckUpdate, ProjectLog, WorkState,
    };
    use alder_log::MemoryLog as MemoryStore;

    #[test]
    fn overlapping_opens_each_leave_the_schema_whole() {
        // Opening drops and recreates every view, so two opens that overlap
        // must not interleave: one would find a view the other had already
        // recreated and fail. A leader and its workers all read at once, so
        // this is the ordinary case, not a corner.
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        // One count: a barrier expecting more openers than there are would
        // hang rather than fail.
        let openers = 16;
        let barrier = Arc::new(Barrier::new(openers));
        let opens: Vec<_> = (0..openers)
            .map(|_| {
                let projection = projection.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    // Every opener runs each round together, and none leaves
                    // early on failure, so the rounds stay aligned and the
                    // last one is as contended as the first.
                    let mut failure = None;
                    for _ in 0..10 {
                        barrier.wait();
                        if let Err(error) = projection.connection() {
                            failure.get_or_insert(error);
                        }
                    }
                    failure
                })
            })
            .collect();
        for open in opens {
            if let Some(error) = open.join().unwrap() {
                panic!("an overlapping open failed: {error:?}");
            }
        }
        // The views the last open left behind still answer.
        let connection = projection.connection().unwrap();
        connection
            .query_row("SELECT count(*) FROM ready", [], |row| row.get::<_, u64>(0))
            .unwrap();
    }

    #[test]
    fn a_respawn_may_reuse_the_handle_of_an_ended_attempt() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = log
            .add_work("Build".to_owned(), None, 42, Vec::new(), Vec::new())
            .unwrap();
        let (_, first) = log.start(&work_id, None, BTreeMap::new()).unwrap();
        log.bind_attempt(&first, "tmux:leader".to_owned(), BTreeMap::new())
            .unwrap();
        log.end_attempt(&first, AttemptOutcome::Lost, "handle absent".to_owned())
            .unwrap();
        let (_, second) = log.start(&work_id, None, BTreeMap::new()).unwrap();
        log.bind_attempt(&second, "tmux:leader".to_owned(), BTreeMap::new())
            .unwrap();
        let snapshot = log.snapshot().unwrap();

        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        projection
            .sync(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();

        let rows = projection
            .raw_query("SELECT count(*) AS total FROM attempts WHERE handle = 'tmux:leader'")
            .unwrap();
        assert_eq!(rows["rows"][0]["total"], 2);
    }

    #[test]
    fn projection_round_trips_the_complete_fold() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = log
            .add_work(
                "Build".to_owned(),
                Some("Specification".to_owned()),
                42,
                Vec::new(),
                vec![CheckDefinition {
                    key: "test".to_owned(),
                    description: "tests pass".to_owned(),
                }],
            )
            .unwrap();
        let (_, attempt_id) = log.start(&work_id, None, BTreeMap::new()).unwrap();
        log.bind_attempt(&attempt_id, "tmux:one".to_owned(), BTreeMap::new())
            .unwrap();
        log.update_attempt(
            &attempt_id,
            Some("opus".to_owned()),
            BTreeMap::new(),
            Some("working".to_owned()),
            vec![CheckUpdate {
                key: "test".to_owned(),
                status: CheckStatus::Satisfied,
                evidence: "CI 42".to_owned(),
            }],
        )
        .unwrap();
        let (_, question_id) = log.ask(&work_id, "Ship?".to_owned()).unwrap();
        log.answer(&question_id, "Yes".to_owned()).unwrap();
        let snapshot = log.snapshot().unwrap();

        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        projection
            .rebuild(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();

        let verified = projection.verify(&snapshot.head, &snapshot.state).unwrap();
        assert_eq!(verified["valid"], true);
        assert_eq!(verified["head"], 6);
        assert_eq!(verified["work_rows"], 1);
        assert_eq!(verified["attempt_rows"], 1);

        let event_rows = projection
            .raw_query("SELECT seq, type FROM events ORDER BY seq")
            .unwrap();
        assert_eq!(event_rows["rows"].as_array().unwrap().len(), 6);
        assert_eq!(event_rows["rows"][0]["type"], "work.changed");
        assert_eq!(event_rows["rows"][5]["type"], "question.answered");
        let question_rows = projection
            .raw_query(
                "SELECT q.answer, count(a.seq) AS answers
                 FROM questions q JOIN question_answers a ON a.question_id = q.id
                 GROUP BY q.id",
            )
            .unwrap();
        assert_eq!(question_rows["rows"][0]["answer"], "Yes");
        assert_eq!(question_rows["rows"][0]["answers"], 1);

        // The attempt row carries the tier verbatim, as an opaque name.
        let attempt_rows = projection
            .raw_query("SELECT id, tier, handle FROM attempts")
            .unwrap();
        assert_eq!(attempt_rows["rows"][0]["id"], attempt_id);
        assert_eq!(attempt_rows["rows"][0]["tier"], "opus");
        assert_eq!(attempt_rows["rows"][0]["handle"], "tmux:one");

        assert!(
            projection
                .raw_query("DELETE FROM work_current")
                .unwrap_err()
                .code
                == "read_only_query"
        );
    }

    /// Observations are part of no live-state table on purpose: status is
    /// never served from SQLite. Every command refolds the decoded log —
    /// `ProjectLog::snapshot` — and the projection is a derived query
    /// surface, so there is no cached `ProjectState` round-trip that could
    /// drop `observations` and cost a cache-served status its dead-worker
    /// attention. What the projection does serialize is the event stream
    /// itself; this pins the round-trip that could actually lose the
    /// picture: the projected events refold to the same observations,
    /// including a level held for an ended attempt (the orphan watch).
    #[test]
    fn projected_events_refold_to_the_same_observation_picture() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = log
            .add_work("Build".to_owned(), None, 0, Vec::new(), Vec::new())
            .unwrap();
        let (_, attempt_id) = log.start(&work_id, None, BTreeMap::new()).unwrap();
        log.bind_attempt(&attempt_id, "tmux:one".to_owned(), BTreeMap::new())
            .unwrap();
        log.report_observation(
            crate::domain::ObservationKey {
                observer: "tmux".to_owned(),
                subject: attempt_id.clone(),
                field: "liveness".to_owned(),
            },
            "present".to_owned(),
        )
        .unwrap();
        log.end_attempt(
            &attempt_id,
            AttemptOutcome::Cancelled,
            "fixture over".to_owned(),
        )
        .unwrap();
        let snapshot = log.snapshot().unwrap();
        assert_eq!(snapshot.state.observations.len(), 1);

        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        projection
            .sync(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();
        assert_eq!(
            projection.verify(&snapshot.head, &snapshot.state).unwrap()["valid"],
            true
        );

        let rows = projection
            .raw_query("SELECT seq, id, at, actor, body_json FROM events ORDER BY seq")
            .unwrap();
        let reloaded: Vec<crate::domain::Event> = rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| crate::domain::Event {
                id: row["id"].as_str().unwrap().to_owned(),
                seq: row["seq"].as_u64().unwrap(),
                at: row["at"].as_str().unwrap().parse().unwrap(),
                actor: row["actor"].as_str().unwrap().to_owned(),
                payload: serde_json::from_str(row["body_json"].as_str().unwrap()).unwrap(),
                schema: "alder.event.v0".to_owned(),
            })
            .collect();
        let refolded = ProjectState::fold(&reloaded).unwrap();
        assert_eq!(refolded.observations, snapshot.state.observations);
        let key = refolded.observations.keys().next().unwrap();
        assert_eq!(key.subject, attempt_id);
        assert_eq!(key.field, "liveness");
    }

    #[test]
    fn verification_detects_each_projection_mismatch() {
        let log = ProjectLog::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = log
            .add_work("Build".to_owned(), None, 0, Vec::new(), Vec::new())
            .unwrap();
        log.start(&work_id, None, BTreeMap::new()).unwrap();
        let snapshot = log.snapshot().unwrap();
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));

        projection
            .rebuild(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();
        let wrong_head =
            Head::try_from_parts(snapshot.head.sequence(), Some("other".to_owned())).unwrap();
        assert_eq!(
            projection
                .verify(&wrong_head, &snapshot.state)
                .unwrap_err()
                .code,
            "projection_mismatch"
        );

        projection
            .connection()
            .unwrap()
            .execute("DELETE FROM work_current", [])
            .unwrap();
        assert_eq!(
            projection
                .verify(&snapshot.head, &snapshot.state)
                .unwrap_err()
                .code,
            "projection_mismatch"
        );

        projection
            .rebuild(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();
        projection
            .connection()
            .unwrap()
            .execute("DELETE FROM attempts", [])
            .unwrap();
        assert_eq!(
            projection
                .verify(&snapshot.head, &snapshot.state)
                .unwrap_err()
                .code,
            "projection_mismatch"
        );
    }

    #[test]
    fn projection_helpers_cover_empty_corrupt_and_scalar_values() {
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        let connection = projection.connection().unwrap();
        assert_eq!(represented_head(&connection).unwrap(), None);
        connection
            .execute(
                "INSERT INTO projection_meta(key, value) VALUES ('revision', 'one')",
                [],
            )
            .unwrap();
        assert_eq!(
            represented_head(&connection).unwrap_err().code,
            "database_error"
        );

        assert_eq!(enum_json(WorkState::Open).unwrap(), "open");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");

        let missing = Projection::new(temporary.path().join("missing.db"));
        assert_eq!(
            missing.raw_query("SELECT 1").unwrap_err().code,
            "database_missing"
        );
    }

    #[test]
    fn opening_a_stale_schema_drops_derived_rows_before_recreating_it() {
        let temporary = TempDir::new().unwrap();
        let path = temporary.path().join("state.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "
                PRAGMA user_version = 2;
                CREATE TABLE events (
                    seq INTEGER PRIMARY KEY,
                    id TEXT NOT NULL UNIQUE,
                    at TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    type TEXT NOT NULL,
                    body_json TEXT NOT NULL
                );
                INSERT INTO events(seq, id, at, actor, type, body_json)
                VALUES (1, 'old', 'now', 'test', 'old.event', '{}');
                ",
            )
            .unwrap();
        drop(connection);

        let projection = Projection::new(&path);
        let connection = projection.connection().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let rows: u64 = connection
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(rows, 0);
    }

    #[test]
    fn sync_rebuilds_only_when_the_represented_head_changes() {
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        let head = Head::empty();
        let state = ProjectState::default();

        assert!(projection.sync(&head, &[], &state).unwrap());
        assert!(!projection.sync(&head, &[], &state).unwrap());
    }
}
