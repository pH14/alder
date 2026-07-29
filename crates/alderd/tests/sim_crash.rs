//! Deterministic crash-anywhere convergence tests for the real alderd paths.
//!
//! A failure prints the complete `Case` (seed, operation sequence, and fault
//! schedule). Re-running that value is byte-for-byte deterministic. Proptest's
//! ordinary `Vec` shrinker removes operations and crash points, so the saved
//! regression is the minimal fault schedule that still fails.

mod sim_host;

use alderd::{driver::Driver, spawn, tier};
use proptest::prelude::*;
use sim_host::{AgentScript, Case, Operation, SimHost, catch_sim_crash, config, execute_case};

#[test]
fn the_memory_log_enforces_real_compare_and_append() {
    assert!(SimHost::new(1).stale_cas_is_rejected());
}

#[test]
fn killing_after_every_spawn_effect_boundary_converges() {
    let probe = SimHost::new(2);
    spawn::spawn(
        &probe,
        "al-sim",
        tier::tier("luna").unwrap(),
        Some("scripted-agent"),
    )
    .unwrap();
    let boundaries = probe.boundary_count();
    assert!(
        boundaries >= 15,
        "spawn exposed only {boundaries} boundaries"
    );

    for crash_after in 1..=boundaries {
        let host = SimHost::new(2);
        host.reset_boundaries(vec![crash_after]);
        let result = catch_sim_crash(|| {
            spawn::spawn(
                &host,
                "al-sim",
                tier::tier("luna").unwrap(),
                Some("scripted-agent"),
            )
        });
        assert!(
            result.is_none(),
            "fault {crash_after}/{boundaries} did not kill spawn; trace={:?}",
            host.trace()
        );
        host.recover(true);
        host.assert_invariant(true);
    }
}

#[test]
fn wake_inject_and_pass_end_crashes_each_converge() {
    let probe = SimHost::new(3);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    let trace = probe.trace();
    let lifecycle: Vec<_> = ["pass.wake", "pass.inject", "pass.end"]
        .into_iter()
        .map(|name| {
            trace
                .iter()
                .position(|entry| entry.ends_with(name))
                .map(|index| index + 1)
                .unwrap_or_else(|| panic!("{name} is absent from {trace:#?}"))
        })
        .collect();

    for crash_after in lifecycle {
        let host = SimHost::new(3);
        host.reset_boundaries(vec![crash_after]);
        let mut driver = Driver::new(host.clone(), config());
        assert!(
            catch_sim_crash(|| driver.poll_once()).is_none(),
            "lifecycle fault {crash_after} did not fire; trace={:?}",
            host.trace()
        );
        host.recover(false);
        host.assert_invariant(false);
    }
}

#[test]
fn the_harness_contains_no_wall_clock_or_real_sleep() {
    let source = include_str!("sim_host/mod.rs");
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

#[test]
fn a_failing_seed_replays_byte_for_byte() {
    let case = Case {
        seed: 0x5eed_cafe,
        operations: vec![
            Operation::SpawnWorker,
            Operation::RestartDaemon,
            Operation::PollDaemon,
            Operation::Tick(7),
            Operation::LeaderDiesMidPass,
            Operation::RestartDaemon,
        ],
        fault_schedule: vec![5, 19],
    };
    assert_eq!(execute_case(&case), execute_case(&case));
}

#[test]
fn a_daemon_restart_interleaves_with_an_interrupted_spawn() {
    let probe = SimHost::new(9);
    spawn::spawn(
        &probe,
        "al-sim",
        tier::tier("luna").unwrap(),
        Some("scripted-agent"),
    )
    .unwrap();
    let crash_after_start = probe
        .trace()
        .iter()
        .position(|entry| entry.ends_with("spawn.work-start"))
        .expect("spawn records its attempt before launching")
        + 1;
    let case = Case {
        seed: 9,
        operations: vec![
            Operation::SpawnWorker,
            Operation::RestartDaemon,
            Operation::PollDaemon,
        ],
        fault_schedule: vec![crash_after_start],
    };
    let digest = execute_case(&case);
    assert!(
        digest
            .sessions
            .iter()
            .any(|session| session.starts_with("alder-work-al-sim:Worker")),
        "the restarted daemon did not converge the interrupted spawn: {digest:#?}"
    );
}

fn generated_case(seed: u64, noise: Vec<u8>, fault_slots: Vec<u8>) -> Case {
    let mut operations = vec![
        Operation::SpawnWorker,
        Operation::RestartDaemon,
        Operation::PollDaemon,
    ];
    for byte in noise {
        operations.push(match byte % 3 {
            0 => Operation::RestartDaemon,
            1 => Operation::PollDaemon,
            _ => Operation::Tick(byte % 11),
        });
    }
    operations.extend([
        Operation::LeaderDiesMidPass,
        Operation::RestartDaemon,
        Operation::PollDaemon,
    ]);

    // Each value is a one-based number of effects after the preceding crash.
    // Relative distances stay reachable when an earlier crash changes which
    // path recovery takes, and Vec shrinking can remove either crash.
    let faults = fault_slots
        .into_iter()
        .map(|slot| 1 + usize::from(slot) % 32)
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
    fn generated_restart_spawn_death_and_double_crash_interleavings_converge(
        seed in any::<u64>(),
        noise in prop::collection::vec(any::<u8>(), 0..8),
        fault_slots in prop::collection::vec(any::<u8>(), 0..=2),
    ) {
        let case = generated_case(seed, noise, fault_slots);
        let first = execute_case(&case);
        let second = execute_case(&case);
        prop_assert_eq!(
            first,
            second,
            "non-replayable case: {:#?}",
            case
        );
    }
}

#[test]
fn a_leader_stub_can_die_mid_pass_without_stranding_it() {
    let host = SimHost::new(4);
    host.set_next_agent(AgentScript::DieMidPass);
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    host.recover(false);
    host.assert_invariant(false);
}
