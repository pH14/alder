//! The checked properties, each under the smallest scenario that exercises
//! it — and then three checks on the harness itself, because a model checker
//! that asks the wrong questions reports the same green as one that asks the
//! right ones. Run with `--nocapture` to see the explored state counts
//! documented in README.md.

use alder_model::Scenario;
use stateright::{Checker, Expectation, Model};

/// Stop exploration well past the largest known-correct graph so a mutation
/// that makes the graph unbounded gets a verdict instead of a hang.
const EXPLORATION_STATE_LIMIT: usize = 200_000;

/// Explore at most the known finite graph, rejecting a capped run before its
/// properties or counts can be mistaken for results from a complete search.
fn explore(scenario: Scenario, name: &str) -> impl Checker<Scenario> {
    let checker = scenario
        .checker()
        .target_state_count(EXPLORATION_STATE_LIMIT)
        .spawn_bfs()
        .join();
    let generated = checker.state_count();
    assert!(
        generated < EXPLORATION_STATE_LIMIT,
        "{name} reached the {EXPLORATION_STATE_LIMIT}-state exploration limit; \
         model checking stopped before its work queue drained"
    );
    checker
}

/// Explore the complete state space and assert every property: `always`
/// properties must have no counterexample, `sometimes` properties must have
/// an example.
///
/// The state count is asserted *exactly*, and that is the third assertion
/// rather than a diagnostic. Properties catch a model that reaches a bad
/// state; nothing but the size of the space catches a model that quietly
/// stopped reaching a good one, or started reaching states its budgets say it
/// cannot. Every count here is README.md's table; the two are the same claim,
/// and a change to either wants the other changed with it, deliberately.
fn check(scenario: Scenario, name: &str, states: usize) {
    let checker = explore(scenario, name);
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

/// Baseline: one daemon, one executor, no faults. The fresh project fires
/// once, the executor behind the command may append one statement, the
/// follow-up run finds nothing, and the loop quiesces recovered.
fn lone_daemon() -> Scenario {
    Scenario::new()
}

/// A second writer appends statements, pauses, and requests rotation while
/// the daemon polls.
fn phone_writer() -> Scenario {
    Scenario {
        phone_rotation: true,
        phone_pause: true,
        phone_work: true,
        ..Scenario::new()
    }
}

/// A rotation request under daemon and notes-file faults: the duplicate-run
/// windows, with a second writer's request in flight.
fn faults_everywhere() -> Scenario {
    Scenario {
        phone_rotation: true,
        daemon_crashes: 1,
        notes_losses: 1,
        ..Scenario::new()
    }
}

/// Every scenario this file checks, for the tests that are about the harness
/// rather than about one protocol claim.
fn scenarios() -> Vec<(&'static str, Scenario)> {
    vec![
        ("lone daemon", lone_daemon()),
        ("phone writer", phone_writer()),
        ("faults everywhere", faults_everywhere()),
    ]
}

#[test]
fn a_lone_daemon_wakes_once_per_change_and_quiesces() {
    check(lone_daemon(), "lone daemon", 11);
}

/// Property 1: a second writer's appends are the wake rule. Every phone
/// statement moves the head past the daemon's notes, every run is consumed
/// by noting the head, and the log never mentions either process.
#[test]
fn a_second_writer_only_ever_moves_the_head() {
    check(phone_writer(), "phone writer", 880);
}

/// Property 2: missed and duplicated runs are harmless. The daemon and the
/// notes file each fail at every point; a delivered wake can be stranded
/// unnoted and the same head can be run twice; every `always` property — the
/// log folds, mentions no readers, mirrors the rotation request — holds
/// through all of it, and every terminal state is recovered.
#[test]
fn crashes_cost_duplicate_runs_and_nothing_else() {
    check(faults_everywhere(), "faults everywhere", 627);
}

/// The statements every scenario makes, in the order `properties` builds
/// them.
const CORE: [&str; 4] = [
    "every reachable log folds cleanly",
    "the log never mentions its own readers",
    "the rotation request mirrors the log",
    "every terminal state is progressing or blocked-and-named",
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
/// questions a scenario asks are load-bearing in a way no exploration
/// notices: drop a `sometimes` and the run goes green faster. Pin the set
/// each scenario asks for, in order, so a gate that stops registering a check
/// fails here instead of passing everywhere.
#[test]
fn each_scenario_registers_exactly_the_properties_its_flags_ask_for() {
    assert_eq!(registered(&lone_daemon()), core_and(&[]));
    assert_eq!(
        registered(&phone_writer()),
        core_and(&["a rotation request is served to a woken command"])
    );
    assert_eq!(
        registered(&faults_everywhere()),
        core_and(&[
            "a crash strands a delivered wake nothing recorded",
            "the command runs twice for the same head",
            "a daemon crash is exercised",
            "lost notes cost a duplicate run and nothing else",
            "a rotation request is served to a woken command",
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
