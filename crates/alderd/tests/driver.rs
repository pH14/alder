//! The orchestration, exercised against a fake world.
//!
//! The fake serves the loop section the way production packs it, remembers
//! every effect, and rejects any `alder` invocation beyond the driver's two
//! reads — which is itself the contract: the driver reads `status` and
//! `refresh`, and appends nothing.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use alderd::{
    config::{Config, Engine},
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
    engine: Option<String>,
    rotate_requested_seq: Option<u64>,
    nudge_requested_seq: Option<u64>,
    review_deadlines: Vec<DateTime<Utc>>,
    session: Option<String>,
    attached: bool,
    /// One pending observation change: the next `alder refresh` reports it,
    /// and — like the real sweep, which appends observation events — moves
    /// the head as it does.
    refresh_changed: bool,
    /// Whether injections fail, standing in for a torn `send-keys` whose
    /// Enter never landed.
    injection_fails: bool,
    /// The body of the pass document the driver hashes.
    pass_doc: String,
    /// Paths passed to the hash reader, in order.
    pass_doc_reads: Vec<PathBuf>,
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
        match args {
            ["status"] => Ok(json!({
                "head": world.head,
                "loop": {
                    "paused": world.paused,
                    "pause_reason": null,
                    "engine": world.engine,
                    "rotate_requested_seq": world.rotate_requested_seq,
                    "nudge_requested_seq": world.nudge_requested_seq,
                    "review_at": world.review_deadlines.iter().min(),
                    "review_deadlines": world.review_deadlines,
                }
            })),
            ["refresh"] => {
                // A changed sweep appends observation events, so reporting a
                // change moves the head — the coupling the driver must absorb
                // without waking twice for its own sweep.
                let changed = world.refresh_changed;
                if changed {
                    world.head += 1;
                    world.refresh_changed = false;
                }
                Ok(json!({"changed": changed}))
            }
            // The driver's complete read surface. Anything else — above all a
            // mutation — is a contract violation, not a stub to add.
            other => Err(DriverError::new(format!(
                "the driver ran `alder {}`, which is not one of its two reads",
                other.join(" ")
            ))),
        }
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let world = self.world.borrow();
        Ok(world.session.is_some() && session == "alder-executor")
    }

    fn tmux_new_session(&self, session: &str, engine: &Engine) -> Result<()> {
        let mut world = self.world.borrow_mut();
        world
            .calls
            .push(format!("tmux new {session} {}", engine.cmd));
        world.session = Some(engine.cmd.clone());
        Ok(())
    }

    fn tmux_kill_session(&self, session: &str) -> Result<()> {
        let mut world = self.world.borrow_mut();
        world.calls.push(format!("tmux kill {session}"));
        world.session = None;
        Ok(())
    }

    fn tmux_send_keys(&self, session: &str, text: &str) -> Result<()> {
        let mut world = self.world.borrow_mut();
        if world.injection_fails {
            return Err(DriverError::new("tmux send-keys Enter failed: torn"));
        }
        world.calls.push(format!("tmux send {session} {text}"));
        Ok(())
    }

    fn tmux_has_clients(&self, _session: &str) -> Result<bool> {
        Ok(self.world.borrow().attached)
    }

    fn read_file(&self, path: &Path) -> Result<Vec<u8>> {
        let mut world = self.world.borrow_mut();
        if path == Path::new(NOTES) {
            return world
                .notes_file
                .clone()
                .ok_or_else(|| DriverError::new("no notes yet"));
        }
        world.pass_doc_reads.push(path.to_path_buf());
        Ok(world.pass_doc.clone().into_bytes())
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
    alderd::decide::config_for(&[("claude", "claude"), ("codex", "codex")])
}

fn selected(engine: &str) -> World {
    World {
        engine: Some(engine.to_owned()),
        pass_doc: "read the state, act".to_owned(),
        ..World::default()
    }
}

/// Notes claiming head 0 was acted on now, so nothing triggers until the test
/// makes something happen.
fn settled(engine: &str) -> World {
    World {
        notes_file: Some(
            serde_json::to_vec(&json!({
                "lastHead": 0,
                "lastWakeAt": DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            }))
            .unwrap(),
        ),
        ..selected(engine)
    }
}

#[test]
fn a_cold_start_creates_the_session_bootstraps_and_notes_the_head_it_acted_on() {
    let fake = Fake::new(selected("claude"));
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();

    let calls = driver.effects().calls();
    let positions = |needle: &str| {
        calls
            .iter()
            .position(|call| call.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle} in {calls:?}"))
    };
    // The session exists before the injection, the first injection points at
    // the pass document, and nothing is ever appended.
    assert!(positions("tmux new") < positions("tmux send"));
    assert!(calls.iter().any(|call| {
        call == "tmux send alder-executor Read .agent/skills/pass/SKILL.md, then read the current \
                 Alder state and act on it (triggers: due)."
    }));
    assert!(
        calls.iter().all(|call| call == "alder status"
            || call == "alder refresh"
            || !call.starts_with("alder")),
        "the driver ran an alder command beyond its two reads: {calls:?}"
    );
    // The delivery was noted durably.
    let notes: Value =
        serde_json::from_slice(driver.effects().world.borrow().notes_file.as_ref().unwrap())
            .unwrap();
    assert_eq!(notes["lastHead"], 0);
    assert!(notes["lastWakeAt"].is_string());

    // With the head noted and the wake fresh, the next poll is idle.
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().logs().last().unwrap(),
        "idle: nothing changed"
    );

    // Another writer appends; the second wake reuses the session and drops
    // the bootstrap sentence.
    driver.effects().world.borrow_mut().head += 1;
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("tmux new"))
            .count(),
        1
    );
    assert!(
        calls.contains(
            &"tmux send alder-executor Read the current Alder state and act on it (triggers: log)."
                .to_owned()
        )
    );
}

#[test]
fn a_restarted_daemon_reads_its_notes_back_and_does_not_rewake_for_old_state() {
    let fake = Fake::new(selected("claude"));
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();
    let woke: usize = driver
        .effects()
        .calls()
        .iter()
        .filter(|call| call.starts_with("tmux send"))
        .count();
    assert_eq!(woke, 1);

    // The process dies; a new driver starts over the same world. The notes
    // file is what keeps it from delivering the same wake again.
    let mut world = std::mem::take(&mut *driver.effects().world.borrow_mut());
    world.calls.clear();
    let mut restarted = Driver::new(Fake::new(world), config());
    restarted.poll_once().unwrap();
    assert!(
        !restarted
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send")),
        "{:?}",
        restarted.effects().calls()
    );
}

#[test]
fn a_lost_notes_file_costs_one_harmless_duplicate_wake() {
    let fake = Fake::new(selected("claude"));
    let mut driver = Driver::new(fake, config());
    driver.poll_once().unwrap();

    // The machine loses the notes. The restarted daemon wakes once more for
    // state already acted on — the harmless direction — and re-notes it.
    let mut world = std::mem::take(&mut *driver.effects().world.borrow_mut());
    world.calls.clear();
    world.notes_file = None;
    let mut restarted = Driver::new(Fake::new(world), config());
    restarted.poll_once().unwrap();
    assert_eq!(
        restarted
            .effects()
            .calls()
            .iter()
            .filter(|call| call.starts_with("tmux send"))
            .count(),
        1
    );
    // And only once: the note is back.
    restarted.poll_once().unwrap();
    assert_eq!(
        restarted
            .effects()
            .calls()
            .iter()
            .filter(|call| call.starts_with("tmux send"))
            .count(),
        1
    );
}

#[test]
fn a_failed_notes_write_is_logged_and_the_wake_still_lands() {
    let mut world = selected("claude");
    world.notes_write_fails = true;
    let mut driver = Driver::new(Fake::new(world), config());
    driver.poll_once().unwrap();
    assert!(
        driver
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send"))
    );
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
fn a_pause_idles_and_an_attached_client_only_holds() {
    let mut paused = selected("claude");
    paused.paused = true;
    let mut driver = Driver::new(Fake::new(paused), config());
    driver.poll_once().unwrap();
    assert!(
        !driver
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send"))
    );

    // Another writer appended past the noted head, so the loop wants to fire;
    // the last wake was just now, so the ceiling has not elapsed and the
    // attached client defers the injection rather than cancelling it.
    let mut attached = settled("claude");
    attached.attached = true;
    attached.head = 1;
    let mut driver = Driver::new(Fake::new(attached), config());
    driver.poll_once().unwrap();
    assert!(
        !driver
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send"))
    );

    // The human detaches; the same trigger, now settled, fires.
    driver.effects().world.borrow_mut().attached = false;
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    assert!(
        driver
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send"))
    );
}

#[test]
fn an_unknown_or_unchosen_engine_notifies_instead_of_guessing() {
    let mut driver = Driver::new(Fake::new(selected("gemini")), config());
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().world.borrow().notices,
        ["engine `gemini` is not configured on this host"]
    );
    assert!(
        !driver
            .effects()
            .calls()
            .iter()
            .any(|call| call.starts_with("tmux send"))
    );

    let unchosen = World::default();
    let mut driver = Driver::new(Fake::new(unchosen), config());
    driver.poll_once().unwrap();
    assert_eq!(
        driver.effects().world.borrow().notices,
        ["no engine is selected; run `alder loop use <engine>`"]
    );
}

#[test]
fn a_rotation_and_an_engine_swap_each_replace_the_session() {
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    driver.poll_once().unwrap();
    assert_eq!(
        driver
            .effects()
            .calls()
            .iter()
            .filter(|c| c.starts_with("tmux new"))
            .count(),
        1
    );

    // A rotation request lands: its own append moves the head, and its
    // sequence is past the head the driver noted.
    {
        let mut world = driver.effects().world.borrow_mut();
        world.head += 1;
        world.rotate_requested_seq = Some(world.head);
    }
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        1
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux new")).count(),
        2
    );

    // Acting consumed the rotation: the next fire reuses the fresh session.
    {
        let mut world = driver.effects().world.borrow_mut();
        world.head += 1;
    }
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    assert_eq!(
        driver
            .effects()
            .calls()
            .iter()
            .filter(|c| c.starts_with("tmux new"))
            .count(),
        2
    );

    {
        let mut world = driver.effects().world.borrow_mut();
        world.engine = Some("codex".to_owned());
        world.head += 1;
    }
    driver.effects().advance(30);
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        2
    );
    assert!(calls.contains(&"tmux new alder-executor codex".to_owned()));
}

#[test]
fn an_aged_session_is_rotated_at_its_wall_clock_budget() {
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    driver.poll_once().unwrap();

    // Fire repeatedly under the age budget: the session is reused.
    driver.effects().world.borrow_mut().head += 1;
    driver.effects().advance(3_600);
    driver.poll_once().unwrap();
    assert_eq!(
        driver
            .effects()
            .calls()
            .iter()
            .filter(|c| c.starts_with("tmux new"))
            .count(),
        1
    );

    // Past the default 21600-second budget the next fire replaces it.
    driver.effects().world.borrow_mut().head += 1;
    driver.effects().advance(21_600);
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        1
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux new")).count(),
        2
    );
}

#[test]
fn a_standing_engine_problem_is_reported_once() {
    let mut driver = Driver::new(Fake::new(selected("gemini")), config());
    for _ in 0..5 {
        driver.poll_once().unwrap();
    }
    assert_eq!(
        driver.effects().world.borrow().notices,
        ["engine `gemini` is not configured on this host"]
    );

    // Choosing a configured engine clears the condition; a later relapse is
    // news again.
    driver.effects().world.borrow_mut().engine = Some("claude".to_owned());
    driver.poll_once().unwrap();
    {
        let mut world = driver.effects().world.borrow_mut();
        world.engine = Some("gemini".to_owned());
        // Another writer appends, so the loop wants to fire again.
        world.head += 1;
    }
    driver.effects().advance(30);
    for _ in 0..3 {
        driver.poll_once().unwrap();
    }
    let notices = driver.effects().world.borrow().notices.clone();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0], notices[1]);
}

#[test]
fn a_repeated_store_outage_is_reported_once() {
    let mut down = selected("claude");
    down.alder_error = Some(DriverError::coded("store_unavailable", "no remote"));
    let mut driver = Driver::new(Fake::new(down), config());
    for _ in 0..5 {
        assert!(driver.poll_once().is_err());
    }
    let notices = driver.effects().world.borrow().notices.clone();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("unavailable"));
}

#[test]
fn only_a_store_outage_counts_toward_the_outage_notice() {
    // Any other failure is a local fault — a missing binary, a bad config —
    // and the operator's own shell reports those. Counting them here would
    // announce an outage the store is not having.
    let mut broken = selected("claude");
    broken.alder_error = Some(DriverError::new("alder is not on PATH"));
    let mut driver = Driver::new(Fake::new(broken), config());
    for _ in 0..5 {
        assert!(driver.poll_once().is_err());
    }
    assert!(driver.effects().world.borrow().notices.is_empty());
}

#[test]
fn an_observation_change_wakes_the_executor_on_its_own() {
    // No one else appended and no deadline has passed: the refresh sweep is
    // the reason to run, and it is reason enough. The sweep's own appends
    // move the head, so the log trigger honestly rides along.
    let mut observed = settled("claude");
    observed.refresh_changed = true;
    let mut driver = Driver::new(Fake::new(observed), config());
    driver.poll_once().unwrap();

    let calls = driver.effects().calls();
    assert!(calls.contains(&"alder refresh".to_owned()));
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("tmux send")
                && call.contains("(triggers: log,observations)")),
        "{calls:?}"
    );
}

#[test]
fn one_observation_change_produces_exactly_one_wake() {
    // The sweep appends, so the head the driver read before refreshing is
    // stale by the time it fires. The status is re-read before noting, and
    // the next poll finds nothing new — not the driver's own sweep.
    let mut observed = settled("claude");
    observed.refresh_changed = true;
    let mut driver = Driver::new(Fake::new(observed), config());
    let sends = |driver: &Driver<Fake>| {
        driver
            .effects()
            .calls()
            .iter()
            .filter(|call| call.starts_with("tmux send"))
            .count()
    };

    driver.poll_once().unwrap();
    assert_eq!(sends(&driver), 1);

    driver.poll_once().unwrap();
    assert_eq!(
        sends(&driver),
        1,
        "the head moved by the driver's own sweep must not wake again"
    );
}

#[test]
fn a_torn_injection_is_not_noted_and_the_next_poll_retries_delivery() {
    let mut torn = selected("claude");
    torn.injection_fails = true;
    let mut driver = Driver::new(Fake::new(torn), config());

    // The Enter never lands: the poll fails and the notes do not advance,
    // because noting an undelivered wake would silence it until the ceiling.
    assert!(driver.poll_once().is_err());
    assert!(
        driver.effects().world.borrow().notes_file.is_none(),
        "a failed delivery must not advance the notes"
    );

    // The next poll retries the same delivery, and only then notes it.
    driver.effects().world.borrow_mut().injection_fails = false;
    driver.poll_once().unwrap();
    assert_eq!(
        driver
            .effects()
            .calls()
            .iter()
            .filter(|call| call.starts_with("tmux send"))
            .count(),
        1
    );
    assert!(driver.effects().world.borrow().notes_file.is_some());
}

#[test]
fn a_deferral_deadline_wakes_the_executor_once_at_its_instant() {
    let mut deferred = settled("claude");
    deferred.review_deadlines = vec![
        DateTime::from_timestamp(1_800_000_000 + 600, 0).unwrap(),
        DateTime::from_timestamp(1_800_000_000 + 1_200, 0).unwrap(),
    ];
    let mut driver = Driver::new(Fake::new(deferred), config());
    let sends = |driver: &Driver<Fake>| {
        driver
            .effects()
            .calls()
            .iter()
            .filter(|call| call.starts_with("tmux send"))
            .count()
    };

    // Before the first instant, nothing.
    driver.poll_once().unwrap();
    assert_eq!(sends(&driver), 0);

    // At the first instant, one wake with `due` provenance.
    driver.effects().advance(600);
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert!(
        calls
            .iter()
            .any(|call| call.starts_with("tmux send") && call.contains("(triggers: due)")),
        "{calls:?}"
    );

    // The executor did not touch the item; that deadline does not fire again —
    // the ceiling, not a per-poll retry, is the backstop.
    driver.poll_once().unwrap();
    assert_eq!(sends(&driver), 1);

    // The second deadline still earns its own wake at its own instant, with
    // nothing else having happened: the wake for the first must not have
    // consumed it.
    driver.effects().advance(600);
    driver.poll_once().unwrap();
    assert_eq!(sends(&driver), 2, "{:?}", driver.effects().calls());

    // And it, too, fires only once.
    driver.poll_once().unwrap();
    assert_eq!(sends(&driver), 2);
}

#[test]
fn a_changed_pass_skill_starts_a_new_era_and_hashes_that_skill() {
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    driver.poll_once().unwrap();

    // The operator edits the pass document. The running engine has already
    // read the old one, so the session cannot be reused.
    {
        let mut world = driver.effects().world.borrow_mut();
        world.pass_doc = "read the state, act, but differently".to_owned();
        world.head += 1;
    }
    driver.effects().advance(30);
    driver.poll_once().unwrap();

    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        1
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux new")).count(),
        2
    );
    // A fresh engine has read nothing, so it is pointed at the document again.
    let bootstrap = "tmux send alder-executor Read .agent/skills/pass/SKILL.md, then read the \
                     current Alder state and act on it (triggers: log).";
    assert!(calls.contains(&bootstrap.to_owned()), "{calls:?}");
    assert_eq!(
        driver.effects().world.borrow().pass_doc_reads,
        vec![
            PathBuf::from(".agent/skills/pass/SKILL.md"),
            PathBuf::from(".agent/skills/pass/SKILL.md"),
        ],
        "the era hash must read the same skill the bootstrap injection names"
    );
}
