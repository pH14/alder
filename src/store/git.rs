use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use serde_json::json;
use tempfile::TempDir;

use crate::{
    domain::{Event, EventDraft, Head},
    error::{AlderError, Result},
};

use super::{AppendResult, Store};

#[derive(Debug, Clone)]
pub struct GitStore {
    root: PathBuf,
    remote: String,
    reference: String,
}

impl GitStore {
    pub fn new(
        root: impl Into<PathBuf>,
        remote: impl Into<String>,
        reference: impl Into<String>,
    ) -> Self {
        Self {
            root: root.into(),
            remote: remote.into(),
            reference: reference.into(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn command<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|error| {
                AlderError::with_context(
                    "store_unavailable",
                    format!("failed to execute Git: {error}"),
                    json!({"remote": self.remote, "ref": self.reference}),
                )
            })
    }

    fn successful<I, S>(&self, args: I, operation: &str) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.command(args)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(self.git_error(operation, &output))
        }
    }

    fn git_error(&self, operation: &str, output: &Output) -> AlderError {
        AlderError::with_context(
            "store_unavailable",
            format!("Git could not {operation}"),
            json!({
                "remote": self.remote,
                "ref": self.reference,
                "status": output.status.code(),
                "stderr": bounded(&output.stderr),
            }),
        )
    }

    fn remote_revision(&self) -> Result<Option<String>> {
        let output = self.command(["ls-remote", "--refs", &self.remote, &self.reference])?;
        if !output.status.success() {
            return Err(self.git_error("read the shared log head", &output));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let revision = stdout
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .map(ToOwned::to_owned);
        Ok(revision)
    }

    fn fetch_revision(&self, revision: &str) -> Result<()> {
        let output = self.command([
            "fetch",
            "--no-tags",
            "--quiet",
            &self.remote,
            &self.reference,
        ])?;
        if !output.status.success() {
            return Err(self.git_error("fetch the shared log", &output));
        }
        self.successful(
            ["cat-file", "-e", &format!("{revision}^{{commit}}")],
            "verify the fetched head",
        )?;
        Ok(())
    }

    fn read_events_at_revision(&self, revision: &str) -> Result<Vec<Event>> {
        let output = self.successful(
            ["ls-tree", "-r", "--name-only", revision, "--", "events"],
            "list events",
        )?;
        let mut paths: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|path| path.starts_with("events/") && path.ends_with(".json"))
            .map(ToOwned::to_owned)
            .collect();
        paths.sort();
        let mut events = Vec::with_capacity(paths.len());
        for path in paths {
            let object = format!("{revision}:{path}");
            let output = self.successful(["show", &object], "read an event")?;
            let event: Event = serde_json::from_slice(&output.stdout).map_err(|error| {
                AlderError::with_context(
                    "invalid_log",
                    format!("event `{path}` is invalid: {error}"),
                    json!({"path": path, "revision": revision}),
                )
            })?;
            events.push(event);
        }
        validate_events(&events)?;
        Ok(events)
    }

    fn create_commit(&self, expected: &Head, event: &Event) -> Result<String> {
        let temporary = TempDir::new()?;
        let index = temporary.path().join("index");
        if let Some(parent) = expected.revision.as_deref() {
            self.git_with_index(&index, ["read-tree", parent], None)?;
        } else {
            self.git_with_index(&index, ["read-tree", "--empty"], None)?;
        }
        let mut bytes = serde_json::to_vec_pretty(event)?;
        bytes.push(b'\n');
        let blob = self.git_with_index(&index, ["hash-object", "-w", "--stdin"], Some(&bytes))?;
        let blob = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
        let path = format!("events/{:020}-{}.json", event.seq, event.id);
        let cache_info = format!("100644,{blob},{path}");
        self.git_with_index(
            &index,
            ["update-index", "--add", "--cacheinfo", &cache_info],
            None,
        )?;
        let tree = self.git_with_index(&index, ["write-tree"], None)?;
        let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();

        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .arg("commit-tree")
            .arg(&tree);
        if let Some(parent) = expected.revision.as_deref() {
            command.arg("-p").arg(parent);
        }
        command
            .env("GIT_AUTHOR_NAME", &event.actor)
            .env("GIT_AUTHOR_EMAIL", "alder@localhost")
            .env("GIT_COMMITTER_NAME", "Alder")
            .env("GIT_COMMITTER_EMAIL", "alder@localhost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let message = format!(
            "alder: {} #{} {}\n",
            event.payload.type_name(),
            event.seq,
            event.id
        );
        child
            .stdin
            .take()
            .expect("commit-tree stdin was piped")
            .write_all(message.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(self.git_error("create an event commit", &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn git_with_index<I, S>(&self, index: &Path, args: I, stdin: Option<&[u8]>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.root)
            .env("GIT_INDEX_FILE", index)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn()?;
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("git stdin was piped")
                .write_all(input)?;
        }
        let output = child.wait_with_output()?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(self.git_error("build an event commit", &output))
        }
    }
}

impl Store for GitStore {
    fn current_head(&self) -> Result<Head> {
        let Some(revision) = self.remote_revision()? else {
            return Ok(Head::empty());
        };
        self.fetch_revision(&revision)?;
        let events = self.read_events_at_revision(&revision)?;
        Ok(Head {
            revision: Some(revision),
            seq: events.len() as u64,
        })
    }

    fn read_events(&self, head: &Head) -> Result<Vec<Event>> {
        match head.revision.as_deref() {
            Some(revision) => {
                let events = self.read_events_at_revision(revision)?;
                if events.len() as u64 != head.seq {
                    return Err(AlderError::with_context(
                        "invalid_head",
                        "the head sequence does not match its Git revision",
                        json!({"head": head, "events": events.len()}),
                    ));
                }
                Ok(events)
            }
            None if head.seq == 0 => Ok(Vec::new()),
            None => Err(AlderError::with_context(
                "invalid_head",
                "an empty Git head must have sequence zero",
                json!({"head": head}),
            )),
        }
    }

    fn append(&self, expected: &Head, draft: &EventDraft) -> Result<AppendResult> {
        let current = self.current_head()?;
        let current_events = self.read_events(&current)?;
        if let Some(existing) = current_events.iter().find(|event| event.id == draft.id) {
            return Ok(AppendResult {
                head: current,
                event: existing.clone(),
            });
        }
        if &current != expected {
            return Err(AlderError::with_context(
                "head_conflict",
                "the shared log advanced before the event was appended",
                json!({"expected_head": expected, "current_head": current}),
            ));
        }
        let event = draft.materialize(expected.seq + 1);
        let commit = self.create_commit(expected, &event)?;
        let refspec = format!("{commit}:{}", self.reference);
        let output = self.command(["push", "--porcelain", &self.remote, &refspec])?;
        if output.status.success() {
            return Ok(AppendResult {
                head: Head {
                    revision: Some(commit),
                    seq: event.seq,
                },
                event,
            });
        }

        let after = self.current_head()?;
        let after_events = self.read_events(&after)?;
        if let Some(existing) = after_events
            .iter()
            .find(|candidate| candidate.id == draft.id)
        {
            return Ok(AppendResult {
                head: after,
                event: existing.clone(),
            });
        }
        if &after != expected {
            return Err(AlderError::with_context(
                "head_conflict",
                "another writer advanced the shared log",
                json!({
                    "expected_head": expected,
                    "current_head": after,
                    "event_id": draft.id,
                }),
            ));
        }
        Err(AlderError::with_context(
            "unknown_append_outcome",
            "the event was not found after an inconclusive push",
            json!({
                "expected_head": expected,
                "current_head": after,
                "event_id": draft.id,
                "stderr": bounded(&output.stderr),
            }),
        ))
    }
}

fn validate_events(events: &[Event]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        let expected = index as u64 + 1;
        if event.seq != expected || !ids.insert(&event.id) {
            return Err(AlderError::with_context(
                "invalid_log",
                "the event log is not a unique contiguous sequence",
                json!({"expected_seq": expected, "event_id": event.id, "actual_seq": event.seq}),
            ));
        }
    }
    Ok(())
}

fn bounded(bytes: &[u8]) -> String {
    let value = String::from_utf8_lossy(bytes);
    let mut result: String = value.chars().take(4096).collect();
    if value.chars().count() > 4096 {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        process::Command,
        sync::{Arc, Barrier},
        thread,
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{EventDraft, EventPayload};

    fn run(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup_two() -> (TempDir, PathBuf, GitStore, GitStore) {
        let temporary = TempDir::new().unwrap();
        let remote = temporary.path().join("remote.git");
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        run(
            temporary.path(),
            &["init", "--bare", remote.to_str().unwrap()],
        );
        run(temporary.path(), &["init", first.to_str().unwrap()]);
        run(
            &first,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run(temporary.path(), &["init", second.to_str().unwrap()]);
        run(
            &second,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        (
            temporary,
            remote,
            GitStore::new(first, "origin", "refs/heads/alder"),
            GitStore::new(second, "origin", "refs/heads/alder"),
        )
    }

    fn setup() -> (TempDir, GitStore) {
        let (temporary, _remote, first, _second) = setup_two();
        (temporary, first)
    }

    fn revision(path: &Path, reference: &str) -> Option<String> {
        let output = Command::new("git")
            .args(["rev-parse", "--verify", reference])
            .current_dir(path)
            .output()
            .unwrap();
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn draft(id: &str) -> EventDraft {
        EventDraft {
            id: id.to_owned(),
            at: Utc::now(),
            actor: "test".to_owned(),
            payload: EventPayload::WorkChanged {
                why: Some("test".to_owned()),
                operations: vec![],
            },
            schema: "alder.event.v0".to_owned(),
        }
    }

    #[test]
    fn each_append_is_one_commit_and_conflicts_are_atomic() {
        let (_temporary, store) = setup();
        let empty = store.current_head().unwrap();
        let first = store.append(&empty, &draft("event-one")).unwrap();
        assert_eq!(first.head.seq, 1);
        let error = store.append(&empty, &draft("event-two")).unwrap_err();
        assert_eq!(error.code, "head_conflict");
        let events = store.read_events(&store.current_head().unwrap()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "event-one");
    }

    #[test]
    fn stale_local_and_remote_tracking_refs_do_not_override_the_remote() {
        let (_temporary, _remote, writer, reader) = setup_two();
        let empty = reader.current_head().unwrap();
        let local_event = draft("local-only").materialize(1);
        let local_commit = reader.create_commit(&empty, &local_event).unwrap();
        run(
            reader.root(),
            &["update-ref", "refs/heads/alder", &local_commit],
        );
        run(
            reader.root(),
            &["update-ref", "refs/remotes/origin/alder", &local_commit],
        );

        let pushed = writer.append(&empty, &draft("remote-event")).unwrap();
        assert_eq!(
            revision(reader.root(), "refs/heads/alder").as_deref(),
            Some(local_commit.as_str())
        );
        assert_eq!(
            revision(reader.root(), "refs/remotes/origin/alder").as_deref(),
            Some(local_commit.as_str())
        );
        let observed = reader.current_head().unwrap();
        let events = reader.read_events(&observed).unwrap();

        assert_eq!(observed, pushed.head);
        assert_ne!(observed.revision.as_deref(), Some(local_commit.as_str()));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "remote-event");
        assert_eq!(
            revision(reader.root(), "refs/heads/alder").as_deref(),
            Some(local_commit.as_str())
        );
        assert_eq!(
            revision(reader.root(), "refs/remotes/origin/alder").as_deref(),
            pushed.head.revision.as_deref()
        );
    }

    #[test]
    fn independent_repositories_observe_events_through_the_remote() {
        let (_temporary, _remote, first, second) = setup_two();
        let empty = first.current_head().unwrap();
        let pushed = first.append(&empty, &draft("event-one")).unwrap();

        let observed = second.current_head().unwrap();
        let events = second.read_events(&observed).unwrap();

        assert_eq!(observed, pushed.head);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "event-one");
    }

    #[test]
    fn an_unpushed_local_commit_is_not_in_the_log() {
        let (_temporary, store) = setup();
        let empty = store.current_head().unwrap();
        let event = draft("local-only").materialize(1);
        let commit = store.create_commit(&empty, &event).unwrap();
        run(store.root(), &["update-ref", "refs/heads/alder", &commit]);
        run(
            store.root(),
            &["update-ref", "refs/remotes/origin/alder", &commit],
        );

        let head = store.current_head().unwrap();

        assert_eq!(head, Head::empty());
        assert!(store.read_events(&head).unwrap().is_empty());
        assert_eq!(store.remote_revision().unwrap(), None);
        assert_eq!(
            revision(store.root(), "refs/heads/alder").as_deref(),
            Some(commit.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_rejected_push_reports_failure_and_leaves_the_remote_unchanged() {
        let (_temporary, remote, store, _second) = setup_two();
        let empty = store.current_head().unwrap();
        let first = store.append(&empty, &draft("event-one")).unwrap();
        let before = revision(&remote, "refs/heads/alder").unwrap();
        assert_eq!(first.head.revision.as_deref(), Some(before.as_str()));

        let hook = remote.join("hooks/pre-receive");
        fs::write(
            &hook,
            "#!/bin/sh\necho 'push rejected by test' >&2\nexit 1\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        let error = store.append(&first.head, &draft("event-two")).unwrap_err();

        assert_eq!(error.code, "unknown_append_outcome");
        assert_eq!(
            revision(&remote, "refs/heads/alder").as_deref(),
            Some(before.as_str())
        );
        let head = store.current_head().unwrap();
        assert_eq!(head, first.head);
        let events = store.read_events(&head).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "event-one");
    }

    #[test]
    fn independent_writers_racing_from_one_remote_head_have_one_winner() {
        let (_temporary, _remote, first, second) = setup_two();
        let first_expected = first.current_head().unwrap();
        let second_expected = second.current_head().unwrap();
        assert_eq!(first_expected, second_expected);
        let verifier = first.clone();

        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = [
            (first, first_expected, "event-one"),
            (second, second_expected, "event-two"),
        ]
        .into_iter()
        .map(|(store, expected, id)| {
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.append(&expected, &draft(id))
            })
        })
        .collect();
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| result
                    .as_ref()
                    .is_err_and(|error| error.code == "head_conflict"))
                .count(),
            1
        );
        let winning_append = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .unwrap();
        assert_eq!(winning_append.head.seq, 1);

        let remote_head = verifier.current_head().unwrap();
        let remote_events = verifier.read_events(&remote_head).unwrap();
        assert_eq!(remote_head, winning_append.head);
        assert_eq!(remote_events.len(), 1);
        assert_eq!(remote_events[0].id, winning_append.event.id);
    }
}
