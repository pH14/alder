//! Every decision the driver makes, as pure functions over a snapshot.
//!
//! Nothing here talks to tmux, Git, or the Alder CLI. The daemon's judgment
//! surface is small on purpose, and keeping it here is what makes that
//! claim checkable.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, Utc};

use crate::{config::Config, loop_state::LoopState};

/// Why the driver would wake the loop. These are Alder's trigger kinds; they
/// are informational provenance, never a limit on what the pass must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    Log,
    Observations,
    Due,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Observations => "observations",
            Self::Due => "due",
        }
    }
}

/// Everything the driver observed this poll that is not already in the loop
/// fold. It remembers nothing about the log: the fold carries that.
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
    /// Wake the loop with these trigger kinds.
    Fire(Vec<Trigger>),
}

/// Which trigger kinds currently hold. Empty means nothing has happened.
pub fn triggers(config: &Config, state: &LoopState, poll: &Poll) -> Vec<Trigger> {
    let mut triggers = Vec::new();
    if log_advanced(state) {
        triggers.push(Trigger::Log);
    }
    if poll.refresh_changed {
        triggers.push(Trigger::Observations);
    }
    if wake_due(state, poll.now) || max_interval_elapsed(config, state, poll.now) {
        triggers.push(Trigger::Due);
    }
    triggers
}

/// Something was appended since the last pass ended.
///
/// The baseline lives in the log, not in the daemon, so a restarted driver
/// recovers it for free. The driver's own wake and end events cannot
/// self-trigger: immediately after a pass ends, the head *is* its `ended_seq`.
/// Before the first pass there is no baseline, and `max_interval_elapsed`
/// already fires a fresh project immediately.
fn log_advanced(state: &LoopState) -> bool {
    state
        .last_pass
        .as_ref()
        .and_then(|pass| pass.ended_seq)
        .is_some_and(|ended_seq| state.head > ended_seq)
}

/// A pass asked to be woken again at a specific time and that time has come.
fn wake_due(state: &LoopState, now: DateTime<Utc>) -> bool {
    state
        .last_pass
        .as_ref()
        .and_then(|pass| pass.wake_at)
        .is_some_and(|wake_at| now >= wake_at)
}

/// The loop has not run for longer than the configured ceiling. A log with no
/// pass at all has waited forever, so a fresh project fires immediately.
fn max_interval_elapsed(config: &Config, state: &LoopState, now: DateTime<Utc>) -> bool {
    let ceiling = TimeDelta::seconds(config.max_interval_seconds as i64);
    match state.last_pass.as_ref().and_then(|pass| pass.ended_at) {
        Some(ended_at) => now >= ended_at + ceiling,
        None => true,
    }
}

/// The one decision the driver makes each poll.
pub fn decide(config: &Config, state: &LoopState, poll: &Poll) -> Decision {
    if state.paused {
        return Decision::Idle("the loop is paused");
    }
    if state.open_pass.is_some() {
        return Decision::Idle("a pass is already open");
    }
    let triggers = triggers(config, state, poll);
    if triggers.is_empty() {
        return Decision::Idle("nothing changed");
    }
    // The ceiling overrides both deferrals: a loop that never runs is worse
    // than an injection landing under someone's cursor.
    let overdue = max_interval_elapsed(config, state, poll.now);
    if poll.attached_client && !overdue {
        return Decision::Hold("a client is attached to the session");
    }
    let settled = poll
        .pending_since
        .is_none_or(|since| poll.now >= since + TimeDelta::seconds(config.debounce_seconds as i64));
    if !settled && !overdue {
        return Decision::Hold("debouncing");
    }
    Decision::Fire(triggers)
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
    pub passes: u32,
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
    state: &LoopState,
    engine: &str,
    pass_doc_hash: u64,
    exists: bool,
    known: Option<&Session>,
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
    if state.rotate_pending {
        return SessionAction::Restart("a rotation is pending");
    }
    if session.pass_doc_hash != pass_doc_hash {
        return SessionAction::Restart("the pass document changed");
    }
    if session.passes >= config.max_passes_per_session {
        return SessionAction::Restart("the session reached its pass budget");
    }
    SessionAction::Reuse
}

/// The message injected into the leader's terminal.
///
/// Trigger kinds ride along as provenance. The pass runs its complete sync
/// regardless, so the message never says "only look at X".
pub fn injection(bootstrap: bool, pass_doc: &str, pass_id: &str, triggers: &[Trigger]) -> String {
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
        format!("Read {pass_doc}, then run one pass (pass-id: {pass_id}; triggers: {kinds}).")
    } else {
        format!("Run one pass (pass-id: {pass_id}; triggers: {kinds}).")
    }
}

/// Whether an open pass has outlived the configured ceiling.
pub fn pass_timed_out(config: &Config, started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= started_at + TimeDelta::seconds(config.pass_timeout_seconds as i64)
}

/// The tmux session a pass's handle names, if the driver can observe it.
///
/// A pass records the handle of the session that runs it. That handle need not
/// be this driver's session, and need not be tmux at all: another writer may
/// have woken the loop with `codex:019f…`. `crashed` is a statement about an
/// observed dead session, so the driver may only make it for a tmux handle it
/// can actually check. Everything else is unobservable, and the only honest
/// verdict left is `timeout`.
pub fn observable_session(handle: &str) -> Option<&str> {
    handle
        .split_once(':')
        .filter(|(kind, value)| *kind == "tmux" && !value.is_empty())
        .map(|(_, value)| value)
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
        "passDoc": ".alder/PASS.md",
    }))
    .expect("a generated config is valid")
}

#[cfg(test)]
mod tests {
    use crate::loop_state::{LastPass, OpenPass};

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

    /// A loop whose one pass ended `minutes` in, at head 40, with nothing
    /// appended since.
    fn ran(minutes: i64) -> LoopState {
        LoopState {
            head: 40,
            last_pass: Some(LastPass {
                id: "hm-pass-1".to_owned(),
                outcome: Some("ok".to_owned()),
                wake_at: None,
                ended_at: Some(at(minutes)),
                ended_seq: Some(40),
            }),
            ..LoopState::default()
        }
    }

    #[test]
    fn each_trigger_kind_has_exactly_one_cause() {
        let config = config_for(&[("claude", "claude")]);

        // The head still stands where the last pass left it.
        assert!(triggers(&config, &ran(0), &poll(1)).is_empty());

        let mut moved = ran(0);
        moved.head = 41;
        assert_eq!(triggers(&config, &moved, &poll(1)), vec![Trigger::Log]);

        // Before any pass there is no baseline, so the log trigger stays
        // silent and the ceiling is what fires a fresh project.
        let mut fresh = LoopState {
            head: 12,
            ..LoopState::default()
        };
        assert_eq!(triggers(&config, &fresh, &poll(0)), vec![Trigger::Due]);
        // An unended last pass leaves no baseline either.
        fresh.last_pass = Some(LastPass {
            id: "hm-pass-1".to_owned(),
            outcome: None,
            wake_at: None,
            ended_at: Some(at(0)),
            ended_seq: None,
        });
        assert!(!triggers(&config, &fresh, &poll(1)).contains(&Trigger::Log));

        let mut observed = poll(1);
        observed.refresh_changed = true;
        assert_eq!(
            triggers(&config, &ran(0), &observed),
            vec![Trigger::Observations]
        );

        let mut due = ran(0);
        due.last_pass.as_mut().unwrap().wake_at = Some(at(20));
        assert!(triggers(&config, &due, &poll(19)).is_empty());
        assert_eq!(triggers(&config, &due, &poll(20)), vec![Trigger::Due]);

        // The ceiling is 1800 seconds by default.
        assert_eq!(triggers(&config, &ran(0), &poll(30)), vec![Trigger::Due]);
        assert!(triggers(&config, &ran(0), &poll(29)).is_empty());

        // A log with no pass at all has waited forever.
        assert_eq!(
            triggers(&config, &LoopState::default(), &poll(0)),
            vec![Trigger::Due]
        );

        let mut all = ran(0);
        all.head = 41;
        let mut everything = poll(30);
        everything.refresh_changed = true;
        assert_eq!(
            triggers(&config, &all, &everything),
            vec![Trigger::Log, Trigger::Observations, Trigger::Due]
        );
        assert_eq!(Trigger::Log.as_str(), "log");
        assert_eq!(Trigger::Observations.as_str(), "observations");
        assert_eq!(Trigger::Due.as_str(), "due");
    }

    #[test]
    fn pause_and_an_open_pass_outrank_every_trigger() {
        let config = config_for(&[("claude", "claude")]);
        let mut paused = ran(0);
        paused.paused = true;
        assert_eq!(
            decide(&config, &paused, &poll(30)),
            Decision::Idle("the loop is paused")
        );

        let mut open = ran(0);
        open.open_pass = Some(OpenPass {
            id: "hm-pass-2".to_owned(),
            engine: "claude".to_owned(),
            handle: "tmux:alder-leader".to_owned(),
            started_at: at(1),
        });
        assert_eq!(
            decide(&config, &open, &poll(30)),
            Decision::Idle("a pass is already open")
        );

        assert_eq!(
            decide(&config, &ran(0), &poll(1)),
            Decision::Idle("nothing changed")
        );
    }

    #[test]
    fn debounce_and_attachment_hold_the_injection_but_not_past_the_ceiling() {
        let config = config_for(&[("claude", "claude")]);
        // Another writer appended past the last pass's end.
        let mut moved = ran(0);
        moved.head = 41;
        let mut fresh = poll(1);
        fresh.pending_since = Some(at(1));
        assert_eq!(
            decide(&config, &moved, &fresh),
            Decision::Hold("debouncing")
        );

        // The default debounce is 20 seconds, so the next poll clears it.
        let mut settled = fresh.clone();
        settled.now = at(2);
        assert_eq!(
            decide(&config, &moved, &settled),
            Decision::Fire(vec![Trigger::Log])
        );

        let mut attached = settled.clone();
        attached.attached_client = true;
        assert_eq!(
            decide(&config, &moved, &attached),
            Decision::Hold("a client is attached to the session")
        );

        // Past the ceiling both deferrals give way.
        let mut overdue = attached.clone();
        overdue.now = at(31);
        overdue.pending_since = Some(at(31));
        assert_eq!(
            decide(&config, &moved, &overdue),
            Decision::Fire(vec![Trigger::Log, Trigger::Due])
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
        let state = LoopState::default();
        let session = Session {
            engine: "claude".to_owned(),
            pass_doc_hash: 7,
            passes: 3,
        };

        assert_eq!(
            session_action(&config, &state, "claude", 7, false, Some(&session)),
            SessionAction::Create
        );
        assert_eq!(
            session_action(&config, &state, "claude", 7, true, None),
            SessionAction::Restart("the running session is not this daemon's")
        );
        assert_eq!(
            session_action(&config, &state, "claude", 7, true, Some(&session)),
            SessionAction::Reuse
        );
        assert_eq!(
            session_action(&config, &state, "codex", 7, true, Some(&session)),
            SessionAction::Restart("the desired engine changed")
        );
        assert_eq!(
            session_action(&config, &state, "claude", 9, true, Some(&session)),
            SessionAction::Restart("the pass document changed")
        );

        let mut rotating = state.clone();
        rotating.rotate_pending = true;
        assert_eq!(
            session_action(&config, &rotating, "claude", 7, true, Some(&session)),
            SessionAction::Restart("a rotation is pending")
        );

        let spent = Session {
            passes: config.max_passes_per_session,
            ..session
        };
        assert_eq!(
            session_action(&config, &state, "claude", 7, true, Some(&spent)),
            SessionAction::Restart("the session reached its pass budget")
        );
    }

    #[test]
    fn injections_state_the_pass_and_its_provenance() {
        assert_eq!(
            injection(
                true,
                ".alder/PASS.md",
                "hm-pass-3",
                &[Trigger::Log, Trigger::Due]
            ),
            "Read .alder/PASS.md, then run one pass (pass-id: hm-pass-3; triggers: log,due)."
        );
        assert_eq!(
            injection(
                false,
                ".alder/PASS.md",
                "hm-pass-4",
                &[Trigger::Observations]
            ),
            "Run one pass (pass-id: hm-pass-4; triggers: observations)."
        );
        assert_eq!(
            injection(false, ".alder/PASS.md", "hm-pass-5", &[]),
            "Run one pass (pass-id: hm-pass-5; triggers: none)."
        );
    }

    #[test]
    fn pass_timeouts_and_content_hashes_are_exact() {
        let config = config_for(&[("claude", "claude")]);
        // The default pass timeout is 3600 seconds.
        assert!(!pass_timed_out(&config, at(0), at(59)));
        assert!(pass_timed_out(&config, at(0), at(60)));

        assert_eq!(content_hash(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(content_hash(b"one"), content_hash(b"one"));
        assert_ne!(content_hash(b"one"), content_hash(b"two"));
    }

    #[test]
    fn only_a_tmux_handle_names_a_session_the_driver_can_observe() {
        assert_eq!(
            observable_session("tmux:alder-leader"),
            Some("alder-leader")
        );
        // Another writer's session is still observable if it is tmux.
        assert_eq!(observable_session("tmux:other-box"), Some("other-box"));
        // A value may contain colons; only the kind is parsed.
        assert_eq!(
            observable_session("tmux:box-17/alder-hm-9a1"),
            Some("box-17/alder-hm-9a1")
        );
        // Nothing else can be checked, so nothing else may be called crashed.
        for opaque in [
            "codex:019f",
            "github-actions:owner/repo/run/1",
            "tmux:",
            "tmux",
            "",
        ] {
            assert_eq!(observable_session(opaque), None, "{opaque}");
        }
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
