//! The orchestration, exercised against a fake world.
//!
//! The fake keeps a tiny model of the loop fold so the ordering rules the
//! driver depends on — intent before effects, one open pass, the crash-window
//! repair — are checked rather than asserted.

use std::{
    cell::RefCell,
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use alderd::{
    config::{Config, Engine},
    driver::Driver,
    effects::Effects,
    error::{DriverError, Result},
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{Value, json};

/// How many `alder show` polls the fake answers for one pass before it calls
/// the driver broken. A driver that waits between polls needs at most one per
/// minute of the pass timeout; one that never waits spins, and this turns that
/// spin into a failed test rather than a hung one.
const POLL_PATIENCE: u32 = 200;

#[derive(Default)]
struct World {
    /// The fake fold: the head, the open pass, and the last ended one.
    head: u64,
    open_pass: Option<(String, DateTime<Utc>, String)>,
    /// id, outcome, and the head the pass ended at.
    last_ended: Option<(String, String, u64)>,
    passes: u32,
    paused: bool,
    engine: Option<String>,
    rotate_pending: bool,
    /// How many `alder show` polls remain before the leader ends its pass.
    polls_until_report: u32,
    /// How many `alder show` polls remain before the engine session dies.
    polls_until_death: Option<u32>,
    /// `alder show` calls answered so far, against `POLL_PATIENCE`.
    shows: u32,
    session: Option<String>,
    attached: bool,
    /// What `alder refresh` reports about the observation sweep.
    refresh_changed: bool,
    /// The body of the pass document the driver hashes.
    pass_doc: String,
    /// The mtime of the local append marker, `None` when the file is absent.
    marker: Option<DateTime<Utc>>,
    calls: Vec<String>,
    logs: Vec<String>,
    notices: Vec<String>,
    /// Every `alder` call fails with this, standing in for a broken store or a
    /// missing binary.
    alder_error: Option<DriverError>,
    /// `alder loop wake` alone fails with this, once.
    wake_failure: Option<DriverError>,
    /// A pass another writer opens the instant this driver tries to wake.
    foreign_wake: Option<(String, String)>,
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
                    "rotate_pending": world.rotate_pending,
                    "open_pass": world.open_pass.as_ref().map(|(id, started, handle)| json!({
                        "id": id,
                        "engine": "claude",
                        "handle": handle,
                        "started_at": started,
                    })),
                    "last_pass": world.last_ended.as_ref().map(|(id, outcome, ended_seq)| json!({
                        "id": id,
                        "outcome": outcome,
                        "wake_at": null,
                        "ended_at": self.now(),
                        "ended_seq": ended_seq,
                    })),
                }
            })),
            ["refresh"] => Ok(json!({"changed": world.refresh_changed})),
            ["loop", "wake", ..] => {
                if let Some(error) = world.wake_failure.take() {
                    return Err(error);
                }
                if let Some((id, handle)) = world.foreign_wake.take() {
                    world.head += 1;
                    world.open_pass = Some((id, self.now(), handle));
                }
                if world.open_pass.is_some() {
                    return Err(DriverError::coded("pass_open", "a pass is open"));
                }
                world.passes += 1;
                world.head += 1;
                let id = format!("hm-pass-{}", world.passes);
                let started = self.now();
                let handle = args
                    .iter()
                    .position(|part| *part == "--handle")
                    .and_then(|index| args.get(index + 1))
                    .unwrap_or(&"tmux:alder-leader");
                world.open_pass = Some((id.clone(), started, (*handle).to_owned()));
                world.rotate_pending = false;
                Ok(json!({"pass_id": id}))
            }
            ["show", id] => {
                world.shows += 1;
                if world.shows > POLL_PATIENCE {
                    return Err(DriverError::new(
                        "the driver polled this pass without ever waiting",
                    ));
                }
                if let Some(remaining) = world.polls_until_death.as_mut() {
                    if *remaining == 0 {
                        world.session = None;
                        world.polls_until_death = None;
                    } else {
                        *remaining -= 1;
                    }
                }
                let open = world
                    .open_pass
                    .as_ref()
                    .is_some_and(|(open, _, _)| open == *id);
                if open && world.polls_until_report > 0 {
                    world.polls_until_report -= 1;
                    return Ok(json!({"current": {"state": "open", "outcome": null}}));
                }
                if open {
                    let id = (*id).to_owned();
                    world.open_pass = None;
                    world.head += 1;
                    let head = world.head;
                    world.last_ended = Some((id, "ok".to_owned(), head));
                }
                Ok(json!({"current": {"state": "ended", "outcome": "ok"}}))
            }
            ["pass", "end", id, "--outcome", outcome, ..] => {
                world.open_pass = None;
                world.head += 1;
                let head = world.head;
                world.last_ended = Some(((*id).to_owned(), (*outcome).to_owned(), head));
                Ok(json!({"pass_id": id, "outcome": outcome}))
            }
            other => Err(DriverError::new(format!("unexpected: {other:?}"))),
        }
    }

    fn tmux_session_exists(&self, session: &str) -> Result<bool> {
        let world = self.world.borrow();
        Ok(world.session.is_some() && session == "alder-leader")
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
        self.world
            .borrow_mut()
            .calls
            .push(format!("tmux send {session} {text}"));
        Ok(())
    }

    fn tmux_has_clients(&self, _session: &str) -> Result<bool> {
        Ok(self.world.borrow().attached)
    }

    fn read_file(&self, _path: &Path) -> Result<Vec<u8>> {
        Ok(self.world.borrow().pass_doc.clone().into_bytes())
    }

    fn file_mtime(&self, _path: &Path) -> Option<DateTime<Utc>> {
        self.world.borrow().marker
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
        pass_doc: "run one pass".to_owned(),
        ..World::default()
    }
}

/// A loop whose one pass ended at the current head, so nothing triggers until
/// the test makes something happen.
fn settled(engine: &str) -> World {
    World {
        passes: 1,
        last_ended: Some(("hm-pass-1".to_owned(), "ok".to_owned(), 0)),
        ..selected(engine)
    }
}

#[test]
fn a_cold_start_creates_the_session_bootstraps_and_records_intent_first() {
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
    // The session exists before the wake, the wake is durable before anything
    // is typed, and the first injection points at the pass document.
    assert!(positions("tmux new") < positions("alder loop wake"));
    assert!(positions("alder loop wake") < positions("tmux send"));
    assert!(calls.iter().any(|call| {
        call == "tmux send alder-leader Read .alder/PASS.md, then run one pass \
                 (pass-id: hm-pass-1; triggers: due)."
    }));
    assert!(calls.contains(&"alder show hm-pass-1".to_owned()));

    // The second pass reuses the session and drops the bootstrap sentence.
    // Another writer appends, advancing the head past the last pass's end.
    driver.effects().world.borrow_mut().head += 1;
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("tmux new"))
            .count(),
        1
    );
    assert!(calls.contains(
        &"tmux send alder-leader Run one pass (pass-id: hm-pass-2; triggers: log).".to_owned()
    ));
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
            .any(|call| call.contains("wake"))
    );

    // Another writer appended past the last pass's end, so the loop wants to
    // fire; the last pass ended now, so the ceiling has not elapsed and the
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
            .any(|call| call.contains("wake"))
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
            .any(|call| call.contains("loop wake"))
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
            .any(|call| call.contains("wake"))
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
fn a_pass_left_open_by_a_crash_is_closed_before_the_next_one_opens() {
    let mut abandoned = selected("claude");
    abandoned.passes = 1;
    abandoned.open_pass = Some((
        "hm-pass-1".to_owned(),
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() - TimeDelta::hours(4),
        "tmux:alder-leader".to_owned(),
    ));
    abandoned.session = Some("claude".to_owned());
    // The leader never reports, so only the driver can close the pass.
    abandoned.polls_until_report = u32::MAX;
    let mut driver = Driver::new(Fake::new(abandoned), config());

    // A daemon that finds an open pass adopts it rather than starting another.
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert!(!calls.iter().any(|call| call.contains("loop wake")));
    assert!(
        calls
            .iter()
            .any(|call| { call.starts_with("alder pass end hm-pass-1 --outcome timeout") })
    );
    assert!(
        driver
            .effects()
            .world
            .borrow()
            .notices
            .iter()
            .any(|notice| notice.contains("timed out"))
    );
}

#[test]
fn a_dead_session_ends_the_pass_as_crashed() {
    let mut running = selected("claude");
    running.polls_until_report = u32::MAX;
    running.polls_until_death = Some(1);
    let mut driver = Driver::new(Fake::new(running), config());
    // The engine dies as soon as the driver starts watching.
    driver.poll_once().unwrap_or_else(|error| panic!("{error}"));
    let world = driver.effects().world.borrow();
    assert_eq!(world.last_ended.as_ref().unwrap().1, "crashed");
    assert!(
        world
            .notices
            .iter()
            .any(|notice| notice.contains("crashed"))
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

    {
        let mut world = driver.effects().world.borrow_mut();
        world.rotate_pending = true;
        world.head += 1;
    }
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

    {
        let mut world = driver.effects().world.borrow_mut();
        world.engine = Some("codex".to_owned());
        world.head += 1;
    }
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        2
    );
    assert!(calls.contains(&"tmux new alder-leader codex".to_owned()));
}

#[test]
fn losing_a_wake_race_concedes_instead_of_ending_the_winner() {
    // The status read shows no open pass, but another writer opens one before
    // the wake lands. The living pass must survive untouched.
    struct Racer;
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    let _ = Racer;
    {
        let mut world = driver.effects().world.borrow_mut();
        // A foreign pass appears only once the driver tries to wake.
        world.foreign_wake = Some(("hm-pass-99".to_owned(), "codex:019f-live".to_owned()));
        world.polls_until_report = u32::MAX;
    }
    driver.poll_once().unwrap();

    let world = driver.effects().world.borrow();
    // Nothing was injected and, crucially, nothing was ended.
    assert!(!world.calls.iter().any(|call| call.starts_with("tmux send")));
    assert!(!world.calls.iter().any(|call| call.contains("pass end")));
    assert!(world.last_ended.is_none());
    assert_eq!(world.open_pass.as_ref().unwrap().0, "hm-pass-99");
    assert!(world.notices.is_empty());
}

#[test]
fn an_unobservable_foreign_pass_can_only_time_out() {
    // A pass another writer opened on Codex names no tmux session this driver
    // can check, so a dead local session must not make it "crashed".
    let mut foreign = selected("claude");
    foreign.passes = 1;
    foreign.open_pass = Some((
        "hm-pass-1".to_owned(),
        DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        "codex:019f-live".to_owned(),
    ));
    foreign.session = None;
    foreign.polls_until_report = u32::MAX;
    let mut driver = Driver::new(Fake::new(foreign), config());
    driver.poll_once().unwrap();

    let world = driver.effects().world.borrow();
    assert_eq!(world.last_ended.as_ref().unwrap().1, "timeout");
    assert!(
        !world
            .calls
            .iter()
            .any(|call| call.contains("--outcome crashed"))
    );
}

#[test]
fn a_pass_naming_another_tmux_session_is_judged_on_that_session() {
    let mut other = selected("claude");
    other.passes = 1;
    other.open_pass = Some((
        "hm-pass-1".to_owned(),
        DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
        "tmux:other-leader".to_owned(),
    ));
    // This driver's own session is alive; the pass's is not.
    other.session = Some("claude".to_owned());
    other.polls_until_report = u32::MAX;
    let mut driver = Driver::new(Fake::new(other), config());
    driver.poll_once().unwrap();

    let world = driver.effects().world.borrow();
    assert_eq!(world.last_ended.as_ref().unwrap().1, "crashed");
    // The driver's own session record must survive: it was not the casualty.
    assert!(world.session.is_some());
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
    for _ in 0..3 {
        driver.poll_once().unwrap();
    }
    let notices = driver.effects().world.borrow().notices.clone();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0], notices[1]);
}

#[test]
fn a_fresh_marker_cuts_the_wait_short_and_a_missing_one_is_silently_fine() {
    // With the marker always ahead of the baseline, the driver never sleeps:
    // it watches the pass to its end without the clock advancing at all.
    let mut hinted = selected("claude");
    hinted.polls_until_report = 3;
    hinted.marker = Some(DateTime::from_timestamp(2_000_000_000, 0).unwrap());
    let mut driver = Driver::new(Fake::new(hinted), config());
    let start = driver.effects().now();
    driver.poll_once().unwrap();
    assert_eq!(driver.effects().now(), start);
    // Cutting a wait short is worth saying out loud, and worth saying why.
    assert!(
        driver
            .effects()
            .logs()
            .iter()
            .any(|line| line == "polling early: the local append marker moved"),
        "{:?}",
        driver.effects().logs()
    );
    let world = driver.effects().world.borrow();
    assert_eq!(world.last_ended.as_ref().unwrap().1, "ok");
    drop(world);

    // Without a marker the same pass is watched on the ordinary cadence:
    // three open polls, each followed by one full 60-second interval.
    let mut unhinted = selected("claude");
    unhinted.polls_until_report = 3;
    let mut driver = Driver::new(Fake::new(unhinted), config());
    let start = driver.effects().now();
    driver.poll_once().unwrap();
    assert_eq!((driver.effects().now() - start).num_seconds(), 180);
    // A wait that ran its course is the ordinary case and says nothing.
    assert!(
        !driver
            .effects()
            .logs()
            .iter()
            .any(|line| line.starts_with("polling early")),
        "{:?}",
        driver.effects().logs()
    );
    let world = driver.effects().world.borrow();
    assert_eq!(world.last_ended.as_ref().unwrap().1, "ok");
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
fn an_observation_change_wakes_the_loop_on_its_own() {
    // Nothing was appended and no deadline has passed: the refresh sweep is
    // the only reason to run, and it is reason enough.
    let mut observed = settled("claude");
    observed.refresh_changed = true;
    let mut driver = Driver::new(Fake::new(observed), config());
    driver.poll_once().unwrap();

    let calls = driver.effects().calls();
    assert!(calls.contains(&"alder refresh".to_owned()));
    let woke = "alder loop wake --engine claude --handle tmux:alder-leader \
                --trigger observations";
    assert!(calls.iter().any(|call| call == woke), "{calls:?}");
}

#[test]
fn a_changed_pass_document_starts_a_new_era() {
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    driver.poll_once().unwrap();

    // The operator edits the pass document. The running engine has already
    // read the old one, so the session cannot be reused.
    {
        let mut world = driver.effects().world.borrow_mut();
        world.pass_doc = "run one pass, but differently".to_owned();
        world.head += 1;
    }
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
    let bootstrap = "tmux send alder-leader Read .alder/PASS.md, then run one pass \
                     (pass-id: hm-pass-2; triggers: log).";
    assert!(calls.contains(&bootstrap.to_owned()), "{calls:?}");
}

#[test]
fn a_wake_that_fails_for_any_other_reason_is_not_conceded() {
    // Conceding is only ever right when another writer won the race. A wake
    // that failed for any other reason must surface, not be swallowed as a
    // polite loss.
    let mut broken = selected("claude");
    broken.wake_failure = Some(DriverError::new("alder is not on PATH"));
    let mut driver = Driver::new(Fake::new(broken), config());
    assert!(driver.poll_once().is_err());

    let world = driver.effects().world.borrow();
    assert!(!world.calls.iter().any(|call| call.starts_with("tmux send")));
    assert!(
        !world
            .logs
            .iter()
            .any(|line| line.contains("conceding this wake"))
    );
}

#[test]
fn another_writers_dead_session_is_not_this_drivers_era_boundary() {
    let mut driver = Driver::new(Fake::new(selected("claude")), config());
    driver.poll_once().unwrap();

    // A pass on another writer's tmux session is open, and that session is
    // gone. Ending it as crashed says nothing about this driver's own engine.
    {
        let mut world = driver.effects().world.borrow_mut();
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        world.open_pass = Some(("hm-pass-9".to_owned(), now, "tmux:other-leader".to_owned()));
        world.polls_until_report = u32::MAX;
        world.head += 1;
    }
    driver.poll_once().unwrap();
    assert_eq!(
        driver
            .effects()
            .world
            .borrow()
            .last_ended
            .as_ref()
            .unwrap()
            .1,
        "crashed"
    );

    // So the next fire reuses the session this driver started, rather than
    // treating it as a stranger's and rotating it away.
    {
        let mut world = driver.effects().world.borrow_mut();
        world.polls_until_report = 0;
        world.head += 1;
    }
    driver.poll_once().unwrap();
    let calls = driver.effects().calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux kill")).count(),
        0,
        "{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("tmux new")).count(),
        1,
        "{calls:?}"
    );
}
