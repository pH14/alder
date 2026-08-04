//! Deterministic crash-anywhere convergence tests for the real alderd paths.
//!
//! A crash lands an arbitrary subset of the interrupted effect's declared
//! footprint — see `simulator/mod.rs` for why that is the model, and for the
//! atomicity asymmetry that makes a log append the one thing which cannot
//! tear.
//!
//! A failure prints the complete `Case` (seed, operation sequence, and fault
//! schedule, each fault carrying the subset it tore). Re-running that value is
//! byte-for-byte deterministic. Proptest's ordinary `Vec` shrinker removes
//! operations and crash points, so the saved regression is the minimal fault
//! schedule that still fails.

mod simulator;

use alderd::{decide::Decision, driver::Driver, loop_state::LoopState};
use proptest::prelude::*;
use simulator::{
    AgentScript, Boundary, Case, Fault, Operation, Simulator, assert_case_converges,
    catch_sim_crash, config, execute_case,
};

fn position_of(trace: &[Boundary], label: &str) -> usize {
    trace
        .iter()
        .position(|boundary| boundary.label == label)
        .map(|index| index + 1)
        .unwrap_or_else(|| panic!("{label} is absent from {trace:#?}"))
}

#[test]
fn the_memory_log_enforces_real_compare_and_append() {
    assert!(Simulator::new(1).stale_cas_is_rejected());
}

/// The enumeration over a complete daemon poll: the status read, the command
/// run, the notes write, and every clock tick between them, each torn every
/// way its footprint allows.
///
/// This is the crash half of the invariant: nothing durable records a wake,
/// so a crash anywhere in the wake path costs at most one missed or one
/// duplicated command run, and recovery converges without any repair
/// verdicts.
#[test]
fn every_torn_subset_of_the_wake_lifecycle_converges() {
    let probe = Simulator::new(3);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    let lifecycle = probe.trace();
    for label in ["daemon.status", "wake.command", "notes.write"] {
        position_of(&lifecycle, label);
    }

    for boundary in &lifecycle {
        for mask in 0..boundary.subsets() {
            let host = Simulator::new(3);
            host.schedule_faults(vec![Fault::torn(boundary.ordinal, mask)]);
            let mut driver = Driver::new(host.clone(), config());
            assert!(
                catch_sim_crash(|| driver.poll_once()).is_none(),
                "lifecycle fault {} landing {:?} did not fire; trace={:#?}",
                boundary.ordinal,
                boundary.landed(mask),
                host.trace()
            );
            host.recover();
            host.assert_invariant();
        }
    }
}

/// A crash between the command run and the notes write is the duplicate-run
/// window: the executor was handed the wake, but the restarted daemon does
/// not know that and runs it again. Pinned so the interesting subset does not
/// depend on enumeration order, and asserted to actually produce the second
/// run — which the invariant then holds harmless.
#[test]
fn a_crash_between_command_and_notes_runs_the_wake_twice_harmlessly() {
    let probe = Simulator::new(29);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    let trace = probe.trace();
    let command = position_of(&trace, "wake.command");
    let notes = position_of(&trace, "notes.write");
    assert!(command < notes, "the command must precede the notes write");

    let host = Simulator::new(29);
    // The command runs whole; the process dies before the notes write.
    host.schedule_faults(vec![Fault::whole(command)]);
    let mut driver = Driver::new(host.clone(), config());
    assert!(catch_sim_crash(|| driver.poll_once()).is_none());
    assert_eq!(host.commands_run(), 1);

    host.recover();
    host.assert_invariant();
    assert!(
        host.commands_run() >= 2,
        "the restarted daemon never re-ran the unnoted wake"
    );
    // Every run carried its trigger provenance, never an empty word.
    assert!(
        host.triggers_seen()
            .iter()
            .all(|triggers| !triggers.is_empty() && triggers != "none"),
        "{:?}",
        host.triggers_seen()
    );
}

/// A command that fails is treated like a torn delivery: nothing is noted,
/// and the next poll runs the same wake again rather than silencing it.
#[test]
fn a_failing_command_is_not_noted_and_is_rerun_when_it_recovers() {
    let host = Simulator::new(31);
    host.fail_commands(true);
    let mut driver = Driver::new(host.clone(), config());
    assert!(driver.poll_once().is_err());
    assert_eq!(host.commands_run(), 0);
    assert!(
        !matches!(host.decision(), Decision::Idle(_)),
        "a failed command must leave the wake owed"
    );

    host.fail_commands(false);
    driver.poll_once().unwrap();
    assert_eq!(host.commands_run(), 1);
    host.assert_invariant();
}

/// The atomicity asymmetry, checked against the effects the daemon actually
/// performs rather than only against the constructors that enforce it: the
/// daemon poll path contains no append at all. The daemon appends nothing —
/// the appends in a run's history all belong to the harness's other writers,
/// which is exactly the real division.
#[test]
fn the_daemon_poll_path_never_appends_to_the_log() {
    let host = Simulator::new(6);
    host.script_executor(AgentScript::Append);
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    let trace = host.trace();
    let mut worlds = 0;
    for boundary in &trace {
        assert!(
            !boundary.footprint.contains(&"append"),
            "the daemon poll path appended to the log: {boundary:#?}"
        );
        if !boundary.footprint.is_empty() {
            worlds += 1;
        }
    }
    assert!(worlds >= 2, "only {worlds} world effects were exercised");
    // The scripted executor really did append — through the harness, as its
    // own process — so the claim above was tested against a run that moved
    // the head.
    assert!(host.snapshot().head.sequence() >= 2);
}

#[test]
fn the_harness_contains_no_wall_clock_or_real_sleep() {
    let source = include_str!("simulator/mod.rs");
    for forbidden in [
        "thread::sleep",
        "std::thread::sleep",
        "Utc::now",
        "SystemTime",
        "Instant::now",
    ] {
        assert!(
            !source.contains(forbidden),
            "the logical-clock harness contains `{forbidden}`"
        );
    }
}

/// The simulator serves the production status builder, then the driver's real
/// reader consumes that document. The scenario populates the fields the
/// driver's triggers actually read, so the check covers more than the empty
/// shape.
#[test]
fn the_simulated_status_serves_the_loop_section_production_builds() {
    let host = Simulator::new(19);
    // A nudge request populates the manual-trigger sequence.
    host.nudge();
    let snapshot = host.snapshot();

    let real = alder::app::status_document(&snapshot.state, &snapshot.head, false, None);
    let simulated = alderd::effects::Effects::alder(&host, &["status"]).unwrap();
    assert_eq!(
        simulated, real,
        "the simulator did not serve the CLI builder"
    );

    // And production's own reader, over both documents. Parsed states rather
    // than key sets: a field that arrives under the right name carrying the
    // wrong thing is a difference here and nowhere above.
    let from_real = LoopState::from_status(&real)
        .expect("production's own status document reads back into LoopState");
    let from_simulated =
        LoopState::from_status(&simulated).expect("the simulated status is production-readable");
    assert_eq!(
        from_simulated, from_real,
        "the loop the daemon sees under simulation is not the loop production \
         reports for the same state"
    );

    // An equality between two empty states would prove nothing, so pin the
    // fields the loop actually turns on.
    assert!(from_real.head > 0, "the status document reports head 0");
    assert!(
        from_real.nudge_requested_seq.is_some(),
        "loop.nudge_requested_seq is gone, and it is the manual trigger"
    );
}

/// A paused loop idles without running anything, whatever else is owed.
#[test]
fn a_pause_outranks_every_trigger_through_the_real_driver() {
    let host = Simulator::new(23);
    host.pause("maintenance window");
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    assert_eq!(host.commands_run(), 0);
    assert!(matches!(host.decision(), Decision::Idle(_)));
}

#[test]
fn a_failing_seed_replays_byte_for_byte() {
    let case = Case {
        seed: 0x5eed_cafe,
        operations: vec![
            Operation::PollDaemon,
            Operation::Nudge,
            Operation::RestartDaemon,
            Operation::PollDaemon,
            Operation::Tick(7),
            Operation::ExecutorAppendsOnNextWake,
            Operation::PollDaemon,
        ],
        fault_schedule: vec![Fault::torn(2, 0b1), Fault::whole(5)],
    };
    assert_eq!(execute_case(&case), execute_case(&case));
}

/// One armed script means one scripted act, whoever runs it: an executor
/// scripted to append does so on exactly one wake, and the wake after it
/// finds an ordinary idle executor.
#[test]
fn one_armed_executor_append_lands_exactly_once() {
    let host = Simulator::new(37);
    let mut driver = Driver::new(host.clone(), config());
    host.script_executor(AgentScript::Append);
    driver.poll_once().unwrap();
    let head_after_first = host.snapshot().head.sequence();

    // The executor's append moved the head, so the next poll wakes again —
    // and the script is consumed, so nothing appends this time.
    driver.poll_once().unwrap();
    assert_eq!(host.snapshot().head.sequence(), head_after_first);
    assert_eq!(host.commands_run(), 2);

    // And now the loop is quiet.
    driver.poll_once().unwrap();
    assert_eq!(host.commands_run(), 2);
    host.assert_invariant();
}

fn generated_case(seed: u64, noise: Vec<u8>, fault_slots: Vec<(u8, u8)>) -> Case {
    let mut operations = vec![Operation::PollDaemon];
    for byte in noise {
        operations.push(match byte % 5 {
            0 => Operation::RestartDaemon,
            1 => Operation::PollDaemon,
            2 => Operation::Nudge,
            3 => Operation::ExecutorAppendsOnNextWake,
            _ => Operation::Tick(byte % 11),
        });
    }
    operations.extend([Operation::Nudge, Operation::PollDaemon]);

    // Each fault is a one-based number of effects after the preceding crash,
    // plus the subset of that effect's footprint that lands before the
    // process dies. Relative distances stay reachable when an earlier crash
    // changes which path recovery takes, and Vec shrinking can remove either
    // crash.
    let faults = fault_slots
        .into_iter()
        .map(|(slot, torn)| Fault::torn(1 + usize::from(slot) % 16, u32::from(torn)))
        .collect();
    Case {
        seed,
        operations,
        fault_schedule: faults,
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        max_shrink_iters: 4096,
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn generated_restart_nudge_append_and_torn_crash_interleavings_converge(
        seed in any::<u64>(),
        noise in prop::collection::vec(any::<u8>(), 0..8),
        fault_slots in prop::collection::vec((any::<u8>(), any::<u8>()), 0..=2),
    ) {
        let case = generated_case(seed, noise, fault_slots);
        // `Tick` can leave the daemon between observations, so the generated
        // prefix itself is not a fixed point. `assert_case_converges` settles
        // each complete schedule only after its recovery loop has drained
        // that logical-time work, and asserts the invariants there before
        // returning this replay witness.
        let first = assert_case_converges(&case);
        let second = assert_case_converges(&case);
        prop_assert_eq!(
            first,
            second,
            "non-replayable case: {:#?}",
            case
        );
    }
}
