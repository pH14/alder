//! The four checked properties, each under the smallest scenario that
//! exercises it. Run with `--nocapture` to see the explored state counts
//! documented in README.md.

use alder_model::Scenario;
use stateright::{Checker, Model};

/// Explore the complete state space and assert every property: `always`
/// properties must have no counterexample, `sometimes` properties must have
/// an example.
fn check(scenario: Scenario, name: &str) -> usize {
    let checker = scenario.checker().spawn_bfs().join();
    checker.assert_properties();
    let states = checker.unique_state_count();
    eprintln!("{name}: {states} unique states");
    states
}

/// Baseline: one daemon, one leader, no faults. Two passes run to the budget
/// and the loop quiesces recovered.
#[test]
fn a_lone_daemon_runs_its_passes_cleanly() {
    // The fault-free run is one linear chain: arm, snapshot, append, end,
    // twice over — nine states, no branching. Growth here means the model
    // gained transitions; investigate before accepting.
    let states = check(Scenario::new(), "lone daemon");
    assert!(states >= 9, "the baseline must explore both passes");
}

/// Property 1: concurrent wake attempts. The daemon and a phone session race
/// `pass.started`; at most one pass is ever open, and the loser concedes —
/// via the `pass_open` check or the CAS conflict — without ending the
/// winner's pass.
#[test]
fn concurrent_wakes_leave_at_most_one_open_pass() {
    let scenario = Scenario {
        phone_wake: true,
        ..Scenario::new()
    };
    check(scenario, "wake race");
}

/// Property 2 (and 4): rotation requests survive crashes. A pass asks for
/// rotation; the daemon, the session, or both crash at every point; the
/// rotation is consumed exactly once, always after a restart performed it,
/// and every crash path ends progressing or blocked-and-named.
#[test]
fn crashes_never_silently_consume_a_rotation() {
    let scenario = Scenario {
        leader_rotate: true,
        phone_pause: true,
        daemon_crashes: 1,
        session_crashes: 1,
        max_passes: 3,
        ..Scenario::new()
    };
    check(scenario, "rotation under crashes");
}

/// Property 3: interleaved writers lose no updates. A handoff submission
/// races the daemon's pass appends, loses its response once, and retries the
/// identical draft; the log stays a clean total order holding the update
/// exactly once.
#[test]
fn interleaved_writers_lose_no_updates() {
    let scenario = Scenario {
        phone_handoff: true,
        ..Scenario::new()
    };
    check(scenario, "CAS writers");
}

/// A deliberate discovery, not a pass/fail property: when a rotation request
/// races a wake, the wake can consume the request without any restart having
/// happened. The checker proves the wart is reachable; README.md discusses
/// it. The safety core (one open pass, exactly-once consumption bookkeeping)
/// still holds throughout.
#[test]
fn a_racing_wake_can_swallow_a_rotation() {
    let scenario = Scenario {
        phone_wake: true,
        phone_rotation: true,
        ..Scenario::new()
    };
    let checker = scenario.checker().spawn_bfs().join();
    checker.assert_properties();
    let trace = checker
        .discovery("a racing wake consumes a rotation nobody performed")
        .expect("the model must exhibit the rotation-swallowing race");
    eprintln!(
        "rotation race: {} unique states; shortest swallow: {:?}",
        checker.unique_state_count(),
        trace.into_actions()
    );
}
