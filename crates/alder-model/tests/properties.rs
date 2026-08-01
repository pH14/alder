//! The four checked properties, each under the smallest scenario that
//! exercises it, plus two scenarios that exist to make a property
//! discriminating rather than to add one — and then three checks on the
//! harness itself, because a model checker that asks the wrong questions
//! reports the same green as one that asks the right ones. Run with
//! `--nocapture` to see the explored state counts documented in README.md.

use alder_model::Scenario;
use alderd::decide::config_for;
use stateright::{Checker, Expectation, Model};

/// Explore the complete state space and assert every property: `always`
/// properties must have no counterexample, `sometimes` properties must have
/// an example.
///
/// The state count is asserted *exactly*, and that is the third assertion
/// rather than a diagnostic. Properties catch a model that reaches a bad
/// state; nothing but the size of the space catches a model that quietly
/// stopped reaching a good one, or started reaching states its budgets say it
/// cannot — a step offered one pass too late, a fault injected past its
/// budget, a counter that stopped counting so two eras hash alike. Every count
/// here is README.md's table; the two are the same claim, and a change to
/// either wants the other changed with it, deliberately.
fn check(scenario: Scenario, name: &str, states: usize) {
    let checker = scenario.checker().spawn_bfs().join();
    checker.assert_properties();
    let explored = checker.unique_state_count();
    eprintln!("{name}: {explored} unique states");
    assert_eq!(
        explored, states,
        "{name} explored {explored} states, not {states}: the model gained or \
         lost transitions. Investigate before accepting a new number, and \
         change README.md's table with it."
    );
}

/// Baseline: one daemon, one leader, no faults. Two passes run to the budget
/// and the loop quiesces recovered.
fn lone_daemon() -> Scenario {
    Scenario::new()
}

/// A host configured for Codex rather than Claude.
fn codex_engine() -> Scenario {
    Scenario {
        config: config_for(&[("codex", "codex")]),
        max_passes: 1,
        ..Scenario::new()
    }
}

/// The daemon and a phone session race `pass.started`.
fn wake_race() -> Scenario {
    Scenario {
        phone_wake: true,
        ..Scenario::new()
    }
}

/// A rotation request under both crash injections.
fn rotation_under_crashes() -> Scenario {
    Scenario {
        leader_rotate: true,
        phone_pause: true,
        daemon_crashes: 1,
        session_crashes: 1,
        max_passes: 3,
        ..Scenario::new()
    }
}

/// A handoff submission interleaved with the daemon's appends.
fn cas_writers() -> Scenario {
    Scenario {
        phone_handoff: true,
        ..Scenario::new()
    }
}

/// A rotation request racing a phone wake.
fn rotation_race() -> Scenario {
    Scenario {
        phone_wake: true,
        phone_rotation: true,
        ..Scenario::new()
    }
}

/// Every scenario this file checks, for the two tests that are about the
/// harness rather than about one protocol claim.
fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("lone daemon", lone_daemon()),
        ("codex engine", codex_engine()),
        ("wake race", wake_race()),
        ("CAS writers", cas_writers()),
        ("rotation race", rotation_race()),
        ("rotation under crashes", rotation_under_crashes()),
    ]
}

/// Baseline: one daemon, one leader, no faults. Two passes run to the budget
/// and the loop quiesces recovered.
#[test]
fn a_lone_daemon_runs_its_passes_cleanly() {
    // The fault-free run is one linear chain: arm, snapshot, append, inject,
    // end, twice over — plus the timeout the daemon may reach for instead of
    // waiting, at each of the two open passes.
    check(lone_daemon(), "lone daemon", 19);
}

/// A wake records the engine the decision resolved, not the one this crate
/// was first written against. On a host configured for Codex every daemon
/// pass must say `codex`; a hard-coded engine passes every other scenario in
/// this file and fails only here, which is the point of running it.
#[test]
fn a_codex_configured_loop_records_codex() {
    check(codex_engine(), "codex engine", 7);
}

/// Property 1: concurrent wake attempts. The daemon and a phone session race
/// `pass.started`; at most one pass is ever open, and the loser concedes —
/// via the `pass_open` check or the CAS conflict — without ending the
/// winner's pass.
#[test]
fn concurrent_wakes_leave_at_most_one_open_pass() {
    check(wake_race(), "wake race", 152);
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
    check(rotation_under_crashes(), "rotation under crashes", 8826);
}

/// Property 3: interleaved writers lose no updates. A handoff submission
/// races the daemon's pass appends, loses its response once, and retries the
/// identical draft; the log stays a clean total order holding the update
/// exactly once.
#[test]
fn interleaved_writers_lose_no_updates() {
    check(cas_writers(), "CAS writers", 659);
}

/// A deliberate discovery, not a pass/fail property: when a rotation request
/// races a wake, the wake can consume the request without any restart having
/// happened. The checker proves the wart is reachable; README.md discusses
/// it. The safety core (one open pass, exactly-once consumption bookkeeping)
/// still holds throughout.
#[test]
fn a_racing_wake_can_swallow_a_rotation() {
    let checker = rotation_race().checker().spawn_bfs().join();
    checker.assert_properties();
    let trace = checker
        .discovery("a racing wake consumes a rotation nobody performed")
        .expect("the model must exhibit the rotation-swallowing race");
    let states = checker.unique_state_count();
    eprintln!(
        "rotation race: {states} unique states; shortest swallow: {:?}",
        trace.into_actions()
    );
    assert_eq!(states, 1393, "the rotation-race space changed");
}

/// The eight statements every scenario makes, in the order `properties`
/// builds them.
const CORE: [&str; 8] = [
    "every reachable log folds cleanly",
    "at most one pass is ever open",
    "a crashed verdict follows a real crash",
    "rotate_pending mirrors the request log",
    "an acknowledged handoff is never lost",
    "every terminal state is progressing or blocked-and-named",
    "the rotation ghost tracks the fold",
    "a daemon wake records a configured engine",
];

fn core_and(extra: &[&'static str]) -> Vec<&'static str> {
    CORE.iter().chain(extra).copied().collect()
}

fn registered(scenario: &Scenario) -> Vec<&'static str> {
    scenario
        .properties()
        .iter()
        .map(|property| property.name)
        .collect()
}

/// A property nobody registers cannot fail, so the flags that decide *which*
/// questions a scenario asks are load-bearing in a way no exploration notices:
/// drop a `sometimes` and the run goes green faster. Pin the set each
/// scenario asks for, in order, so a gate that stops registering a check
/// fails here instead of passing everywhere.
#[test]
fn each_scenario_registers_exactly_the_properties_its_flags_ask_for() {
    // With one waker there is no race, so the ordering guarantee is checkable
    // rather than merely reachable; every scenario has one or the other.
    const SINGLE_WAKER: &str = "a rotation consumed by the daemon was performed first";

    assert_eq!(registered(&lone_daemon()), core_and(&[SINGLE_WAKER]));
    assert_eq!(registered(&codex_engine()), core_and(&[SINGLE_WAKER]));
    assert_eq!(
        registered(&wake_race()),
        core_and(&[
            "a lost wake race is conceded",
            "a wake append loses the CAS race",
        ])
    );
    assert_eq!(
        registered(&cas_writers()),
        core_and(&[
            "a wake records the log trigger that woke it",
            SINGLE_WAKER,
            "a lost response is absorbed idempotently",
            "a handoff append loses the CAS race and retries",
        ])
    );
    assert_eq!(
        registered(&rotation_race()),
        core_and(&[
            "a rotation is performed and then consumed",
            "a lost wake race is conceded",
            "a wake append loses the CAS race",
            "a racing wake consumes a rotation nobody performed",
        ])
    );
    assert_eq!(
        registered(&rotation_under_crashes()),
        core_and(&[
            "a crash strands a pass nobody was told to run",
            "a stranded pass is repaired by timeout",
            "a rotation is performed and then consumed",
            SINGLE_WAKER,
            "a crashed pass is attributed in the log",
            "a daemon crash is exercised",
        ])
    );
}

/// A `sometimes` property claims the model can *reach* something, and its
/// worth is entirely in what the model had to do to get there. One that
/// already holds before the first transition claims nothing: it is witnessed
/// by the empty log, and it stays witnessed however the protocol breaks. So
/// no scenario may register one the initial state already satisfies.
#[test]
fn no_sometimes_property_is_witnessed_before_the_model_moves() {
    // The fault-free scenarios register none, which is why the guard below
    // counts across the whole set rather than per scenario.
    let mut checked = 0;
    for (name, scenario) in scenarios() {
        for property in scenario.properties() {
            if property.expectation != Expectation::Sometimes {
                continue;
            }
            for initial in scenario.init_states() {
                assert!(
                    !(property.condition)(&scenario, &initial),
                    "{name}: the `sometimes` property {:?} already holds in the \
                     initial state, so the example it finds witnesses nothing",
                    property.name
                );
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no scenario registers a `sometimes` property");
}
