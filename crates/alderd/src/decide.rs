//! Every decision the driver makes, as pure functions over a snapshot.
//!
//! Nothing here talks to a shell or the Alder CLI. The daemon's judgment
//! surface is small on purpose, and keeping it here is what makes that
//! claim checkable.
//!
//! The wake rule is one comparison: the head has moved past the last head this
//! driver acted on. That baseline lives in the driver's machine-local
//! [`Notes`], never in the log — the log never mentions its own readers — and
//! losing it is harmless: the driver runs the command once more, the executor
//! behind the command reads the fold, finds nothing new to do, and idles.
//! Passes are idempotent; a missed or duplicated wake changes nothing durable.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::{config::Config, loop_state::LoopState};

/// Why the driver would run the command. These kinds are informational
/// provenance in `ALDERD_TRIGGERS`; they never limit what the command must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    Manual,
    Log,
    Due,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Log => "log",
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
    /// The head this driver last ran the command for.
    #[serde(default)]
    pub last_head: u64,
    /// When this driver last ran the command.
    #[serde(default)]
    pub last_wake_at: Option<DateTime<Utc>>,
}

/// Everything the driver observed this poll that is not already in the loop
/// fold or its own notes.
#[derive(Debug, Clone)]
pub struct Poll {
    pub now: DateTime<Utc>,
    /// When the fire condition first became true, for debouncing.
    pub pending_since: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do; sleep until the next poll.
    Idle(&'static str),
    /// The condition holds but the command waits.
    Hold(&'static str),
    /// Run the command, with these trigger kinds as provenance.
    Fire(Vec<Trigger>),
}

/// A nudge request is outstanding for this driver when it is later in the
/// log than the last head the driver acted on. Acting consumes it by moving
/// the noted head past it; no clearing write exists, because the log does not
/// record its readers.
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
    if review_due(state, notes, poll.now) || max_interval_elapsed(config, notes, poll.now) {
        triggers.push(Trigger::Due);
    }
    triggers
}

/// A deferral's deadline has arrived and this driver has not run the command
/// since it passed. One wake per deadline: an executor that reviews the item
/// moves the deadline or removes it, and an executor that does not is caught
/// by the max-interval ceiling rather than woken every poll. Every blocked
/// item's deadline is checked, not just the earliest: an item that stays
/// blocked past its own deadline must not swallow the wake a later deadline
/// is owed.
fn review_due(state: &LoopState, notes: &Notes, now: DateTime<Utc>) -> bool {
    state
        .review_deadlines
        .iter()
        .any(|&deadline| now >= deadline && notes.last_wake_at.is_none_or(|woke| woke < deadline))
}

/// The command has not run for longer than the configured ceiling. A driver
/// with no notes has never run it, so a fresh start fires immediately.
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
    // The ceiling overrides the debounce: a loop that never runs is worse
    // than a redundant run. A pending nudge does the same, because a nudge is
    // the human overriding that politeness.
    let urgent = max_interval_elapsed(config, notes, poll.now) || nudge_pending(state, notes);
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

/// The `ALDERD_TRIGGERS` value: the trigger names, comma-joined.
pub fn trigger_names(triggers: &[Trigger]) -> String {
    if triggers.is_empty() {
        return "none".to_owned();
    }
    triggers
        .iter()
        .map(|trigger| trigger.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Build a config in code, for tests and for callers that never read a file.
pub fn config_for(command: &str) -> Config {
    serde_json::from_value(serde_json::json!({"command": command}))
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
            pending_since: None,
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
        let config = config_for("run-the-executor");

        // The head still stands where the driver left it.
        assert!(triggers(&config, &settled_state(), &acted(0), &poll(1)).is_empty());

        let mut moved = settled_state();
        moved.head = 41;
        assert_eq!(
            triggers(&config, &moved, &acted(0), &poll(1)),
            vec![Trigger::Log]
        );

        // Fresh notes have never run anything: the ceiling fires immediately,
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

        // A deferral deadline is a due trigger once, at its instant.
        let mut deferred = settled_state();
        deferred.review_deadlines = vec![at(20)];
        assert!(triggers(&config, &deferred, &acted(0), &poll(19)).is_empty());
        assert_eq!(
            triggers(&config, &deferred, &acted(0), &poll(20)),
            vec![Trigger::Due]
        );
        // A wake delivered after the deadline consumes it; the ceiling is the
        // backstop for an executor that never reviews the item.
        assert!(triggers(&config, &deferred, &acted(21), &poll(22)).is_empty());

        // A head behind the note is still a difference: a rebuilt or
        // truncated log ref wakes once, and re-noting self-heals.
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
        assert_eq!(
            triggers(&config, &all, &acted(0), &poll(30)),
            vec![Trigger::Manual, Trigger::Log, Trigger::Due]
        );
        assert_eq!(Trigger::Manual.as_str(), "manual");
        assert_eq!(Trigger::Log.as_str(), "log");
        assert_eq!(Trigger::Due.as_str(), "due");
    }

    #[test]
    fn each_deadline_earns_its_own_wake() {
        let config = config_for("run-the-executor");
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
    fn a_nudge_is_pending_only_past_the_noted_head() {
        let state = |nudge| LoopState {
            nudge_requested_seq: nudge,
            ..LoopState::default()
        };
        let notes = |last_head| Notes {
            last_head,
            last_wake_at: None,
        };
        assert!(!nudge_pending(&state(None), &notes(0)));
        assert!(nudge_pending(&state(Some(3)), &notes(0)));
        assert!(nudge_pending(&state(Some(5)), &notes(4)));
        assert!(!nudge_pending(&state(Some(4)), &notes(4)));
        assert!(!nudge_pending(&state(Some(3)), &notes(4)));
    }

    #[test]
    fn pause_outranks_every_trigger() {
        let config = config_for("run-the-executor");
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
    fn debounce_holds_the_command_but_not_past_the_ceiling() {
        let config = config_for("run-the-executor");
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

        // Past the ceiling the debounce gives way.
        let mut overdue = fresh.clone();
        overdue.now = at(31);
        overdue.pending_since = Some(at(31));
        assert_eq!(
            decide(&config, &moved, &acted(0), &overdue),
            Decision::Fire(vec![Trigger::Log, Trigger::Due])
        );
    }

    #[test]
    fn a_nudge_fires_through_the_debounce_but_respects_pause() {
        let config = config_for("run-the-executor");
        let mut nudged = settled_state();
        nudged.head = 41;
        nudged.nudge_requested_seq = Some(41);

        // Debounce has not settled; a nudge fires anyway, because it is the
        // human overriding the driver's politeness.
        let mut held = poll(1);
        held.pending_since = Some(at(1));
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

        let config = config_for("run-the-executor");
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
            "command": "run-the-executor",
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
    fn trigger_names_are_a_comma_list_and_none_when_empty() {
        assert_eq!(
            trigger_names(&[Trigger::Manual, Trigger::Log, Trigger::Due]),
            "manual,log,due"
        );
        assert_eq!(trigger_names(&[Trigger::Due]), "due");
        assert_eq!(trigger_names(&[]), "none");
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
}
