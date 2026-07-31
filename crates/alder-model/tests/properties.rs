//! The four checked properties, each under the smallest scenario that
//! exercises it, plus two scenarios that exist to make a property
//! discriminating rather than to add one. Run with `--nocapture` to see the
//! explored state counts documented in README.md.

use alder_model::Scenario;
use alderd::decide::config_for;
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
    // The fault-free run is one linear chain: arm, snapshot, append, inject,
    // end, twice over — plus the timeout the daemon may reach for instead of
    // waiting, at each of the two open passes. Growth beyond that means the
    // model gained transitions; investigate before accepting.
    let states = check(Scenario::new(), "lone daemon");
    assert!(states >= 19, "the baseline must explore both passes");
}

/// A wake records the engine the decision resolved, not the one this crate
/// was first written against. On a host configured for Codex every daemon
/// pass must say `codex`; a hard-coded engine passes every other scenario in
/// this file and fails only here, which is the point of running it.
#[test]
fn a_codex_configured_loop_records_codex() {
    let scenario = Scenario {
        config: config_for(&[("codex", "codex")]),
        max_passes: 1,
        ..Scenario::new()
    };
    check(scenario, "codex engine");
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
///
/// "Every point" includes the window between the durable wake and the
/// injection, where a daemon crash strands a pass the log shows open and no
/// engine was ever told to run. Two `sometimes` properties hold that window
/// open: the crash must actually reach it, and `timeout` must actually
/// repair it. Otherwise the liveness claim above is green for the cheap
/// reason that nothing ever stranded a pass.
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
