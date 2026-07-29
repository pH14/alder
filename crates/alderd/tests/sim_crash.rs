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

use alderd::{driver::Driver, spawn, tier};
use proptest::prelude::*;
use simulator::{
    AgentScript, Boundary, Case, Fault, MIRRORED, MUTATION_ENVELOPE, Operation, Simulator, Site,
    catch_sim_crash, config, execute_case,
};

/// The real CLI, as source, for the drift tripwires below.
const CLI_SOURCE: &str = include_str!("../../../src/app.rs");

fn spawn_probe(seed: u64) -> Simulator {
    let probe = Simulator::new(seed);
    spawn::spawn(
        &probe,
        "al-sim",
        tier::tier("luna").unwrap(),
        Some("scripted-agent"),
    )
    .unwrap();
    probe
}

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

/// The convergence property, at full strength: for every effect a spawn
/// performs, and for every subset of that effect's declared footprint, the
/// process dies having landed exactly that subset and recovery still reaches
/// the fixpoint.
///
/// The subsets are enumerated, not sampled, and none is excluded: a footprint
/// is a superset of what a real interrupted command can leave, so covering all
/// of it needs no argument about which torn states git can actually produce.
#[test]
fn every_torn_subset_of_every_spawn_effect_converges() {
    let probe = spawn_probe(2);
    let boundaries = probe.trace();
    assert!(
        boundaries.len() >= 15,
        "spawn exposed only {} boundaries",
        boundaries.len()
    );
    let torn: usize = boundaries
        .iter()
        .filter(|boundary| boundary.subsets() > 2)
        .count();
    assert!(
        torn >= 2,
        "no spawn effect has a footprint worth tearing: {boundaries:#?}"
    );

    for boundary in &boundaries {
        for mask in 0..boundary.subsets() {
            let host = Simulator::new(2);
            host.schedule_faults(vec![Fault::torn(boundary.ordinal, mask)]);
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
                "fault {}/{} landing {:?} did not kill spawn; trace={:#?}",
                boundary.ordinal,
                boundaries.len(),
                boundary.landed(mask),
                host.trace()
            );
            host.recover(true);
            host.assert_invariant(true);
        }
    }
}

/// The same enumeration over a complete daemon poll: the wake, the injection,
/// the pass ending, and every read and clock tick between them, each torn
/// every way its footprint allows.
#[test]
fn every_torn_subset_of_the_pass_lifecycle_converges() {
    let probe = Simulator::new(3);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    let lifecycle = probe.trace();
    for label in ["pass.wake", "pass.inject", "pass.end"] {
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
            host.recover(false);
            host.assert_invariant(false);
        }
    }
}

/// The atomicity asymmetry, checked against the effects the system actually
/// performs rather than only against the constructors that enforce it.
///
/// Every boundary that appends offers exactly two subsets — nothing, or the
/// whole record — and no boundary mixes an append with world state, so no
/// crash schedule can express "half a record landed".
#[test]
fn a_log_append_tears_to_nothing_or_everything() {
    let mut appended = 0;
    let mut worlds = 0;
    let mut check = |boundary: &Boundary| {
        if boundary.footprint.contains(&"append") {
            appended += 1;
            assert_eq!(
                boundary.footprint,
                vec!["append"],
                "an append shares its boundary with world state: {boundary:#?}"
            );
            assert_eq!(
                boundary.subsets(),
                2,
                "an append offers a torn subset: {boundary:#?}"
            );
        } else if !boundary.footprint.is_empty() {
            worlds += 1;
        }
    };

    for boundary in &spawn_probe(5).trace() {
        check(boundary);
    }
    let leader = Simulator::new(6);
    Driver::new(leader.clone(), config()).poll_once().unwrap();
    for boundary in &leader.trace() {
        check(boundary);
    }

    assert!(appended >= 4, "only {appended} appends were exercised");
    assert!(worlds >= 4, "only {worlds} world effects were exercised");
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

/// The body of `fn <name>` in the CLI source.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    source
        .split_once(&format!("\nfn {name}("))
        .unwrap_or_else(|| panic!("`fn {name}` is gone from src/app.rs"))
        .1
        .split_once("\n}\n")
        .unwrap_or_else(|| panic!("`fn {name}` has no body"))
        .0
}

/// Every `mutation_output(...)` call in the CLI source, as source text.
///
/// Parens are balanced with string literals skipped, so a `format!` argument
/// carrying a bracket cannot end a call early.
fn mutation_calls(source: &str) -> Vec<&str> {
    let mut calls = Vec::new();
    for (index, needle) in source.match_indices("mutation_output(") {
        if source[..index].ends_with("fn ") {
            continue; // the definition, not a call
        }
        let open = index + needle.len() - 1;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
            if in_string {
                match byte {
                    _ if escaped => escaped = false,
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + offset);
                        break;
                    }
                }
                _ => {}
            }
        }
        calls.push(&source[open..=end.expect("a `mutation_output` call is balanced")]);
    }
    calls
}

/// The tripwire for the drift risk named in `simulator::MIRRORED`.
///
/// The simulated dispatcher hand-mirrors the CLI's `--json` shapes, and
/// nothing in the type system ties the two together. This reads the CLI's own
/// source and fails when the mutation envelope moves, or when a mirrored
/// schema or field stops being emitted *by the region that answers that
/// command* — which is the point of narrowing rather than searching the whole
/// file. `alder.attempt.edit.v0` is claimed by two call sites, binding and
/// updating, and only the binding is modelled here; a whole-file search would
/// keep passing on the strength of the arm nobody simulates.
#[test]
fn the_simulated_dispatcher_still_mirrors_the_cli_pack() {
    let body = function_body(CLI_SOURCE, "mutation_output");
    let envelope: Vec<&str> = body
        .match_indices("object.insert(\"")
        .map(|(index, needle)| {
            body[index + needle.len()..]
                .split('"')
                .next()
                .expect("an inserted key is quoted")
        })
        .collect();
    assert_eq!(
        envelope, MUTATION_ENVELOPE,
        "the CLI's mutation envelope moved; mirror it in simulator/mod.rs"
    );

    let calls = mutation_calls(CLI_SOURCE);
    assert!(
        calls.len() >= 10,
        "only {} mutation_output calls parsed out of src/app.rs; the scanner \
         is broken, not the CLI",
        calls.len()
    );

    for mirrored in MIRRORED {
        let region = match mirrored.site {
            Site::Function(name) => function_body(CLI_SOURCE, name),
            Site::MutationCall(needle) => {
                let matched: Vec<_> = calls.iter().filter(|call| call.contains(needle)).collect();
                assert_eq!(
                    matched.len(),
                    1,
                    "`alder {}` is pinned to `{needle}`, which now matches {} \
                     mutation_output calls; the needle no longer names one \
                     answer",
                    mirrored.command,
                    matched.len()
                );
                matched[0]
            }
        };
        assert!(
            region.contains(&format!("\"{}\"", mirrored.schema)),
            "`alder {}` no longer answers as `{}` from the region that builds \
             it; the simulated dispatcher is mirroring a CLI that moved",
            mirrored.command,
            mirrored.schema
        );
        for field in mirrored.fields {
            assert!(
                region.contains(&format!("\"{field}\"")),
                "`alder {}` no longer emits `{field}`, which the simulator's \
                 answer still carries; mirror the change in simulator/mod.rs",
                mirrored.command
            );
        }
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
        fault_schedule: vec![Fault::torn(5, 0b10), Fault::whole(19)],
    };
    assert_eq!(execute_case(&case), execute_case(&case));
}

#[test]
fn a_daemon_restart_interleaves_with_an_interrupted_spawn() {
    let probe = spawn_probe(9);
    let crash_after_start = position_of(&probe.trace(), "spawn.work-start");
    let case = Case {
        seed: 9,
        operations: vec![
            Operation::SpawnWorker,
            Operation::RestartDaemon,
            Operation::PollDaemon,
        ],
        fault_schedule: vec![Fault::whole(crash_after_start)],
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

/// A worktree torn in half — the directory made, the admin entry not — is the
/// residue that motivates the harness's own path sweep. Pinned as a case of
/// its own so the interesting subset does not depend on proptest finding it.
#[test]
fn a_worktree_torn_before_its_admin_entry_still_converges() {
    let probe = spawn_probe(11);
    let trace = probe.trace();
    let add = trace[position_of(&trace, "spawn.worktree-add") - 1].clone();
    assert_eq!(
        add.footprint,
        vec!["branch", "worktree-entry", "directory", "file"],
        "the worktree footprint changed; re-derive the torn subsets below"
    );
    // The directory and its `.git` file, with no branch and no admin entry:
    // what a `git worktree add` killed part-way through can leave, and what
    // `git worktree prune` would not reclaim.
    for mask in [0b1100, 0b1000, 0b0100] {
        let host = Simulator::new(11);
        host.schedule_faults(vec![Fault::torn(add.ordinal, mask)]);
        assert!(
            catch_sim_crash(|| {
                spawn::spawn(
                    &host,
                    "al-sim",
                    tier::tier("luna").unwrap(),
                    Some("scripted-agent"),
                )
            })
            .is_none(),
            "the torn worktree add did not kill spawn"
        );
        assert!(
            !host.digest().paths.is_empty(),
            "subset {mask:04b} of the worktree footprint left no residue to converge from"
        );
        host.recover(true);
        host.assert_invariant(true);
        // The residue was real, and it took the harness's own sweep to clear
        // it. Production has no such step; that is handoff `al-handoff-vpzdqw`,
        // and this assertion is what keeps the gap from being papered over by
        // a convergence proof that never had to face it.
        assert!(
            host.trace()
                .iter()
                .any(|boundary| boundary.label == "repair.path-sweep"),
            "subset {mask:04b} converged without ever sweeping the stray path"
        );
    }
}

/// Production types the injection and presses Enter as two separate tmux
/// invocations, so a daemon killed between them leaves the pane holding a line
/// nobody submitted while the log already says the pass was woken. Pinned so
/// the subset does not depend on proptest finding it.
#[test]
fn an_injection_torn_before_its_enter_leaves_text_nobody_submitted() {
    let probe = Simulator::new(13);
    Driver::new(probe.clone(), config()).poll_once().unwrap();
    let trace = probe.trace();
    let inject = trace[position_of(&trace, "pass.inject") - 1].clone();
    assert_eq!(
        inject.footprint,
        vec!["typed", "submitted"],
        "the injection footprint changed; re-derive the torn subset below"
    );

    // The text sent, the Enter not.
    let host = Simulator::new(13);
    host.schedule_faults(vec![Fault::torn(inject.ordinal, 0b01)]);
    let mut driver = Driver::new(host.clone(), config());
    assert!(
        catch_sim_crash(|| driver.poll_once()).is_none(),
        "the torn injection did not kill the poll"
    );
    let digest = host.digest();
    assert!(
        digest
            .sessions
            .iter()
            .any(|session| session.ends_with(":typed")),
        "the torn injection left no unsubmitted text: {digest:#?}"
    );
    assert!(
        digest.state.contains("Open"),
        "the pass should be open and unhanded-over: {digest:#?}"
    );

    // Convergence, and with it the pane invariant: recovery times the
    // abandoned pass out, and `assert_invariant` holds the leftover text to
    // the rule that makes it transient rather than pretending it is gone.
    host.recover(false);
    host.assert_invariant(false);
}

fn generated_case(seed: u64, noise: Vec<u8>, fault_slots: Vec<(u8, u8)>) -> Case {
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

    // Each fault is a one-based number of effects after the preceding crash,
    // plus the subset of that effect's footprint that lands before the process
    // dies. Relative distances stay reachable when an earlier crash changes
    // which path recovery takes, and Vec shrinking can remove either crash.
    let faults = fault_slots
        .into_iter()
        .map(|(slot, torn)| Fault::torn(1 + usize::from(slot) % 32, u32::from(torn)))
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
    fn generated_restart_spawn_death_and_torn_crash_interleavings_converge(
        seed in any::<u64>(),
        noise in prop::collection::vec(any::<u8>(), 0..8),
        fault_slots in prop::collection::vec((any::<u8>(), any::<u8>()), 0..=2),
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
    let host = Simulator::new(4);
    host.set_next_agent(AgentScript::DieMidPass);
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    host.recover(false);
    host.assert_invariant(false);
}
