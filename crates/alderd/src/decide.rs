//! Every decision the driver makes, as pure functions over a snapshot.
//!
//! Nothing here talks to tmux, Git, or the Alder CLI. The daemon's judgment
//! surface is small on purpose, and keeping it here is what makes that
//! claim checkable.
//!
//! The wake rule is one comparison: the head has moved past the last head this
//! driver acted on. That baseline lives in the driver's machine-local
//! [`Notes`], never in the log — the log never mentions its own readers — and
//! losing it is harmless: the driver acts once more, the leader reads the fold,
//! finds nothing new to do, and idles. Passes are idempotent; a missed or
//! duplicated wake changes nothing durable.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::{config::Config, loop_state::LoopState};

/// Why the driver would wake the leader. These kinds are informational
/// provenance in the injected line; they never limit what the leader must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    Manual,
    Log,
    Observations,
    Due,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Log => "log",
            Self::Observations => "observations",
            Self::Due => "due",
        }
    }
}

/// What this driver remembers about its own past actions, persisted to a
/// machine-local file under `.alder/`. This is the generalization of the old
/// append marker: not "something was appended" but "the last head I acted on".
///
/// The file carries zero durable-project weight. A driver that loses it acts
/// once more than it needed to, and acting on an unchanged state is a no-op.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notes {
    /// The head this driver last delivered a wake for.
    #[serde(default)]
    pub last_head: u64,
    /// When this driver last delivered a wake.
    #[serde(default)]
    pub last_wake_at: Option<DateTime<Utc>>,
}

/// Everything the driver observed this poll that is not already in the loop
/// fold or its own notes.
#[derive(Debug, Clone)]
pub struct Poll {
    pub now: DateTime<Utc>,
    /// `alder refresh --json` reported a semantic observation change.
    pub refresh_changed: bool,
    /// When the fire condition first became true, for debouncing.
    pub pending_since: Option<DateTime<Utc>>,
    /// A human is attached to the tmux session and should not be interrupted.
    pub attached_client: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do; sleep until the next poll.
    Idle(&'static str),
    /// The condition holds but the injection waits.
    Hold(&'static str),
    /// Wake the leader, with these trigger kinds as provenance.
    Fire(Vec<Trigger>),
}

/// A rotation request is outstanding for this driver when it is later in the
/// log than the last head the driver acted on. Acting consumes it by moving
/// the noted head past it; no clearing write exists, because the log does not
/// record its readers.
pub fn rotate_pending(state: &LoopState, notes: &Notes) -> bool {
    state
        .rotate_requested_seq
        .is_some_and(|requested| requested > notes.last_head)
}

/// A nudge follows the identical rule over its own request sequence.
pub fn nudge_pending(state: &LoopState, notes: &Notes) -> bool {
    state
        .nudge_requested_seq
        .is_some_and(|requested| requested > notes.last_head)
}

/// Which trigger kinds currently hold. Empty means nothing has happened.
pub fn triggers(config: &Config, state: &LoopState, notes: &Notes, poll: &Poll) -> Vec<Trigger> {
    let mut triggers = Vec::new();
    if nudge_pending(state, notes) {
        triggers.push(Trigger::Manual);
    }
    // "Differs", not "moved past": a rebuilt or truncated log ref moves the
    // head backwards, and waking once then re-noting self-heals, while
    // suppressing until the ceiling does not.
    if state.head != notes.last_head {
        triggers.push(Trigger::Log);
    }
    if poll.refresh_changed {
        triggers.push(Trigger::Observations);
    }
    if review_due(state, notes, poll.now) || max_interval_elapsed(config, notes, poll.now) {
        triggers.push(Trigger::Due);
    }
    triggers
}

/// A deferral's deadline has arrived and this driver has not woken the leader
/// since it passed. One wake per deadline: a leader that reviews the item
/// moves the deadline or removes it, and a leader that does not is caught by
/// the max-interval ceiling rather than woken every poll. Every blocked
/// item's deadline is checked, not just the earliest: an item that stays
/// blocked past its own deadline must not swallow the wake a later deadline
/// is owed.
fn review_due(state: &LoopState, notes: &Notes, now: DateTime<Utc>) -> bool {
    state
        .review_deadlines
        .iter()
        .any(|&deadline| now >= deadline && notes.last_wake_at.is_none_or(|woke| woke < deadline))
}

/// The leader has not been woken for longer than the configured ceiling. A
/// driver with no notes has never woken anyone, so a fresh start fires
/// immediately.
fn max_interval_elapsed(config: &Config, notes: &Notes, now: DateTime<Utc>) -> bool {
    let ceiling = TimeDelta::seconds(config.max_interval_seconds as i64);
    match notes.last_wake_at {
        Some(woke) => now >= woke + ceiling,
        None => true,
    }
}

/// The one decision the driver makes each poll.
pub fn decide(config: &Config, state: &LoopState, notes: &Notes, poll: &Poll) -> Decision {
    if state.paused {
        return Decision::Idle("the loop is paused");
    }
    let triggers = triggers(config, state, notes, poll);
    if triggers.is_empty() {
        return Decision::Idle("nothing changed");
    }
    // The ceiling overrides both deferrals: a loop that never runs is worse
    // than an injection landing under someone's cursor. A pending nudge does
    // the same, because a nudge is the human overriding that politeness.
    let urgent = max_interval_elapsed(config, notes, poll.now) || nudge_pending(state, notes);
    if poll.attached_client && !urgent {
        return Decision::Hold("a client is attached to the session");
    }
    let settled = poll
        .pending_since
        .is_none_or(|since| poll.now >= since + TimeDelta::seconds(config.debounce_seconds as i64));
    if !settled && !urgent {
        return Decision::Hold("debouncing");
    }
    Decision::Fire(triggers)
}

/// One step of the wait between full polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Sleep this long, then ask again.
    Sleep(std::time::Duration),
    /// Stop waiting and run the next full poll now.
    Poll(&'static str),
}

/// The driver waits out its poll interval in hint-sized slices, statting the
/// local append marker between slices. The marker is a hint with zero
/// correctness weight: it only ever cuts the wait short, causing a status
/// read that would have happened anyway, and a missing marker (`None`) simply
/// lets the interval run its course.
pub fn next_wait(
    config: &Config,
    waited: std::time::Duration,
    baseline: DateTime<Utc>,
    marker: Option<DateTime<Utc>>,
) -> Wait {
    if marker.is_some_and(|moved| moved > baseline) {
        return Wait::Poll("the local append marker moved");
    }
    if waited >= config.poll() {
        return Wait::Poll("the poll interval elapsed");
    }
    Wait::Sleep(config.hint_poll().min(config.poll() - waited))
}

/// Which engine the driver should actually run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineChoice {
    Run(String),
    /// The desired engine is not configured on this machine.
    Unknown(String),
    /// No engine was chosen and the machine offers more than one.
    Ambiguous,
}

pub fn resolve_engine(config: &Config, state: &LoopState) -> EngineChoice {
    match state.engine.as_deref() {
        Some(engine) if config.engines.contains_key(engine) => EngineChoice::Run(engine.to_owned()),
        Some(engine) => EngineChoice::Unknown(engine.to_owned()),
        None if config.engines.len() == 1 => EngineChoice::Run(
            config
                .engines
                .keys()
                .next()
                .expect("exactly one engine")
                .clone(),
        ),
        None => EngineChoice::Ambiguous,
    }
}

/// What the daemon remembers about the tmux session it launched. Nothing here
/// is durable: a daemon restart forgets it and restarts the session, which is
/// the safe direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub engine: String,
    pub pass_doc_hash: u64,
    /// When this daemon created the session. Rotation is by wall-clock age:
    /// nothing durable counts passes, so nothing counts them here either.
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// Inject into the running session.
    Reuse,
    /// Kill it, then create a fresh one for the stated reason.
    Restart(&'static str),
    /// Nothing is running; create one.
    Create,
}

pub fn session_action(
    config: &Config,
    rotate_pending: bool,
    engine: &str,
    pass_doc_hash: u64,
    exists: bool,
    known: Option<&Session>,
    now: DateTime<Utc>,
) -> SessionAction {
    if !exists {
        return SessionAction::Create;
    }
    let Some(session) = known else {
        // Something is running that this daemon did not start. It may be any
        // engine at any age, so treat it as an era that must end.
        return SessionAction::Restart("the running session is not this daemon's");
    };
    if session.engine != engine {
        return SessionAction::Restart("the desired engine changed");
    }
    if rotate_pending {
        return SessionAction::Restart("a rotation is pending");
    }
    if session.pass_doc_hash != pass_doc_hash {
        return SessionAction::Restart("the pass document changed");
    }
    if now >= session.created_at + TimeDelta::seconds(config.max_session_age_seconds as i64) {
        return SessionAction::Restart("the session reached its age budget");
    }
    SessionAction::Reuse
}

/// The message injected into the leader's terminal.
///
/// It carries no identifier, because nothing durable exists to identify: the
/// leader reads the fold and acts on whatever the state demands. Trigger kinds
/// ride along as provenance and never say "only look at X".
pub fn injection(bootstrap: bool, pass_doc: &str, triggers: &[Trigger]) -> String {
    let kinds = triggers
        .iter()
        .map(|trigger| trigger.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let kinds = if kinds.is_empty() {
        "none".to_owned()
    } else {
        kinds
    };
    if bootstrap {
        format!(
            "Read {pass_doc}, then read the current Alder state and act on it (triggers: {kinds})."
        )
    } else {
        format!("Read the current Alder state and act on it (triggers: {kinds}).")
    }
}

/// Reports a persistent condition once, and again only when it changes or
/// clears. An operator should hear about an unconfigured engine, not hear
/// about it every poll forever.
#[derive(Debug, Default)]
pub struct Notice {
    current: Option<String>,
}

impl Notice {
    /// Raise a condition. True exactly when the operator should be told.
    pub fn raise(&mut self, condition: &str) -> bool {
        if self.current.as_deref() == Some(condition) {
            return false;
        }
        self.current = Some(condition.to_owned());
        true
    }

    /// The condition no longer holds; the next occurrence is news again.
    pub fn clear(&mut self) {
        self.current = None;
    }
}

/// A stable content hash for the pass document. Any change is an era boundary,
/// so only equality matters and a small non-cryptographic hash is enough.
pub fn content_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Build a config in code, for tests and for callers that never read a file.
pub fn config_for(engines: &[(&str, &str)]) -> Config {
    let map: BTreeMap<String, crate::config::Engine> = engines
        .iter()
        .map(|(name, cmd)| {
            (
                (*name).to_owned(),
                crate::config::Engine {
                    cmd: (*cmd).to_owned(),
                    args: Vec::new(),
                },
            )
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "engines": map.iter().map(|(name, engine)| {
            (name.clone(), serde_json::json!({"cmd": engine.cmd}))
        }).collect::<serde_json::Map<_, _>>(),
        "passDoc": ".agent/skills/pass/SKILL.md",
    }))
    .expect("a generated config is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + minutes * 60, 0).expect("a valid instant")
    }

    fn poll(now: i64) -> Poll {
        Poll {
            now: at(now),
            refresh_changed: false,
            pending_since: None,
            attached_client: false,
        }
    }

    /// A loop at head 40 with no requests and no deferral.
    fn settled_state() -> LoopState {
        LoopState {
            head: 40,
            ..LoopState::default()
        }
    }

    /// A driver that acted on head 40 at `minutes` in.
    fn acted(minutes: i64) -> Notes {
        Notes {
            last_head: 40,
            last_wake_at: Some(at(minutes)),
        }
    }

    #[test]
    fn each_trigger_kind_has_exactly_one_cause() {
        let config = config_for(&[("claude", "claude")]);

        // The head still stands where the driver left it.
        assert!(triggers(&config, &settled_state(), &acted(0), &poll(1)).is_empty());

        let mut moved = settled_state();
        moved.head = 41;
        assert_eq!(
            triggers(&config, &moved, &acted(0), &poll(1)),
            vec![Trigger::Log]
        );

        // Fresh notes have never woken anyone: the ceiling fires immediately,
        // and any nonzero head is also a log trigger.
        let fresh = Notes::default();
        assert_eq!(
            triggers(&config, &settled_state(), &fresh, &poll(0)),
            vec![Trigger::Log, Trigger::Due]
        );
        assert_eq!(
            triggers(&config, &LoopState::default(), &fresh, &poll(0)),
            vec![Trigger::Due]
        );

        let mut observed = poll(1);
        observed.refresh_changed = true;
        assert_eq!(
            triggers(&config, &settled_state(), &acted(0), &observed),
            vec![Trigger::Observations]
        );

        // A deferral deadline is a due trigger once, at its instant.
        let mut deferred = settled_state();
        deferred.review_deadlines = vec![at(20)];
        assert!(triggers(&config, &deferred, &acted(0), &poll(19)).is_empty());
        assert_eq!(
            triggers(&config, &deferred, &acted(0), &poll(20)),
            vec![Trigger::Due]
        );
        // A wake delivered after the deadline consumes it; the ceiling is the
        // backstop for a leader that never reviews the item.
        assert!(triggers(&config, &deferred, &acted(21), &poll(22)).is_empty());

        // A head behind the note is still a difference: a rebuilt or
        // truncated log ref wakes the leader once, and re-noting self-heals.
        let mut rebuilt = settled_state();
        rebuilt.head = 39;
        assert_eq!(
            triggers(&config, &rebuilt, &acted(0), &poll(1)),
            vec![Trigger::Log]
        );

        // The ceiling is 1800 seconds by default.
        assert_eq!(
            triggers(&config, &settled_state(), &acted(0), &poll(30)),
            vec![Trigger::Due]
        );
        assert!(triggers(&config, &settled_state(), &acted(0), &poll(29)).is_empty());

        // A nudge request past the noted head is the manual trigger; the
        // request event itself also moved the head.
        let mut nudged = settled_state();
        nudged.head = 41;
        nudged.nudge_requested_seq = Some(41);
        assert_eq!(
            triggers(&config, &nudged, &acted(0), &poll(1)),
            vec![Trigger::Manual, Trigger::Log]
        );
        // One already acted on is consumed: the noted head has passed it.
        let consumed = Notes {
            last_head: 41,
            last_wake_at: Some(at(0)),
        };
        assert!(triggers(&config, &nudged, &consumed, &poll(1)).is_empty());

        let mut all = settled_state();
        all.head = 41;
        all.nudge_requested_seq = Some(41);
        let mut everything = poll(30);
        everything.refresh_changed = true;
        assert_eq!(
            triggers(&config, &all, &acted(0), &everything),
            vec![
                Trigger::Manual,
                Trigger::Log,
                Trigger::Observations,
                Trigger::Due
            ]
        );
        assert_eq!(Trigger::Manual.as_str(), "manual");
        assert_eq!(Trigger::Log.as_str(), "log");
        assert_eq!(Trigger::Observations.as_str(), "observations");
        assert_eq!(Trigger::Due.as_str(), "due");
    }

    #[test]
    fn each_deadline_earns_its_own_wake() {
        let config = config_for(&[("claude", "claude")]);
        let mut deferred = settled_state();
        deferred.review_deadlines = vec![at(10), at(20)];

        // The first deadline fires at its instant.
        assert!(triggers(&config, &deferred, &acted(0), &poll(9)).is_empty());
        assert_eq!(
            triggers(&config, &deferred, &acted(0), &poll(10)),
            vec![Trigger::Due]
        );
        // The wake for the first deadline does not consume the second: the
        // item behind the first stays blocked, and the second deadline still
        // earns its wake at its own instant, with no other trigger holding.
        assert!(triggers(&config, &deferred, &acted(10), &poll(19)).is_empty());
        assert_eq!(
            triggers(&config, &deferred, &acted(10), &poll(20)),
            vec![Trigger::Due]
        );
        // And a wake past the second consumes both.
        assert!(triggers(&config, &deferred, &acted(21), &poll(22)).is_empty());
    }

    #[test]
    fn requests_are_pending_only_past_the_noted_head() {
        let state = |rotate, nudge| LoopState {
            rotate_requested_seq: rotate,
            nudge_requested_seq: nudge,
            ..LoopState::default()
        };
        let notes = |last_head| Notes {
            last_head,
            last_wake_at: None,
        };
        assert!(!rotate_pending(&state(None, None), &notes(0)));
        assert!(rotate_pending(&state(Some(3), None), &notes(0)));
        assert!(rotate_pending(&state(Some(5), None), &notes(4)));
        assert!(!rotate_pending(&state(Some(4), None), &notes(4)));
        assert!(!rotate_pending(&state(Some(3), None), &notes(4)));

        assert!(!nudge_pending(&state(None, None), &notes(0)));
        assert!(nudge_pending(&state(None, Some(5)), &notes(4)));
        assert!(!nudge_pending(&state(None, Some(4)), &notes(4)));
        // Each is consumed independently of the other.
        assert!(!rotate_pending(&state(None, Some(5)), &notes(4)));
    }

    #[test]
    fn pause_outranks_every_trigger() {
        let config = config_for(&[("claude", "claude")]);
        let mut paused = settled_state();
        paused.paused = true;
        paused.head = 99;
        assert_eq!(
            decide(&config, &paused, &acted(0), &poll(30)),
            Decision::Idle("the loop is paused")
        );

        assert_eq!(
            decide(&config, &settled_state(), &acted(0), &poll(1)),
            Decision::Idle("nothing changed")
        );
    }

    #[test]
    fn debounce_and_attachment_hold_the_injection_but_not_past_the_ceiling() {
        let config = config_for(&[("claude", "claude")]);
        // Another writer appended past the noted head.
        let mut moved = settled_state();
        moved.head = 41;
        let mut fresh = poll(1);
        fresh.pending_since = Some(at(1));
        assert_eq!(
            decide(&config, &moved, &acted(0), &fresh),
            Decision::Hold("debouncing")
        );

        // The default debounce is 20 seconds, so the next poll clears it.
        let mut settled = fresh.clone();
        settled.now = at(2);
        assert_eq!(
            decide(&config, &moved, &acted(0), &settled),
            Decision::Fire(vec![Trigger::Log])
        );

        let mut attached = settled.clone();
        attached.attached_client = true;
        assert_eq!(
            decide(&config, &moved, &acted(0), &attached),
            Decision::Hold("a client is attached to the session")
        );

        // Past the ceiling both deferrals give way.
        let mut overdue = attached.clone();
        overdue.now = at(31);
        overdue.pending_since = Some(at(31));
        assert_eq!(
            decide(&config, &moved, &acted(0), &overdue),
            Decision::Fire(vec![Trigger::Log, Trigger::Due])
        );
    }

    #[test]
    fn a_nudge_fires_through_both_deferrals_but_respects_pause() {
        let config = config_for(&[("claude", "claude")]);
        let mut nudged = settled_state();
        nudged.head = 41;
        nudged.nudge_requested_seq = Some(41);

        // Debounce has not settled and a client is attached; a nudge fires
        // anyway, because it is the human overriding the driver's politeness.
        let mut held = poll(1);
        held.pending_since = Some(at(1));
        held.attached_client = true;
        assert_eq!(
            decide(&config, &nudged, &acted(0), &held),
            Decision::Fire(vec![Trigger::Manual, Trigger::Log])
        );

        // Pause still outranks it.
        let mut paused = nudged.clone();
        paused.paused = true;
        assert_eq!(
            decide(&config, &paused, &acted(0), &held),
            Decision::Idle("the loop is paused")
        );
    }

    #[test]
    fn the_wait_slices_by_the_hint_and_a_moved_marker_cuts_it_short() {
        use std::time::Duration;

        let config = config_for(&[("claude", "claude")]);
        let baseline = at(0);
        let slice = Duration::from_secs(1);

        // A missing marker is silently fine: the interval runs its course,
        // one hint-sized slice at a time. The default poll is 60 seconds.
        assert_eq!(
            next_wait(&config, Duration::ZERO, baseline, None),
            Wait::Sleep(slice)
        );
        assert_eq!(
            next_wait(&config, Duration::from_secs(59), baseline, None),
            Wait::Sleep(slice)
        );
        assert_eq!(
            next_wait(&config, Duration::from_secs(60), baseline, None),
            Wait::Poll("the poll interval elapsed")
        );

        // A marker at or before the baseline is no signal.
        assert_eq!(
            next_wait(&config, Duration::ZERO, baseline, Some(at(0))),
            Wait::Sleep(slice)
        );
        assert_eq!(
            next_wait(&config, Duration::ZERO, baseline, Some(at(-5))),
            Wait::Sleep(slice)
        );

        // A marker past the baseline stops the wait at once, even when the
        // interval has already elapsed too: the marker reason wins.
        assert_eq!(
            next_wait(&config, Duration::ZERO, baseline, Some(at(1))),
            Wait::Poll("the local append marker moved")
        );
        assert_eq!(
            next_wait(&config, Duration::from_secs(60), baseline, Some(at(1))),
            Wait::Poll("the local append marker moved")
        );

        // The final slice never overshoots the interval.
        let uneven: Config = serde_json::from_value(serde_json::json!({
            "engines": {"claude": {"cmd": "claude"}},
            "passDoc": ".agent/skills/pass/SKILL.md",
            "pollSeconds": 10,
            "hintPollSeconds": 3,
        }))
        .expect("a valid config");
        assert_eq!(
            next_wait(&uneven, Duration::from_secs(9), baseline, None),
            Wait::Sleep(Duration::from_secs(1))
        );
    }

    #[test]
    fn the_desired_engine_must_exist_on_this_machine() {
        let single = config_for(&[("claude", "claude")]);
        let several = config_for(&[("claude", "claude"), ("codex", "codex")]);
        let chosen = |engine: Option<&str>| LoopState {
            engine: engine.map(ToOwned::to_owned),
            ..LoopState::default()
        };

        assert_eq!(
            resolve_engine(&single, &chosen(Some("claude"))),
            EngineChoice::Run("claude".to_owned())
        );
        assert_eq!(
            resolve_engine(&single, &chosen(Some("codex"))),
            EngineChoice::Unknown("codex".to_owned())
        );
        // One configured engine needs no choice to be recorded.
        assert_eq!(
            resolve_engine(&single, &chosen(None)),
            EngineChoice::Run("claude".to_owned())
        );
        assert_eq!(
            resolve_engine(&several, &chosen(None)),
            EngineChoice::Ambiguous
        );
        assert_eq!(
            resolve_engine(&several, &chosen(Some("codex"))),
            EngineChoice::Run("codex".to_owned())
        );
    }

    #[test]
    fn every_era_boundary_restarts_the_session() {
        let config = config_for(&[("claude", "claude")]);
        let session = Session {
            engine: "claude".to_owned(),
            pass_doc_hash: 7,
            created_at: at(0),
        };

        assert_eq!(
            session_action(&config, false, "claude", 7, false, Some(&session), at(1)),
            SessionAction::Create
        );
        assert_eq!(
            session_action(&config, false, "claude", 7, true, None, at(1)),
            SessionAction::Restart("the running session is not this daemon's")
        );
        assert_eq!(
            session_action(&config, false, "claude", 7, true, Some(&session), at(1)),
            SessionAction::Reuse
        );
        assert_eq!(
            session_action(&config, false, "codex", 7, true, Some(&session), at(1)),
            SessionAction::Restart("the desired engine changed")
        );
        assert_eq!(
            session_action(&config, false, "claude", 9, true, Some(&session), at(1)),
            SessionAction::Restart("the pass document changed")
        );
        assert_eq!(
            session_action(&config, true, "claude", 7, true, Some(&session), at(1)),
            SessionAction::Restart("a rotation is pending")
        );

        // The default age budget is 21600 seconds: six hours.
        assert_eq!(
            session_action(&config, false, "claude", 7, true, Some(&session), at(359)),
            SessionAction::Reuse
        );
        assert_eq!(
            session_action(&config, false, "claude", 7, true, Some(&session), at(360)),
            SessionAction::Restart("the session reached its age budget")
        );
    }

    #[test]
    fn injections_say_to_read_the_state_and_carry_provenance() {
        assert_eq!(
            injection(
                true,
                ".agent/skills/pass/SKILL.md",
                &[Trigger::Log, Trigger::Due]
            ),
            "Read .agent/skills/pass/SKILL.md, then read the current Alder state and act on it \
             (triggers: log,due)."
        );
        assert_eq!(
            injection(
                false,
                ".agent/skills/pass/SKILL.md",
                &[Trigger::Observations]
            ),
            "Read the current Alder state and act on it (triggers: observations)."
        );
        assert_eq!(
            injection(false, ".agent/skills/pass/SKILL.md", &[]),
            "Read the current Alder state and act on it (triggers: none)."
        );
    }

    #[test]
    fn notes_round_trip_and_default_to_never_having_acted() {
        let notes = Notes {
            last_head: 17,
            last_wake_at: Some(at(3)),
        };
        let bytes = serde_json::to_vec(&notes).unwrap();
        assert_eq!(serde_json::from_slice::<Notes>(&bytes).unwrap(), notes);

        // Absent fields decode as the fresh state, so a truncated or
        // hand-edited file degrades to one extra wake rather than an error.
        let sparse: Notes = serde_json::from_str("{}").unwrap();
        assert_eq!(sparse, Notes::default());
    }

    #[test]
    fn content_hashes_are_exact() {
        // FNV-1a, pinned to its published 64-bit vectors: both the basis and
        // the mixing step are fixed, so a hash that merely varies with its
        // input is not enough to pass.
        assert_eq!(content_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(content_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(content_hash(b"foobar"), 0x8594_4171_f739_67e8);
        assert_eq!(content_hash(b"one"), content_hash(b"one"));
        assert_ne!(content_hash(b"one"), content_hash(b"two"));
    }

    #[test]
    fn a_persistent_condition_is_reported_once_per_occurrence() {
        let mut notice = Notice::default();
        assert!(notice.raise("engine `gemini` is not configured"));
        for _ in 0..5 {
            assert!(!notice.raise("engine `gemini` is not configured"));
        }
        // A different condition is different news.
        assert!(notice.raise("no engine is selected"));
        assert!(!notice.raise("no engine is selected"));
        // Clearing makes the original condition news again.
        notice.clear();
        assert!(notice.raise("no engine is selected"));
        notice.clear();
        notice.clear();
        assert!(notice.raise("engine `gemini` is not configured"));
    }
}
