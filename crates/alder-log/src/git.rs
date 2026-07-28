use crate::{AppendDisposition, AppendReceipt, Head, Log, LogError, Record, RecordDraft};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

/// A Git-backed log whose configured remote reference is authoritative.
#[derive(Debug, Clone)]
pub struct GitLog {
    root: PathBuf,
    remote: String,
    reference: String,
}
impl GitLog {
    /// Create a Git log using explicit repository, remote, and authoritative ref.
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
    fn command<I, S>(&self, args: I) -> Result<Output, LogError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .map_err(|error| LogError::Unavailable {
                message: format!("failed to execute Git: {error}"),
            })
    }
    fn successful<I, S>(&self, args: I, operation: &str) -> Result<Output, LogError>
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
    fn git_error(&self, operation: &str, output: &Output) -> LogError {
        LogError::Unavailable {
            message: format!("Git could not {operation}: {}", bounded(&output.stderr)),
        }
    }
    fn remote_revision(&self) -> Result<Option<String>, LogError> {
        let output = self.command(["ls-remote", "--refs", &self.remote, &self.reference])?;
        if !output.status.success() {
            return Err(self.git_error("read the shared log head", &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .map(ToOwned::to_owned))
    }
    fn fetch_revision(&self, revision: &str) -> Result<(), LogError> {
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
    fn verify_authoritative_revision(&self, revision: &str) -> Result<(), LogError> {
        if !is_object_id(revision) {
            return Err(LogError::InvalidHead {
                message: "a Git head revision must be a full object ID".to_owned(),
            });
        }
        let Some(authoritative) = self.remote_revision()? else {
            return Err(LogError::InvalidHead {
                message: "the authoritative remote log is empty".to_owned(),
            });
        };
        self.fetch_revision(&authoritative)?;
        let object = self.command(["cat-file", "-e", &format!("{revision}^{{commit}}")])?;
        if !object.status.success() {
            return Err(LogError::InvalidHead {
                message: "the requested Git revision is unavailable".to_owned(),
            });
        }
        let output = self.command(["merge-base", "--is-ancestor", revision, &authoritative])?;
        if output.status.success() {
            Ok(())
        } else if output.status.code() == Some(1) {
            Err(LogError::InvalidHead {
                message: "the requested revision is not in the authoritative remote history"
                    .to_owned(),
            })
        } else {
            Err(self.git_error("verify a pinned log head", &output))
        }
    }
    fn read_records_at_revision(&self, revision: &str) -> Result<Vec<Record>, LogError> {
        let output = self.successful(
            ["ls-tree", "-r", "--name-only", revision, "--", "events"],
            "list records",
        )?;
        let mut paths: Vec<_> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|path| is_event_path(path))
            .map(ToOwned::to_owned)
            .collect();
        paths.sort();
        let mut records = Vec::with_capacity(paths.len());
        for path in paths {
            let output =
                self.successful(["show", &format!("{revision}:{path}")], "read a record")?;
            records.push(serde_json::from_slice(&output.stdout).map_err(|error| {
                LogError::InvalidLog {
                    message: format!("record `{path}` is invalid: {error}"),
                }
            })?);
        }
        validate_records(&records)?;
        Ok(records)
    }
    fn create_commit(&self, expected: &Head, record: &Record) -> Result<String, LogError> {
        let temporary = TemporaryDirectory::new()?;
        let index = temporary.path().join("index");
        if let Some(parent) = expected.revision() {
            self.git_with_index(&index, ["read-tree", parent], None)?;
        } else {
            self.git_with_index(&index, ["read-tree", "--empty"], None)?;
        }
        let mut bytes = serde_json::to_vec_pretty(record)?;
        bytes.push(b'\n');
        let blob = self.git_with_index(&index, ["hash-object", "-w", "--stdin"], Some(&bytes))?;
        let blob = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
        let path = format!("events/{:020}-{}.json", record.sequence(), record.id());
        self.git_with_index(
            &index,
            [
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            None,
        )?;
        let tree = self.git_with_index(&index, ["write-tree"], None)?;
        let tree = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
        let mut command = Command::new("git");
        command
            .current_dir(&self.root)
            .arg("commit-tree")
            .arg(&tree);
        if let Some(parent) = expected.revision() {
            command.arg("-p").arg(parent);
        }
        command
            .env("GIT_AUTHOR_NAME", record.actor())
            .env("GIT_AUTHOR_EMAIL", "alder-log@localhost")
            .env("GIT_COMMITTER_NAME", "Alder Log")
            .env("GIT_COMMITTER_EMAIL", "alder-log@localhost")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        child
            .stdin
            .take()
            .expect("commit-tree stdin was piped")
            .write_all(
                format!(
                    "alder: {} #{} {}\n",
                    record.kind(),
                    record.sequence(),
                    record.id()
                )
                .as_bytes(),
            )?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(self.git_error("create a record commit", &output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }
    fn git_with_index<I, S>(
        &self,
        index: &Path,
        args: I,
        stdin: Option<&[u8]>,
    ) -> Result<Output, LogError>
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
            Err(self.git_error("build a record commit", &output))
        }
    }
    fn resolve_after_push(
        &self,
        expected: &Head,
        draft: &RecordDraft,
        diagnostic: &str,
    ) -> Result<AppendReceipt, LogError> {
        let after = self.head().map_err(|error| LogError::UnknownOutcome {
            message: format!("push response was inconclusive and resolution failed: {error}"),
        })?;
        let records = self
            .read_all(&after)
            .map_err(|error| LogError::UnknownOutcome {
                message: format!(
                    "push response was inconclusive and resolution read failed: {error}"
                ),
            })?;
        if let Some(existing) = records.iter().find(|record| record.id() == draft.id()) {
            if !existing.matches_draft(draft) {
                return Err(LogError::RecordIdCollision {
                    id: draft.id().clone(),
                });
            }
            return Ok(AppendReceipt {
                disposition: AppendDisposition::AlreadyPresent,
                record: existing.clone(),
                observed_head: after,
            });
        }
        if &after != expected {
            return Err(LogError::HeadConflict {
                expected: expected.clone(),
                observed: after,
            });
        }
        Err(LogError::UnknownOutcome {
            message: format!(
                "the record was absent after an inconclusive push: {}",
                diagnostic
            ),
        })
    }
}
impl Log for GitLog {
    fn head(&self) -> Result<Head, LogError> {
        let Some(revision) = self.remote_revision()? else {
            return Ok(Head::empty());
        };
        self.fetch_revision(&revision)?;
        Head::try_from_parts(
            self.read_records_at_revision(&revision)?.len() as u64,
            Some(revision),
        )
    }
    fn read(&self, through: &Head, after: u64) -> Result<Vec<Record>, LogError> {
        if after > through.sequence() {
            return Err(LogError::InvalidRange {
                after,
                through: through.sequence(),
            });
        }
        let records = match through.revision() {
            Some(revision) => {
                self.verify_authoritative_revision(revision)?;
                self.read_records_at_revision(revision)?
            }
            None if through.is_empty() => Vec::new(),
            None => {
                return Err(LogError::InvalidHead {
                    message: "a non-empty Git head requires a revision".to_owned(),
                });
            }
        };
        if records.len() as u64 != through.sequence() {
            return Err(LogError::InvalidHead {
                message: "the head sequence does not match its Git revision".to_owned(),
            });
        }
        Ok(records[after as usize..].to_vec())
    }
    fn append(&self, expected: &Head, draft: &RecordDraft) -> Result<AppendReceipt, LogError> {
        let current = self.head()?;
        let records = self.read_all(&current)?;
        if let Some(existing) = records.iter().find(|record| record.id() == draft.id()) {
            if !existing.matches_draft(draft) {
                return Err(LogError::RecordIdCollision {
                    id: draft.id().clone(),
                });
            }
            return Ok(AppendReceipt {
                disposition: AppendDisposition::AlreadyPresent,
                record: existing.clone(),
                observed_head: current,
            });
        }
        if expected != &current {
            return Err(LogError::HeadConflict {
                expected: expected.clone(),
                observed: current,
            });
        }
        let record = Record::materialize(draft, current.sequence() + 1);
        let commit = self.create_commit(expected, &record)?;
        let refspec = format!("{commit}:{}", self.reference);
        let mut command = Command::new("git");
        command
            .args(["push", "--porcelain", &self.remote, &refspec])
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().map_err(|error| LogError::Unavailable {
            message: format!("failed to start Git push: {error}"),
        })?;
        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                return self.resolve_after_push(
                    expected,
                    draft,
                    &format!("failed to collect the Git push response: {error}"),
                );
            }
        };
        if output.status.success() {
            return Ok(AppendReceipt {
                disposition: AppendDisposition::Appended,
                record: record.clone(),
                observed_head: Head::try_from_parts(record.sequence(), Some(commit))?,
            });
        }
        self.resolve_after_push(expected, draft, &bounded(&output.stderr))
    }
}
fn is_event_path(path: &str) -> bool {
    path.starts_with("events/") && path.ends_with(".json")
}
fn is_object_id(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64) && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn validate_records(records: &[Record]) -> Result<(), LogError> {
    let mut ids = BTreeSet::new();
    for (index, record) in records.iter().enumerate() {
        let expected = index as u64 + 1;
        if record.sequence() != expected || !ids.insert(record.id()) {
            return Err(LogError::InvalidLog {
                message: format!(
                    "records are not a unique contiguous sequence at expected sequence {expected}"
                ),
            });
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

struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new() -> Result<Self, LogError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..32 {
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("alder-log-{}-{unique}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(LogError::Io {
            message: "could not create a unique temporary Git index directory".to_owned(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(id: &str, sequence: u64) -> Record {
        serde_json::from_value(json!({
            "id": id,
            "seq": sequence,
            "at": "2026-07-27T12:00:00Z",
            "actor": "test",
            "type": "example.changed",
            "body": {},
            "schema": "example.v1"
        }))
        .unwrap()
    }

    #[test]
    fn validation_rejects_duplicate_and_gapped_histories() {
        validate_records(&[record("one", 1), record("two", 2)]).unwrap();
        assert!(matches!(
            validate_records(&[record("one", 1), record("one", 2)]),
            Err(LogError::InvalidLog { .. })
        ));
        assert!(matches!(
            validate_records(&[record("one", 1), record("two", 3)]),
            Err(LogError::InvalidLog { .. })
        ));
    }

    #[test]
    fn malformed_envelopes_are_rejected_before_history_validation() {
        let malformed = json!({
            "id": "bad/id",
            "seq": 1,
            "at": "2026-07-27T12:00:00Z",
            "actor": "test",
            "type": "example.changed",
            "body": {},
            "schema": "example.v1"
        });
        assert!(serde_json::from_value::<Record>(malformed).is_err());
    }

    #[test]
    fn only_full_object_ids_are_accepted_as_git_revisions() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"b".repeat(64)));
        assert!(!is_object_id("refs/heads/log"));
        assert!(!is_object_id(&"g".repeat(40)));
    }
}
