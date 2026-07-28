use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use alder_log::{
    AppendDisposition, EventType, GitLog, Head, Log, MemoryLog, RecordDraft, RecordId, SchemaId,
};
use chrono::{TimeZone, Utc};
use serde_json::json;

fn draft(id: &str, by: u64) -> RecordDraft {
    RecordDraft::new(
        RecordId::try_from(id).unwrap(),
        Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0).unwrap(),
        "integration-test",
        EventType::try_from("example.counter.incremented").unwrap(),
        json!({"by": by}),
        SchemaId::try_from("example.counter.v1").unwrap(),
    )
    .unwrap()
}

fn exercise(log: &impl Log) {
    let empty = log.head().unwrap();
    assert_eq!(empty, Head::empty());
    let first_draft = draft("one", 1);
    let first = log.append(&empty, &first_draft).unwrap();
    assert_eq!(first.disposition, AppendDisposition::Appended);
    assert_eq!(first.record.sequence(), 1);
    let first_head = first.observed_head.clone();

    let second = log.append(&first.observed_head, &draft("two", 2)).unwrap();
    assert_eq!(second.record.sequence(), 2);
    assert_eq!(second.observed_head.sequence(), 2);
    assert_eq!(
        log.read(&first_head, 0)
            .unwrap()
            .into_iter()
            .map(|record| record.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["one"]
    );
    assert_eq!(
        log.read(&second.observed_head, 1)
            .unwrap()
            .into_iter()
            .map(|record| record.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["two"]
    );
    assert!(matches!(
        log.read(&first_head, 2),
        Err(alder_log::LogError::InvalidRange {
            after: 2,
            through: 1
        })
    ));
    assert!(matches!(
        log.append(&first_head, &draft("three", 3)),
        Err(alder_log::LogError::HeadConflict { .. })
    ));
    assert_eq!(log.read_all(&second.observed_head).unwrap().len(), 2);

    let replay = log.append(&empty, &first_draft).unwrap();
    assert_eq!(replay.disposition, AppendDisposition::AlreadyPresent);
    assert_eq!(replay.observed_head, second.observed_head);
    assert_eq!(replay.record.sequence(), 1);
    assert!(matches!(
        log.append(&second.observed_head, &draft("one", 9)),
        Err(alder_log::LogError::RecordIdCollision { .. })
    ));
}

#[test]
fn public_api_supports_an_arbitrary_event_in_memory_and_git() {
    exercise(&MemoryLog::new());

    let (_temporary, _remote, local) = setup_git();
    exercise(&GitLog::new(local, "origin", "refs/heads/log"));
}

#[test]
fn exactly_one_memory_writer_wins_from_a_shared_head() {
    let log = Arc::new(MemoryLog::new());
    let expected = log.head().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = [("one", 1), ("two", 2)]
        .into_iter()
        .map(|(id, by)| {
            let log = log.clone();
            let expected = expected.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                log.append(&expected, &draft(id, by))
            })
        })
        .collect();
    barrier.wait();
    assert_one_winner(handles);
    assert_eq!(log.read_all(&log.head().unwrap()).unwrap().len(), 1);
}

#[test]
fn exactly_one_git_writer_wins_from_a_shared_head() {
    let (temporary, remote, first_root) = setup_git();
    let second_root = temporary.path().join("second");
    init_local(temporary.path(), &second_root, &remote);
    let first = GitLog::new(first_root, "origin", "refs/heads/log");
    let second = GitLog::new(second_root, "origin", "refs/heads/log");
    let expected = first.head().unwrap();
    assert_eq!(second.head().unwrap(), expected);
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = [(first.clone(), "one", 1), (second, "two", 2)]
        .into_iter()
        .map(|(log, id, by)| {
            let expected = expected.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                log.append(&expected, &draft(id, by))
            })
        })
        .collect();
    barrier.wait();
    assert_one_winner(handles);
    assert_eq!(first.read_all(&first.head().unwrap()).unwrap().len(), 1);
}

#[test]
fn git_rejects_a_log_shaped_revision_outside_the_authoritative_history() {
    let (_temporary, _remote, local) = setup_git();
    let primary = GitLog::new(&local, "origin", "refs/heads/log");
    let unrelated = GitLog::new(&local, "origin", "refs/heads/unrelated");
    primary
        .append(&Head::empty(), &draft("primary", 1))
        .unwrap();
    let unrelated_head = unrelated
        .append(&Head::empty(), &draft("unrelated", 1))
        .unwrap()
        .observed_head;

    assert!(matches!(
        primary.read_all(&unrelated_head),
        Err(alder_log::LogError::InvalidHead { .. })
    ));
}

#[test]
fn stale_local_and_tracking_refs_do_not_override_the_remote() {
    let (temporary, remote, local) = setup_git();
    let shadow_remote = temporary.path().join("shadow.git");
    run(
        temporary.path(),
        &["init", "--bare", shadow_remote.to_str().unwrap()],
    );
    run(
        &local,
        &["remote", "add", "shadow", shadow_remote.to_str().unwrap()],
    );
    let shadow = GitLog::new(&local, "shadow", "refs/heads/log");
    let stale = shadow
        .append(&Head::empty(), &draft("local-only", 1))
        .unwrap()
        .observed_head;
    let stale_commit = stale.revision().unwrap().to_owned();
    run(&local, &["update-ref", "refs/heads/log", &stale_commit]);
    run(
        &local,
        &["update-ref", "refs/remotes/origin/log", &stale_commit],
    );

    let log = GitLog::new(&local, "origin", "refs/heads/log");
    assert_eq!(log.head().unwrap(), Head::empty());
    assert!(matches!(
        log.read_all(&stale),
        Err(alder_log::LogError::InvalidHead { .. })
    ));

    let writer_root = temporary.path().join("writer");
    init_local(temporary.path(), &writer_root, &remote);
    let writer = GitLog::new(writer_root, "origin", "refs/heads/log");
    let pushed = writer
        .append(&Head::empty(), &draft("remote-event", 2))
        .unwrap()
        .observed_head;

    let observed = log.head().unwrap();
    assert_eq!(observed, pushed);
    assert_ne!(observed.revision(), Some(stale_commit.as_str()));
    assert_eq!(
        log.read_all(&observed)
            .unwrap()
            .into_iter()
            .map(|record| record.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["remote-event"]
    );
    assert_eq!(
        reference(&local, "refs/heads/log").as_deref(),
        Some(stale_commit.as_str())
    );
}

#[cfg(unix)]
#[test]
fn a_lost_push_response_with_a_committed_record_resolves_to_already_present() {
    use std::os::unix::fs::PermissionsExt;

    let (temporary, remote, local) = setup_git();
    let log = GitLog::new(&local, "origin", "refs/heads/log");
    let first = log.append(&Head::empty(), &draft("one", 1)).unwrap();

    // Commit the identical second draft to a copy of the remote, then have the
    // hook publish that copy as the authoritative ref while failing the push,
    // as if the caller's own push had committed but its response was lost.
    let copy = temporary.path().join("copy.git");
    run(
        temporary.path(),
        &[
            "clone",
            "--bare",
            remote.to_str().unwrap(),
            copy.to_str().unwrap(),
        ],
    );
    let copy_local = temporary.path().join("copy-local");
    init_local(temporary.path(), &copy_local, &copy);
    let copy_log = GitLog::new(&copy_local, "origin", "refs/heads/log");
    let second_draft = draft("two", 2);
    copy_log
        .append(&first.observed_head, &second_draft)
        .unwrap();

    let hook = remote.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        format!(
            "#!/bin/sh\n\
             unset GIT_QUARANTINE_PATH GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES\n\
             git fetch --quiet \"{}\" refs/heads/log\n\
             git update-ref refs/heads/log FETCH_HEAD\n\
             echo 'push response lost by test' >&2\n\
             exit 1\n",
            copy.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).unwrap();

    let receipt = log.append(&first.observed_head, &second_draft).unwrap();
    assert_eq!(receipt.disposition, AppendDisposition::AlreadyPresent);
    assert_eq!(receipt.record.sequence(), 2);
    assert_eq!(receipt.observed_head.sequence(), 2);
    assert_eq!(receipt.observed_head, log.head().unwrap());
    assert_eq!(
        log.read_all(&receipt.observed_head)
            .unwrap()
            .into_iter()
            .map(|record| record.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
}

#[cfg(unix)]
#[test]
fn an_inconclusive_rejected_push_is_resolved_without_partial_visibility() {
    use std::os::unix::fs::PermissionsExt;

    let (_temporary, remote, local) = setup_git();
    let log = GitLog::new(local, "origin", "refs/heads/log");
    let first = log.append(&Head::empty(), &draft("one", 1)).unwrap();
    let hook = remote.join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\necho 'push rejected by test' >&2\nexit 1\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&hook, permissions).unwrap();

    assert!(matches!(
        log.append(&first.observed_head, &draft("two", 2)),
        Err(alder_log::LogError::UnknownOutcome { .. })
    ));
    let observed = log.head().unwrap();
    assert_eq!(observed, first.observed_head);
    assert_eq!(log.read_all(&observed).unwrap().len(), 1);
}

#[test]
fn a_record_cache_is_only_trusted_for_the_revision_that_wrote_it() {
    let (temporary, _remote, local) = setup_git();
    let cache = temporary.path().join("cache");
    let cached = || GitLog::new(&local, "origin", "refs/heads/log").with_cache(&cache);
    let first = cached().append(&Head::empty(), &draft("one", 1)).unwrap();
    let second = cached()
        .append(&first.observed_head, &draft("two", 2))
        .unwrap();
    let head = second.observed_head.clone();
    let revision = head.revision().unwrap().to_owned();

    // A later process reads the recorded revision back rather than the tree.
    let file = cache.join("records.json");
    assert!(file.is_file());
    assert_eq!(cached().read_all(&head).unwrap().len(), 2);

    // A cache naming a different revision describes a different tree, so it
    // is ignored and the records are read from Git again.
    let recorded = std::fs::read_to_string(&file).unwrap();
    let misfiled = recorded.replace(&revision, &"0".repeat(revision.len()));
    assert_ne!(misfiled, recorded);
    std::fs::write(&file, &misfiled).unwrap();
    assert_eq!(cached().read_all(&head).unwrap().len(), 2);

    // So is a cache from another log in the same directory, and one that is
    // not readable at all. Neither can answer for this revision.
    std::fs::write(
        &file,
        recorded.replace("refs/heads/log", "refs/heads/other"),
    )
    .unwrap();
    assert_eq!(cached().read_all(&head).unwrap().len(), 2);
    std::fs::write(&file, b"not a cache").unwrap();
    assert_eq!(cached().read_all(&head).unwrap().len(), 2);

    // A good cache is rewritten over any of that, and still describes the log.
    assert_eq!(
        cached()
            .read_all(&head)
            .unwrap()
            .into_iter()
            .map(|record| record.id().as_str().to_owned())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "alder-log-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create test directory: {error}"),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

fn setup_git() -> (TestDirectory, PathBuf, PathBuf) {
    let temporary = TestDirectory::new();
    let remote = temporary.path().join("remote.git");
    let local = temporary.path().join("local");
    run(
        temporary.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    init_local(temporary.path(), &local, &remote);
    (temporary, remote, local)
}

fn init_local(parent: &Path, local: &Path, remote: &Path) {
    run(parent, &["init", local.to_str().unwrap()]);
    run(
        local,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
}

fn assert_one_winner(
    handles: Vec<thread::JoinHandle<Result<alder_log::AppendReceipt, alder_log::LogError>>>,
) {
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(alder_log::LogError::HeadConflict { .. })))
            .count(),
        1
    );
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn reference(directory: &Path, name: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", name])
        .current_dir(directory)
        .output()
        .unwrap();
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn run(directory: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
