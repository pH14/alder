//! The orchestration, exercised against a fake world.
//!
//! The fake serves the loop section the way production packs it, remembers
//! every effect, and rejects any `alder` invocation beyond the driver's one
//! read — which is itself the contract: the driver reads `status`, runs the
//! configured command when a trigger fires, and appends nothing.

use std::{
    cell::RefCell,
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use alderd::{
    config::Config,
    driver::Driver,
    effects::Effects,
    error::{DriverError, Result},
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

#[derive(Default)]
struct World {
    /// The fake fold: the head and the loop section's durable statements.
    head: u64,
    paused: bool,
    nudge_requested_seq: Option<u64>,
    review_deadlines: Vec<DateTime<Utc>>,
    /// Whether the configured command fails, standing in for a wake command
    /// that crashed part-way.
    command_fails: bool,
    /// How long the configured command takes, in clock seconds. The fake
    /// advances its clock by this much inside `run_command`, standing in for
    /// a slow executor pass.
    command_duration_seconds: i64,
    /// Serve a truncated status document — a loop section with no `paused` —
    /// standing in for a store that answers with something unusable.
    partial_status: bool,
    /// Every command run: `(command, triggers)` pairs, in order.
    commands: Vec<(String, String)>,
    /// The machine-local notes file, as bytes on the fake disk.
    notes_file: Option<Vec<u8>>,
    /// Whether notes writes fail, standing in for a full disk.
    notes_write_fails: bool,
    calls: Vec<String>,
    logs: Vec<String>,
    notices: Vec<String>,
    /// Every `alder` call fails with this, standing in for a broken store or a
    /// missing binary.
    alder_error: Option<DriverError>,
}

struct Fake {
    world: RefCell<World>,
    clock: AtomicI64,
}

impl Fake {
    fn new(world: World) -> Self {
        Self {
            world: RefCell::new(world),
            clock: AtomicI64::new(1_800_000_000),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.world.borrow().calls.clone()
    }

    fn logs(&self) -> Vec<String> {
        self.world.borrow().logs.clone()
    }

    fn commands(&self) -> Vec<(String, String)> {
        self.world.borrow().commands.clone()
    }

    /// Move the clock without going through a sleep the driver chose.
    fn advance(&self, seconds: i64) {
        self.clock.fetch_add(seconds, Ordering::SeqCst);
    }
}

const NOTES: &str = ".alder/alderd-notes.json";

impl Effects for Fake {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.clock.load(Ordering::SeqCst), 0).expect("a valid instant")
    }

    fn alder(&self, args: &[&str]) -> Result<Value> {
        let mut world = self.world.borrow_mut();
        world.calls.push(format!("alder {}", args.join(" ")));
        if let Some(error) = world.alder_error.clone() {
            return Err(error);
        }
        if world.partial_status {
            // Parsed exactly the way production parses it, so the error and
            // its code are the real ones rather than a stub's.
            return alderd::loop_state::LoopState::from_status(&json!({
                "head": world.head,
                "loop": {}
            }))
            .map(|_| unreachable!("a paused-less loop section must not parse"));
        }
        match args {
            ["status"] => Ok(json!({
                "head": world.head,
                "loop": {
                    "paused": world.paused,
                    "pause_reason": null,
                    "engine": null,
                    "rotate_requested_seq": null,
                    "nudge_requested_seq": world.nudge_requested_seq,
                    "review_at": world.review_deadlines.iter().min(),
                    "review_deadlines": world.review_deadlines,
                }
            })),
            // The driver's complete read surface. Anything else — above all a
            // mutation — is a contract violation, not a stub to add.
            other => Err(DriverError::new(format!(
                "the driver ran `alder {}`, which is not its one read",
                other.join(" ")
            ))),
        }
    }

    fn run_command(&self, command: &str, triggers: &str) -> Result<()> {
        let mut world = self.world.borrow_mut();
        if world.command_fails {
            return Err(DriverError::new("the command exited with exit status: 1"));
        }
        self.clock
            .fetch_add(world.command_duration_seconds, Ordering::SeqCst);
        world.calls.push(format!("command {command} [{triggers}]"));
        world
            .commands
            .push((command.to_owned(), triggers.to_owned()));
        Ok(())
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let world = self.world.borrow();
        assert_eq!(
            path,
            Path::new(NOTES),
            "the driver reads nothing but its own notes"
        );
        world
            .notes_file
            .clone()
            .ok_or_else(|| DriverError::new("no notes yet"))
    }

    fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        let mut world = self.world.borrow_mut();
        assert_eq!(
            path,
            Path::new(NOTES),
            "the driver writes nothing but its own notes"
        );
        if world.notes_write_fails {
            return Err(DriverError::new("disk full"));
        }
        world.notes_file = Some(bytes.to_vec());
        Ok(())
    }

    fn file_mtime(&self, _path: &Path) -> Option<DateTime<Utc>> {
        None
    }

    fn notify(&self, message: &str) {
        self.world.borrow_mut().notices.push(message.to_owned());
    }

    fn sleep(&self, duration: Duration) {
        self.clock
            .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
    }

    fn log(&self, message: &str) {
        self.world.borrow_mut().logs.push(message.to_owned());
    }
}

fn config() -> Config {
    alderd::decide::config_for("run-the-executor")
}

/// Notes claiming head 0 was acted on now, so nothing triggers until the test
/// makes something happen.
fn settled() -> World {
    World {
        notes_file: Some(
            serde_json::to_vec(&json!({
                "lastHead": 0,
                "lastWakeAt": DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            }))
            .unwrap(),
        ),
        ..World::default()
    }
}

#[test]
fn a_cold_start_runs_the_command_once_and_notes_the_head_it_acted_on() {
    let fake = Fake::new(World::default());
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();

    // Fresh notes: the ceiling fires immediately, and the command is run with
    // the trigger provenance in its environment.
    assert_eq!(
        driver.effects().commands(),
        [("run-the-executor".to_owned(), "due".to_owned())]
    );
    let calls = driver.effects().calls();
    assert!(
        calls
            .iter()
            .all(|call| call == "alder status" || !call.starts_with("alder")),
        "the driver ran an alder command beyond its one read: {calls:?}"
    );
    // The run was noted durably.
    let notes: Value =
        serde_json::from_slice(driver.effects().world.borrow().notes_file.as_ref().unwrap())
            .unwrap();
    assert_eq!(notes["lastHead"], 0);
    assert!(notes["lastWakeAt"].is_string());

    // With the head noted and the run fresh, the next poll is idle.
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().logs().last().unwrap(),
        "idle: nothing changed"
    );

    // Another writer appends; the next wake carries the log trigger.
    driver.effects().world.borrow_mut().head += 1;
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().commands().last().unwrap().1,
        "log".to_owned()
    );
}

#[test]
fn a_restarted_daemon_reads_its_notes_back_and_does_not_rerun_for_old_state() {
    let fake = Fake::new(World::default());
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);

    // The process dies; a new driver starts over the same world. The notes
    // file is what keeps it from running the same wake again.
    let mut world = std::mem::take(&mut *driver.effects().world.borrow_mut());
    world.calls.clear();
    world.commands.clear();
    let mut restarted = Driver::new(Fake::new(world), config());
    restarted.poll_once().unwrap();
    assert!(
        restarted.effects().commands().is_empty(),
        "{:?}",
        restarted.effects().commands()
    );
}

#[test]
fn a_lost_notes_file_costs_one_harmless_duplicate_run() {
    let fake = Fake::new(World::default());
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();

    // The machine loses the notes. The restarted daemon runs once more for
    // state already acted on — the harmless direction — and re-notes it.
    let mut world = std::mem::take(&mut *driver.effects().world.borrow_mut());
    world.calls.clear();
    world.commands.clear();
    world.notes_file = None;
    let mut restarted = Driver::new(Fake::new(world), config());
    restarted.poll_once().unwrap();
    assert_eq!(restarted.effects().commands().len(), 1);
    // And only once: the note is back.
    restarted.poll_once().unwrap();
    assert_eq!(restarted.effects().commands().len(), 1);
}

#[test]
fn a_failed_notes_write_is_logged_and_the_run_still_lands() {
    let world = World {
        notes_write_fails: true,
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(world), config());
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);
    assert!(
        driver
            .effects()
            .logs()
            .iter()
            .any(|line| line.contains("cannot persist the driver notes")),
        "{:?}",
        driver.effects().logs()
    );
}

#[test]
fn a_pause_idles_without_running_anything() {
    let paused = World {
        paused: true,
        head: 9,
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(paused), config());
    driver.poll_once().unwrap();
    assert!(driver.effects().commands().is_empty());
    assert_eq!(
        driver.effects().logs().last().unwrap(),
        "idle: the loop is paused"
    );
}

#[test]
fn a_repeated_store_outage_is_reported_once() {
    let down = World {
        alder_error: Some(DriverError::coded("store_unavailable", "no remote")),
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(down), config());
    for _ in 0..5 {
        assert!(driver.poll_once().is_err());
    }
    let notices = driver.effects().world.borrow().notices.clone();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("unavailable"));
}

#[test]
fn a_partial_status_document_is_an_outage_never_default_unpaused() {
    // A loop section missing `paused` must not decide anything — above all
    // it must not read as "unpaused" and run the command — and it counts
    // toward the outage notice like a store that did not answer at all.
    let mut truncated = settled();
    truncated.partial_status = true;
    truncated.head = 42;
    let mut driver = Driver::new(Fake::new(truncated), config());
    for _ in 0..5 {
        let error = driver
            .poll_once()
            .expect_err("a partial status is an error");
        assert!(error.is("malformed_status"), "{error}");
    }
    assert!(
        driver.effects().commands().is_empty(),
        "a truncated status ran the command: {:?}",
        driver.effects().commands()
    );
    let notices = driver.effects().world.borrow().notices.clone();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("unavailable"), "{notices:?}");
}

#[test]
fn a_local_fault_never_counts_toward_the_outage_notice() {
    // A store outage and an unusable status document count; any other
    // failure is a local fault — a missing binary, a bad config — and the
    // operator's own shell reports those. Counting them here would announce
    // an outage the store is not having.
    let broken = World {
        alder_error: Some(DriverError::new("alder is not on PATH")),
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(broken), config());
    for _ in 0..5 {
        assert!(driver.poll_once().is_err());
    }
    assert!(driver.effects().world.borrow().notices.is_empty());
}

#[test]
fn a_failed_command_is_not_noted_and_the_next_poll_reruns_it() {
    let torn = World {
        command_fails: true,
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(torn), config());

    // The command dies: the poll fails and the notes do not advance, because
    // noting an unfinished wake would silence it until the ceiling.
    assert!(driver.poll_once().is_err());
    assert!(
        driver.effects().world.borrow().notes_file.is_none(),
        "a failed command must not advance the notes"
    );

    // The next poll reruns the same wake, and only then notes it. Crash
    // re-runs; the command is idempotent by the waking design.
    driver.effects().world.borrow_mut().command_fails = false;
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);
    assert!(driver.effects().world.borrow().notes_file.is_some());
}

#[test]
fn a_nudge_is_the_manual_trigger_and_is_consumed_by_acting() {
    let mut nudged = settled();
    nudged.head = 2;
    nudged.nudge_requested_seq = Some(2);
    let mut driver = Driver::new(Fake::new(nudged), config());
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().commands(),
        [("run-the-executor".to_owned(), "manual,log".to_owned())]
    );

    // Acting moved the note past the request: nothing more to do.
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);
}

#[test]
fn a_deferral_deadline_runs_the_command_once_at_its_instant() {
    let mut deferred = settled();
    deferred.review_deadlines = vec![
        DateTime::from_timestamp(1_800_000_000 + 600, 0).unwrap(),
        DateTime::from_timestamp(1_800_000_000 + 1_200, 0).unwrap(),
    ];
    let mut driver = Driver::new(Fake::new(deferred), config());

    // Before the first instant, nothing.
    driver.poll_once().unwrap();
    assert!(driver.effects().commands().is_empty());

    // At the first instant, one run with `due` provenance.
    driver.effects().advance(600);
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().commands(),
        [("run-the-executor".to_owned(), "due".to_owned())]
    );

    // The executor did not touch the item; that deadline does not fire again —
    // the ceiling, not a per-poll retry, is the backstop.
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);

    // The second deadline still earns its own run at its own instant, with
    // nothing else having happened: the run for the first must not have
    // consumed it.
    driver.effects().advance(600);
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 2);

    // And it, too, fires only once.
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 2);
}

#[test]
fn a_deadline_that_passes_while_the_command_runs_fires_on_the_next_poll() {
    // The wake is noted at its decision instant, not at the command's end.
    // One deadline fires at minute 10; the command then runs for five
    // minutes, during which the second deadline (minute 12) passes. Had the
    // note carried the command's end (minute 15), that second deadline would
    // be silently swallowed.
    let mut deferred = settled();
    deferred.command_duration_seconds = 300;
    deferred.review_deadlines = vec![
        DateTime::from_timestamp(1_800_000_000 + 600, 0).unwrap(),
        DateTime::from_timestamp(1_800_000_000 + 720, 0).unwrap(),
    ];
    let mut driver = Driver::new(Fake::new(deferred), config());

    driver.effects().advance(600);
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 1);

    // The next poll happens at minute 15. The deadline at minute 12 is owed
    // its wake: the decision that ran the command predates it.
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().commands().len(),
        2,
        "a deadline passing during the command run was swallowed by noting \
         the command's end instead of the decision instant"
    );
    // The note advanced past both deadlines now; nothing more fires.
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().commands().len(), 2);
}

#[test]
fn a_noted_wake_in_the_future_is_clamped_to_now_with_a_warning() {
    // The clock rolled back since the note was written: the notes claim a
    // wake far in the future. Unclamped, the ceiling and every deadline
    // would stay suppressed until the wall clock caught up.
    let future = World {
        notes_file: Some(
            serde_json::to_vec(&json!({
                "lastHead": 0,
                "lastWakeAt": DateTime::from_timestamp(1_900_000_000, 0).unwrap(),
            }))
            .unwrap(),
        ),
        ..World::default()
    };
    let mut driver = Driver::new(Fake::new(future), config());

    // The first poll clamps (warning logged, clamp persisted) and idles:
    // as-if the wake had just happened.
    driver.poll_once().unwrap();
    assert!(driver.effects().commands().is_empty());
    assert!(
        driver
            .effects()
            .logs()
            .iter()
            .any(|line| line.contains("clock rolled back")),
        "{:?}",
        driver.effects().logs()
    );
    let notes: Value =
        serde_json::from_slice(driver.effects().world.borrow().notes_file.as_ref().unwrap())
            .unwrap();
    let persisted: DateTime<Utc> = notes["lastWakeAt"]
        .as_str()
        .expect("the clamped note carries a wake time")
        .parse()
        .expect("the wake time parses");
    assert_eq!(
        persisted,
        DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        "the clamp was not persisted at the poll instant"
    );

    // The ceiling now measures from the clamped instant: it elapses on
    // schedule instead of decades late.
    driver.effects().advance(1800);
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().commands().len(),
        1,
        "the ceiling stayed suppressed by a future-dated note"
    );
}
