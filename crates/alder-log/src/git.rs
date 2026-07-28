use crate::{AppendDisposition, AppendReceipt, Head, Log, LogError, Record, RecordDraft};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

/// Schema tag of the on-disk record cache.
const RECORD_CACHE_SCHEMA: &str = "alder-log.records.v0";

/// A Git-backed log whose configured remote reference is authoritative.
#[derive(Debug, Clone)]
pub struct GitLog {
    root: PathBuf,
    remote: String,
    reference: String,
    cache: Option<PathBuf>,
    memo: Arc<Mutex<Memo>>,
}

/// Facts a Git revision fixes forever, remembered for this process only.
///
/// A revision names an immutable tree, and the authoritative ref only ever
/// fast-forwards, so neither entry can go stale within a process.
#[derive(Debug, Default)]
struct Memo {
    /// Revisions read from the authoritative remote or proved to precede it.
    authoritative: BTreeSet<String>,
    /// The decoded records of one revision.
    records: Option<(String, Arc<Vec<Record>>)>,
}

/// The on-disk record cache as written.
#[derive(Serialize)]
struct RecordCacheEntry<'a> {
    schema: &'a str,
    remote: &'a str,
    reference: &'a str,
    revision: &'a str,
    records: &'a [Record],
}

/// The on-disk record cache as read back.
#[derive(Deserialize)]
struct CachedRecords {
    schema: String,
    remote: String,
    reference: String,
    revision: String,
    records: Vec<Record>,
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
            cache: None,
            memo: Arc::new(Mutex::new(Memo::default())),
        }
    }
    /// Cache decoded records under `directory`, keyed by Git revision.
    ///
    /// A revision names an immutable tree, so a hit needs no revalidation and
    /// a read whose head has not moved touches no event body at all. The
    /// directory holds derived local data that is trusted for the revision it
    /// names, exactly like a rebuilt projection; deleting it costs one slower
    /// read and nothing else.
    pub fn with_cache(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cache = Some(directory.into());
        self
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
    /// A private ref naming the most recently fetched authoritative revision.
    ///
    /// It exists so one `fetch` can both publish the remote head and leave a
    /// name to resolve, and so the fetched objects stay reachable. It is
    /// force-updated from the remote immediately before every read and is
    /// never consulted on its own, so it cannot stand in for the remote. The
    /// digest keeps two logs in one repository on separate refs.
    ///
    /// Every reader of one log shares this name — worktrees of a repository
    /// share its refs — so concurrent reads race to update it. Git waits out
    /// a held ref lock (`core.filesRefLockTimeout`, 100ms by default) and then
    /// fails the update, and fails outright on one whose ref moved under it,
    /// so a read can lose that race. Whichever revision wins came from this
    /// same remote, so a loser loses nothing but the retry in
    /// `authoritative_revision`.
    fn anchor(&self) -> String {
        format!(
            "refs/alder-log/{:016x}",
            store_digest(&self.remote, &self.reference)
        )
    }
    fn fetch_anchor(&self, anchor: &str) -> Result<Output, LogError> {
        self.command([
            "fetch",
            "--no-tags",
            "--quiet",
            &self.remote,
            &format!("+{}:{anchor}", self.reference),
        ])
    }
    /// Bring the authoritative ref local and return the revision it names.
    ///
    /// One fetch does both jobs, so an ordinary read costs a single remote
    /// round trip instead of the `ls-remote` plus `fetch` pair it replaces.
    /// `ls-remote` runs only when that fetch fails, to separate an empty
    /// remote log from an unreachable one — and, between those, from a
    /// transfer that reached the log and failed on the anchor behind it.
    fn authoritative_revision(&self) -> Result<Option<String>, LogError> {
        let anchor = self.anchor();
        let mut fetched = self.fetch_anchor(&anchor)?;
        if !fetched.status.success() {
            if self.remote_revision()?.is_none() {
                return Ok(None);
            }
            // The remote answered, so that fetch read the log and failed
            // updating the anchor every reader of it shares: a concurrent read
            // took that ref first, and Git fails the losing update rather than
            // the transfer behind it. Nothing is lost by losing — the winner
            // read this same remote — so fetch once more, now from the value
            // that won.
            fetched = self.fetch_anchor(&anchor)?;
            if !fetched.status.success() {
                return Err(self.git_error("fetch the shared log", &fetched));
            }
        }
        let output = self.successful(
            ["rev-parse", "--verify", &format!("{anchor}^{{commit}}")],
            "resolve the fetched log head",
        )?;
        let revision = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !is_object_id(&revision) {
            return Err(LogError::Unavailable {
                message: "Git did not resolve the fetched log head to an object ID".to_owned(),
            });
        }
        self.remember_authoritative(&revision);
        Ok(Some(revision))
    }
    fn verify_authoritative_revision(&self, revision: &str) -> Result<(), LogError> {
        if !is_object_id(revision) {
            return Err(LogError::InvalidHead {
                message: "a Git head revision must be a full object ID".to_owned(),
            });
        }
        if self.remembers_authoritative(revision) {
            return Ok(());
        }
        let Some(authoritative) = self.authoritative_revision()? else {
            return Err(LogError::InvalidHead {
                message: "the authoritative remote log is empty".to_owned(),
            });
        };
        if revision == authoritative {
            return Ok(());
        }
        let object = self.command(["cat-file", "-e", &format!("{revision}^{{commit}}")])?;
        if !object.status.success() {
            return Err(LogError::InvalidHead {
                message: "the requested Git revision is unavailable".to_owned(),
            });
        }
        let output = self.command(["merge-base", "--is-ancestor", revision, &authoritative])?;
        if output.status.success() {
            self.remember_authoritative(revision);
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
    fn remember_authoritative(&self, revision: &str) {
        let mut memo = self.memo.lock().expect("the log memo lock was poisoned");
        memo.authoritative.insert(revision.to_owned());
    }
    fn remembers_authoritative(&self, revision: &str) -> bool {
        let memo = self.memo.lock().expect("the log memo lock was poisoned");
        memo.authoritative.contains(revision)
    }
    /// The records of `revision`, from memory, then the cache, then Git.
    fn records_at_revision(&self, revision: &str) -> Result<Arc<Vec<Record>>, LogError> {
        {
            let memo = self.memo.lock().expect("the log memo lock was poisoned");
            if let Some((remembered, records)) = &memo.records
                && remembered == revision
            {
                return Ok(records.clone());
            }
        }
        let records = match self.cached_records(revision) {
            Some(records) => Arc::new(records),
            None => {
                let records = Arc::new(self.read_records_at_revision(revision)?);
                self.cache_records(revision, &records);
                records
            }
        };
        let mut memo = self.memo.lock().expect("the log memo lock was poisoned");
        memo.records = Some((revision.to_owned(), records.clone()));
        Ok(records)
    }
    fn read_records_at_revision(&self, revision: &str) -> Result<Vec<Record>, LogError> {
        let output = self.successful(
            [
                "ls-tree",
                "-r",
                "--name-only",
                "-z",
                revision,
                "--",
                "events",
            ],
            "list records",
        )?;
        let mut paths: Vec<_> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| String::from_utf8_lossy(entry).into_owned())
            .filter(|path| is_event_path(path))
            .collect();
        paths.sort();
        let bodies = self.read_blobs(revision, &paths)?;
        let records = paths
            .iter()
            .zip(bodies)
            .map(|(path, body)| {
                serde_json::from_slice(&body).map_err(|error| LogError::InvalidLog {
                    message: format!("record `{path}` is invalid: {error}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_records(&records)?;
        Ok(records)
    }
    /// Read every listed event body through one `cat-file --batch`, so the
    /// number of Git processes a read costs does not grow with the log.
    fn read_blobs(&self, revision: &str, paths: &[String]) -> Result<Vec<Vec<u8>>, LogError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut requests = Vec::new();
        for path in paths {
            requests.extend_from_slice(format!("{revision}:{path}\n").as_bytes());
        }
        let mut child = Command::new("git")
            .args(["cat-file", "--batch"])
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| LogError::Unavailable {
                message: format!("failed to execute Git: {error}"),
            })?;
        let mut stdin = child.stdin.take().expect("cat-file stdin was piped");
        // Git answers each request as it reads it, so the requests have to be
        // written while this process drains the answers.
        let writer = thread::spawn(move || stdin.write_all(&requests));
        let output = child.wait_with_output()?;
        let written = writer
            .join()
            .map_err(|_| LogError::Io {
                message: "the Git request writer panicked".to_owned(),
            })
            .and_then(|result| result.map_err(LogError::from));
        if !output.status.success() {
            return Err(self.git_error("read records", &output));
        }
        written?;
        parse_batch(&output.stdout, paths)
    }
    fn cache_file(&self) -> Option<PathBuf> {
        self.cache
            .as_ref()
            .map(|directory| directory.join("records.json"))
    }
    fn cached_records(&self, revision: &str) -> Option<Vec<Record>> {
        let bytes = fs::read(self.cache_file()?).ok()?;
        let cached: CachedRecords = serde_json::from_slice(&bytes).ok()?;
        (cached.schema == RECORD_CACHE_SCHEMA
            && cached.remote == self.remote
            && cached.reference == self.reference
            && cached.revision == revision
            && validate_records(&cached.records).is_ok())
        .then_some(cached.records)
    }
    /// Replace the cache with one revision's records. The cache is derived
    /// data, so every failure here is dropped: it costs a slower read, never a
    /// wrong one.
    fn cache_records(&self, revision: &str, records: &[Record]) {
        let Some(path) = self.cache_file() else {
            return;
        };
        let Some(directory) = path.parent() else {
            return;
        };
        let entry = RecordCacheEntry {
            schema: RECORD_CACHE_SCHEMA,
            remote: &self.remote,
            reference: &self.reference,
            revision,
            records,
        };
        let Ok(bytes) = serde_json::to_vec(&entry) else {
            return;
        };
        if fs::create_dir_all(directory).is_err() {
            return;
        }
        // Write and rename so a concurrent reader sees one whole revision.
        // The name is unique per writer, including between threads, so two
        // writers cannot interleave into one temporary file and rename the
        // mixture over the cache.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let temporary = directory.join(format!(
            "records.json.{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::write(&temporary, &bytes).is_err() || fs::rename(&temporary, &path).is_err() {
            let _ = fs::remove_file(&temporary);
        }
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
        let Some(revision) = self.authoritative_revision()? else {
            return Ok(Head::empty());
        };
        Head::try_from_parts(
            self.records_at_revision(&revision)?.len() as u64,
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
                self.records_at_revision(revision)?
            }
            None if through.is_empty() => Arc::new(Vec::new()),
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
/// Split a `cat-file --batch` response stream into one body per request.
///
/// Git answers each request with `<oid> <type> <size>` and then exactly that
/// many bytes followed by a newline, or with a header ending in `missing`.
fn parse_batch(stdout: &[u8], paths: &[String]) -> Result<Vec<Vec<u8>>, LogError> {
    let mut rest = stdout;
    let mut bodies = Vec::with_capacity(paths.len());
    for path in paths {
        let truncated = || LogError::InvalidLog {
            message: format!("Git returned no readable content for record `{path}`"),
        };
        let end = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(truncated)?;
        let header = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end + 1..];
        let size = blob_size(&header).ok_or_else(|| LogError::InvalidLog {
            message: format!("record `{path}` is unreadable: {header}"),
        })?;
        // The body and the newline behind it. A size too large to add to is
        // one no stream can satisfy, so it is short like any other.
        let claimed = size.checked_add(1).ok_or_else(truncated)?;
        if rest.len() < claimed {
            return Err(truncated());
        }
        bodies.push(rest[..size].to_vec());
        rest = &rest[claimed..];
    }
    Ok(bodies)
}
fn blob_size(header: &str) -> Option<usize> {
    let mut fields = header.split(' ');
    let _object = fields.next()?;
    if fields.next()? != "blob" {
        return None;
    }
    fields.next()?.parse().ok()
}
/// FNV-1a over `remote`, a separator, and `reference`. This names a private
/// local ref and carries no security weight.
fn store_digest(remote: &str, reference: &str) -> u64 {
    let mut digest: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in remote.bytes().chain([0]).chain(reference.bytes()) {
        digest ^= u64::from(byte);
        digest = digest.wrapping_mul(0x0000_0100_0000_01b3);
    }
    digest
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

    fn paths(count: usize) -> Vec<String> {
        (0..count)
            .map(|index| format!("events/{index}.json"))
            .collect()
    }

    #[test]
    fn a_batch_response_is_split_by_the_size_its_header_declares() {
        // A record body contains newlines, so only the declared size can say
        // where it ends.
        let mut stdout = b"aaa blob 4\n{\n}\n\n".to_vec();
        stdout.extend_from_slice(b"bbb blob 2\n{}\n");
        assert_eq!(
            parse_batch(&stdout, &paths(2)).unwrap(),
            [b"{\n}\n".to_vec(), b"{}".to_vec()]
        );
        assert!(parse_batch(b"", &[]).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_batch_response_names_the_record_it_failed_on() {
        let unreadable = |stdout: &[u8]| match parse_batch(stdout, &paths(1)) {
            Err(LogError::InvalidLog { message }) => message,
            other => panic!("expected an invalid log, got {other:?}"),
        };
        // Git reports an object it does not have, and answers for other kinds
        // of object than the blobs a record has to be.
        assert!(unreadable(b"aaa missing\n").contains("events/0.json"));
        assert!(unreadable(b"aaa tree 4\n1234\n").contains("events/0.json"));
        // A stream that stops early is not silently short, and a size no
        // stream could satisfy is short rather than a panic.
        assert!(unreadable(b"").contains("events/0.json"));
        assert!(unreadable(b"aaa blob 9\n{}\n").contains("events/0.json"));
        assert!(
            unreadable(format!("aaa blob {}\n", usize::MAX).as_bytes()).contains("events/0.json")
        );
        // Two headers, one body: the second record has nothing behind it.
        let mut stdout = b"aaa blob 2\n{}\n".to_vec();
        stdout.extend_from_slice(b"bbb blob 2\n");
        assert!(matches!(
            parse_batch(&stdout, &paths(2)),
            Err(LogError::InvalidLog { .. })
        ));
    }

    #[test]
    fn only_blob_headers_carry_a_size() {
        assert_eq!(blob_size("aaa blob 12"), Some(12));
        assert_eq!(blob_size("aaa blob 0"), Some(0));
        assert_eq!(blob_size("aaa missing"), None);
        assert_eq!(blob_size("aaa tree 12"), None);
        assert_eq!(blob_size("aaa blob twelve"), None);
        assert_eq!(blob_size("aaa blob"), None);
        assert_eq!(blob_size(""), None);
    }

    #[test]
    fn two_logs_in_one_repository_get_separate_anchor_refs() {
        let anchor =
            |remote: &str, reference: &str| GitLog::new("/repository", remote, reference).anchor();
        assert_ne!(
            anchor("origin", "refs/heads/log"),
            anchor("origin", "refs/heads/other")
        );
        assert_ne!(
            anchor("origin", "refs/heads/log"),
            anchor("upstream", "refs/heads/log")
        );
        // The separator keeps a split of the same characters distinct.
        assert_ne!(anchor("ab", "c"), anchor("a", "bc"));
        assert_eq!(
            anchor("origin", "refs/heads/log"),
            anchor("origin", "refs/heads/log")
        );
    }

    #[test]
    fn only_full_object_ids_are_accepted_as_git_revisions() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"b".repeat(64)));
        assert!(!is_object_id("refs/heads/log"));
        assert!(!is_object_id(&"g".repeat(40)));
    }
}
