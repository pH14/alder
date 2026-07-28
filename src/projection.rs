use std::{
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    domain::{Event, Head, ProjectState, Question},
    error::{AlderError, Result},
};

#[derive(Debug, Clone)]
pub struct Projection {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservedHandle {
    pub handle: String,
    pub attempt_id: Option<String>,
    pub status: ObservationStatus,
    pub metadata: Value,
    pub observed_at: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Present,
    Absent,
    Unknown,
}

impl ObservationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationRun {
    pub kind: String,
    pub success: bool,
    pub executions: u32,
    pub duration_ms: u64,
    pub stderr: String,
    pub validation_error: Option<String>,
    pub observed_at: String,
    pub object_count: usize,
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
        let work_count: u64 =
            connection.query_row("SELECT count(*) FROM work_current", [], |row| row.get(0))?;
        let attempt_count: u64 =
            connection.query_row("SELECT count(*) FROM attempts", [], |row| row.get(0))?;
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
            "head": head.seq,
            "revision": head.revision,
            "work_rows": work_count,
            "attempt_rows": attempt_count,
        }))
    }

    pub fn observations(&self) -> Result<Vec<ObservedHandle>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT handle, attempt_id, status, metadata, observed_at, detail
             FROM observed_handles ORDER BY handle",
        )?;
        let rows = statement.query_map([], |row| {
            let status: String = row.get(2)?;
            let metadata: String = row.get(3)?;
            Ok(ObservedHandle {
                handle: row.get(0)?,
                attempt_id: row.get(1)?,
                status: parse_status(&status),
                metadata: serde_json::from_str(&metadata).unwrap_or_else(|_| json!({})),
                observed_at: row.get(4)?,
                detail: row.get(5)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn observation_runs(&self) -> Result<Vec<ObservationRun>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT kind, success, executions, duration_ms, stderr, validation_error,
                    observed_at, object_count
             FROM observation_runs ORDER BY kind",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ObservationRun {
                kind: row.get(0)?,
                success: row.get::<_, i64>(1)? != 0,
                executions: row.get::<_, u32>(2)?,
                duration_ms: row.get(3)?,
                stderr: row.get(4)?,
                validation_error: row.get(5)?,
                observed_at: row.get(6)?,
                object_count: row.get(7)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn observation_run(&self, kind: &str) -> Result<Option<ObservationRun>> {
        Ok(self
            .observation_runs()?
            .into_iter()
            .find(|run| run.kind == kind))
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
        )?;
        let mut statement = connection.prepare(sql)?;
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
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            let mut object = serde_json::Map::new();
            for (index, name) in columns.iter().enumerate() {
                object.insert(name.clone(), sql_value(row.get_ref(index)?));
            }
            values.push(Value::Object(object));
        }
        Ok(json!({"columns": columns, "rows": values}))
    }

    pub(crate) fn connection(&self) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(&self.path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        create_schema(&connection)?;
        Ok(connection)
    }
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
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
            outcome TEXT,
            opened_seq INTEGER NOT NULL,
            changed_seq INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS handoffs (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            artifact_ref TEXT NOT NULL,
            note TEXT,
            state TEXT NOT NULL,
            submitted_seq INTEGER NOT NULL,
            work_id TEXT,
            integrated_seq INTEGER
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
            handle TEXT UNIQUE,
            metadata TEXT NOT NULL,
            note TEXT,
            started_seq INTEGER NOT NULL,
            bound_seq INTEGER,
            updated_seq INTEGER NOT NULL,
            ended_seq INTEGER
        );
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
        CREATE TABLE IF NOT EXISTS observed_handles (
            handle TEXT PRIMARY KEY,
            attempt_id TEXT,
            status TEXT NOT NULL,
            metadata TEXT NOT NULL,
            observed_at TEXT NOT NULL,
            detail TEXT
        );
        CREATE TABLE IF NOT EXISTS observation_runs (
            kind TEXT PRIMARY KEY,
            success INTEGER NOT NULL,
            executions INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL,
            stderr TEXT NOT NULL,
            validation_error TEXT,
            observed_at TEXT NOT NULL,
            object_count INTEGER NOT NULL
        );

        DROP VIEW IF EXISTS handoffs_submitted;
        CREATE VIEW handoffs_submitted AS
            SELECT * FROM handoffs WHERE state = 'submitted';
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
            SELECT * FROM questions WHERE answer IS NULL;
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
    )?;
    Ok(())
}

fn represented_head(connection: &Connection) -> Result<Option<Head>> {
    let revision: Option<String> = connection
        .query_row(
            "SELECT value FROM projection_meta WHERE key = 'revision'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let seq: Option<String> = connection
        .query_row(
            "SELECT value FROM projection_meta WHERE key = 'seq'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match (revision, seq) {
        (None, None) => Ok(None),
        (Some(revision), Some(seq)) => Ok(Some(Head {
            revision: (!revision.is_empty()).then_some(revision),
            seq: seq.parse().map_err(|_| {
                AlderError::new("database_error", "projection head sequence is invalid")
            })?,
        })),
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
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "
        DELETE FROM question_answers;
        DELETE FROM questions;
        DELETE FROM attempt_checks;
        DELETE FROM attempts;
        DELETE FROM work_checks;
        DELETE FROM dependencies;
        DELETE FROM handoffs;
        DELETE FROM work_current;
        DELETE FROM events;
        DELETE FROM projection_meta;
        ",
    )?;
    insert_events(&transaction, events)?;
    insert_state(&transaction, state)?;
    transaction.execute(
        "INSERT INTO projection_meta(key, value) VALUES ('revision', ?1)",
        [head.revision.as_deref().unwrap_or("")],
    )?;
    transaction.execute(
        "INSERT INTO projection_meta(key, value) VALUES ('seq', ?1)",
        [head.seq.to_string()],
    )?;
    transaction.commit()?;
    Ok(())
}

fn insert_events(transaction: &Transaction<'_>, events: &[Event]) -> Result<()> {
    for event in events {
        transaction.execute(
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
        )?;
    }
    Ok(())
}

fn insert_state(transaction: &Transaction<'_>, state: &ProjectState) -> Result<()> {
    for work in state.work.values() {
        transaction.execute(
            "INSERT INTO work_current
             (id, title, spec, priority, state, block_reason, outcome, opened_seq, changed_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                work.id,
                work.title,
                work.spec,
                work.priority,
                enum_json(work.state)?,
                work.block_reason,
                work.outcome,
                work.opened_seq,
                work.changed_seq,
            ],
        )?;
        for required in &work.requires {
            transaction.execute(
                "INSERT INTO dependencies(work_id, required_id) VALUES (?1, ?2)",
                params![work.id, required],
            )?;
        }
        for check in &work.checks {
            transaction.execute(
                "INSERT INTO work_checks(work_id, key, description) VALUES (?1, ?2, ?3)",
                params![work.id, check.key, check.description],
            )?;
        }
    }
    for handoff in state.handoffs.values() {
        transaction.execute(
            "INSERT INTO handoffs
             (id, title, artifact_ref, note, state, submitted_seq, work_id, integrated_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                handoff.id,
                handoff.title,
                handoff.artifact_ref,
                handoff.note,
                enum_json(handoff.state)?,
                handoff.submitted_seq,
                handoff.work_id,
                handoff.integrated_seq,
            ],
        )?;
    }
    for attempt in state.attempts.values() {
        transaction.execute(
            "INSERT INTO attempts
             (id, work_id, state, outcome, handle, metadata, note, started_seq, bound_seq,
              updated_seq, ended_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                attempt.id,
                attempt.work_id,
                enum_json(attempt.state)?,
                attempt.outcome.map(enum_json).transpose()?,
                attempt.handle,
                serde_json::to_string(&attempt.metadata)?,
                attempt.note,
                attempt.started_seq,
                attempt.bound_seq,
                attempt.updated_seq,
                attempt.ended_seq,
            ],
        )?;
        for check in attempt.checks.values() {
            transaction.execute(
                "INSERT INTO attempt_checks(attempt_id, key, status, evidence, updated_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    attempt.id,
                    check.key,
                    enum_json(check.status)?,
                    check.evidence,
                    check.updated_seq,
                ],
            )?;
        }
    }
    for question in state.questions.values() {
        insert_question(transaction, question)?;
    }
    Ok(())
}

fn insert_question(transaction: &Transaction<'_>, question: &Question) -> Result<()> {
    transaction.execute(
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
    )?;
    for answer in &question.answers {
        transaction.execute(
            "INSERT INTO question_answers(question_id, seq, answer, actor)
             VALUES (?1, ?2, ?3, ?4)",
            params![question.id, answer.seq, answer.answer, answer.actor],
        )?;
    }
    Ok(())
}

fn enum_json<T: Serialize>(value: T) -> Result<String> {
    let encoded = serde_json::to_string(&value)?;
    Ok(encoded.trim_matches('"').to_owned())
}

fn parse_status(value: &str) -> ObservationStatus {
    match value {
        "present" => ObservationStatus::Present,
        "absent" => ObservationStatus::Absent,
        _ => ObservationStatus::Unknown,
    }
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

pub(crate) fn replace_observation_kind(
    projection: &Projection,
    kind: &str,
    handles: &[ObservedHandle],
    run: &ObservationRun,
) -> Result<()> {
    let mut connection = projection.connection()?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "DELETE FROM observed_handles WHERE handle LIKE ?1 ESCAPE '\\'",
        [format!(
            "{}:%",
            kind.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )],
    )?;
    for handle in handles {
        transaction.execute(
            "INSERT INTO observed_handles
             (handle, attempt_id, status, metadata, observed_at, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                handle.handle,
                handle.attempt_id,
                handle.status.as_str(),
                serde_json::to_string(&handle.metadata)?,
                handle.observed_at,
                handle.detail,
            ],
        )?;
    }
    upsert_run(&transaction, run)?;
    transaction.commit()?;
    Ok(())
}

fn upsert_run(transaction: &Transaction<'_>, run: &ObservationRun) -> Result<()> {
    transaction.execute(
        "INSERT INTO observation_runs
         (kind, success, executions, duration_ms, stderr, validation_error, observed_at,
          object_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(kind) DO UPDATE SET
           success=excluded.success,
           executions=excluded.executions,
           duration_ms=excluded.duration_ms,
           stderr=excluded.stderr,
           validation_error=excluded.validation_error,
           observed_at=excluded.observed_at,
           object_count=excluded.object_count",
        params![
            run.kind,
            run.success as i64,
            run.executions,
            run.duration_ms,
            run.stderr,
            run.validation_error,
            run.observed_at,
            run.object_count,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::{
            CheckDefinition, CheckStatus, CheckUpdate, EventPayload, HandoffDefinition, Ledger,
            WorkState,
        },
        store::MemoryStore,
    };

    #[test]
    fn rebuild_preserves_observations() {
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));
        let handle = ObservedHandle {
            handle: "tmux:one".to_owned(),
            attempt_id: None,
            status: ObservationStatus::Present,
            metadata: json!({}),
            observed_at: Utc::now().to_rfc3339(),
            detail: None,
        };
        let run = ObservationRun {
            kind: "tmux".to_owned(),
            success: true,
            executions: 1,
            duration_ms: 1,
            stderr: String::new(),
            validation_error: None,
            observed_at: Utc::now().to_rfc3339(),
            object_count: 1,
        };
        replace_observation_kind(&projection, "tmux", &[handle], &run).unwrap();
        let event = Event {
            id: "one".to_owned(),
            seq: 1,
            at: Utc::now(),
            actor: "test".to_owned(),
            payload: EventPayload::HandoffSubmitted {
                handoff: HandoffDefinition {
                    id: "hm-handoff-a".to_owned(),
                    title: "handoff".to_owned(),
                    artifact_ref: "ref".to_owned(),
                    note: None,
                },
            },
            schema: "alder.event.v0".to_owned(),
        };
        let state = ProjectState::fold(std::slice::from_ref(&event)).unwrap();
        projection
            .rebuild(
                &Head {
                    revision: Some("one".to_owned()),
                    seq: 1,
                },
                &[event],
                &state,
            )
            .unwrap();
        assert_eq!(projection.observations().unwrap().len(), 1);
    }

    #[test]
    fn projection_round_trips_the_complete_fold_and_observation_state() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = ledger
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
        let (_, attempt_id) = ledger.start(&work_id, BTreeMap::new()).unwrap();
        ledger
            .bind_attempt(&attempt_id, "tmux:one".to_owned(), BTreeMap::new())
            .unwrap();
        ledger
            .update_attempt(
                &attempt_id,
                BTreeMap::from([("engine".to_owned(), json!("opus"))]),
                Some("working".to_owned()),
                vec![CheckUpdate {
                    key: "test".to_owned(),
                    status: CheckStatus::Satisfied,
                    evidence: "CI 42".to_owned(),
                }],
            )
            .unwrap();
        let (_, question_id) = ledger.ask(&work_id, "Ship?".to_owned()).unwrap();
        ledger.answer(&question_id, "Yes".to_owned()).unwrap();
        let snapshot = ledger.snapshot().unwrap();

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

        let handle = ObservedHandle {
            handle: "tmux:one".to_owned(),
            attempt_id: Some(attempt_id),
            status: ObservationStatus::Absent,
            metadata: json!({"state": "gone"}),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            detail: Some("not found".to_owned()),
        };
        let run = ObservationRun {
            kind: "tmux".to_owned(),
            success: false,
            executions: 4,
            duration_ms: 20,
            stderr: "failed".to_owned(),
            validation_error: Some("bad output".to_owned()),
            observed_at: "2026-01-01T00:00:00Z".to_owned(),
            object_count: 1,
        };
        replace_observation_kind(&projection, "tmux", &[handle], &run).unwrap();
        let observed = projection.observations().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].status, ObservationStatus::Absent);
        assert_eq!(observed[0].metadata, json!({"state": "gone"}));
        let runs = projection.observation_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].success);
        assert_eq!(runs[0].executions, 4);
        assert_eq!(
            projection.observation_run("tmux").unwrap().unwrap().stderr,
            "failed"
        );
        assert!(projection.observation_run("other").unwrap().is_none());

        assert!(
            projection
                .raw_query("DELETE FROM work_current")
                .unwrap_err()
                .code
                == "read_only_query"
        );
    }

    #[test]
    fn verification_detects_each_projection_mismatch() {
        let ledger = Ledger::new(MemoryStore::new(), "hm", "tester");
        let (_, work_id) = ledger
            .add_work("Build".to_owned(), None, 0, Vec::new(), Vec::new())
            .unwrap();
        ledger.start(&work_id, BTreeMap::new()).unwrap();
        let snapshot = ledger.snapshot().unwrap();
        let temporary = TempDir::new().unwrap();
        let projection = Projection::new(temporary.path().join("state.db"));

        projection
            .rebuild(&snapshot.head, &snapshot.events, &snapshot.state)
            .unwrap();
        let wrong_head = Head {
            revision: Some("other".to_owned()),
            seq: snapshot.head.seq,
        };
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
        assert_eq!(parse_status("present"), ObservationStatus::Present);
        assert_eq!(parse_status("absent"), ObservationStatus::Absent);
        assert_eq!(parse_status("anything"), ObservationStatus::Unknown);
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");

        let missing = Projection::new(temporary.path().join("missing.db"));
        assert_eq!(
            missing.raw_query("SELECT 1").unwrap_err().code,
            "database_missing"
        );
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
