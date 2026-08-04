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

use alderd::{driver::Driver, loop_state::LoopState, spawn, tier};
use proptest::prelude::*;
use simulator::{
    AgentScript, Boundary, Case, Fault, Operation, Simulator, assert_case_converges,
    catch_sim_crash, config, execute_case,
};

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

/// The same enumeration over a complete daemon poll: the session
/// reconciliation, the injection, the notes write, and every read and clock
/// tick between them, each torn every way its footprint allows.
///
/// This is the crash half of the new invariant: nothing durable records a
/// wake, so a crash anywhere in the wake path costs at most one missed or one
/// duplicated delivery, and recovery converges without any repair verdicts.
#[test]
fn every_torn_subset_of_the_wake_lifecycle_converges() {
    let probe = Simulator::new(3);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    probe.run_leader_if_injected();
    let lifecycle = probe.trace();
    for label in ["wake.session-create", "wake.inject", "notes.write"] {
        position_of(&lifecycle, label);
    }

    for boundary in &lifecycle {
        for mask in 0..boundary.subsets() {
            let host = Simulator::new(3);
            host.schedule_faults(vec![Fault::torn(boundary.ordinal, mask)]);
            let mut driver = Driver::new(host.clone(), config());
            assert!(
                catch_sim_crash(|| {
                    driver.poll_once()?;
                    host.run_leader_if_injected();
                    Ok::<(), alderd::error::DriverError>(())
                })
                .is_none(),
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

/// A crash between the injection and the notes write is the duplicate-wake
/// window: the leader was handed the line, but the restarted daemon does not
/// know that and delivers it again. Pinned so the interesting subset does not
/// depend on enumeration order, and asserted to actually produce the second
/// delivery — which the invariant then holds harmless.
#[test]
fn a_crash_between_injection_and_notes_delivers_the_wake_twice_harmlessly() {
    let probe = Simulator::new(29);
    let mut driver = Driver::new(probe.clone(), config());
    driver.poll_once().unwrap();
    let trace = probe.trace();
    let inject = position_of(&trace, "wake.inject");
    let notes = position_of(&trace, "notes.write");
    assert!(inject < notes, "the injection must precede the notes write");

    let host = Simulator::new(29);
    // The injection lands whole; the process dies before the notes write.
    host.schedule_faults(vec![Fault::whole(inject)]);
    let mut driver = Driver::new(host.clone(), config());
    assert!(catch_sim_crash(|| driver.poll_once()).is_none());
    assert_eq!(host.wakes_delivered(), 1);

    host.recover(false);
    host.assert_invariant(false);
    assert!(
        host.wakes_delivered() >= 2,
        "the restarted daemon never re-delivered the unnoted wake"
    );
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
    // The daemon poll path is checked too, and the stronger fact rides along:
    // it contains no append at all. The daemon appends nothing.
    let leader = Simulator::new(6);
    Driver::new(leader.clone(), config()).poll_once().unwrap();
    leader.run_leader_if_injected();
    for boundary in &leader.trace() {
        assert!(
            !boundary.footprint.contains(&"append"),
            "the daemon poll path appended to the log: {boundary:#?}"
        );
        check(boundary);
    }

    assert!(appended >= 2, "only {appended} appends were exercised");
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

/// Every key of `simulated` must exist in `real`.
///
/// The direction is the whole point. An extra key here means the simulator
/// serves a shape production does not have, so daemon code could come to
/// depend on it while the real CLI hands back nothing. A key production has
/// and the simulator omits is the safe direction: the simulator fails first.
fn assert_no_invented_keys(what: &str, simulated: &serde_json::Value, real: &serde_json::Value) {
    let simulated = simulated
        .as_object()
        .unwrap_or_else(|| panic!("{what}: the simulated answer is not an object: {simulated}"));
    let real = real
        .as_object()
        .unwrap_or_else(|| panic!("{what}: production's value is not an object: {real}"));
    for key in simulated.keys() {
        assert!(
            real.contains_key(key),
            "{what}: the simulator serves `{key}`, which production does not \
             emit; its real keys are {:?}",
            real.keys().collect::<Vec<_>>()
        );
    }
}

/// Every key `production_reads` must exist in `real`.
fn assert_still_emitted(what: &str, real: &serde_json::Value, production_reads: &[&str]) {
    for key in production_reads {
        assert!(
            real.get(key).is_some(),
            "{what}: production no longer emits `{key}`, which alderd reads; \
             its real keys are {:?}",
            real.as_object()
                .map(|object| object.keys().collect::<Vec<_>>())
        );
    }
}

/// The half of the drift guard a source scan cannot reach.
///
/// `current` and each `in_flight` item are not `json!` literals in the CLI:
/// production serialises a domain value, so the field names live on the type
/// and no search of `src/app.rs` will ever find them. Scanning for `current`
/// or `in_flight` therefore keeps passing while every key *inside* has been
/// renamed — the same failure the schema sites had one level up. This compares
/// the simulator's answers against the real serialisation instead.
#[test]
fn the_simulated_dispatcher_serves_no_nested_shape_production_lacks() {
    let host = spawn_probe(17);
    Driver::new(host.clone(), config()).poll_once().unwrap();
    let state = host.snapshot().state;

    // `show <work>` — read by `Brief::from_show`, two levels down into checks.
    let answer = alderd::effects::Effects::alder(&host, &["show", "al-sim"]).unwrap();
    let work = serde_json::to_value(state.work.get("al-sim").expect("the work item")).unwrap();
    assert_no_invented_keys("show <work> current", &answer["current"], &work);
    assert_still_emitted(
        "show <work> current",
        &work,
        &["id", "title", "spec", "checks"],
    );
    assert_no_invented_keys(
        "show <work> current.checks[]",
        &answer["current"]["checks"][0],
        &work["checks"][0],
    );
    assert_still_emitted(
        "show <work> current.checks[]",
        &work["checks"][0],
        &["key", "description"],
    );

    // `status --section in_flight` items — read by `spawn::open_attempt`.
    let answer =
        alderd::effects::Effects::alder(&host, &["status", "--section", "in_flight"]).unwrap();
    let attempt = state
        .attempts
        .values()
        .next()
        .expect("the spawn started one");
    let attempt = serde_json::to_value(attempt).unwrap();
    assert_no_invented_keys("in_flight[]", &answer["in_flight"][0], &attempt);
    assert_still_emitted("in_flight[]", &attempt, &["id", "work_id", "handle"]);
}

/// The simulator serves the production status builder, then the driver's real
/// reader consumes that document. The scenario populates every field the loop
/// section carries, so the check covers more than the empty shape.
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
    // fields the loop actually turns on. `review_at` is null on both sides: no
    // command the daemon sends defers work, so no scenario this harness
    // reaches populates it — the CLI test suite covers it end to end.
    assert!(from_real.head > 0, "the status document reports head 0");
    assert_eq!(from_real.engine.as_deref(), Some("stub"));
    assert!(
        from_real.nudge_requested_seq.is_some(),
        "loop.nudge_requested_seq is gone, and it is the manual trigger"
    );
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

/// An execution outliving its ended attempt — the leader ended the attempt,
/// then died before killing the session — must surface as production's
/// `orphan` finding through the ordinary observe-then-reconcile round, and
/// the repair must kill exactly that session.
///
/// This test FAILS if `orphan` stops surfacing: the observation sweep must
/// keep the ended attempt's liveness key `present` while the session lives
/// (retiring it on attempt end was the regression), and the harness's stray
/// sweep deliberately refuses to kill a session an ended attempt still
/// accounts for, so nothing here converges around a silent `orphan` path.
#[test]
fn an_ended_attempts_live_session_surfaces_as_an_orphan_and_is_killed() {
    let host = spawn_probe(13);
    // The world has one live worker; make its liveness durable first, as any
    // running loop would have.
    assert!(host.observe_and_reconcile().is_empty());

    // The attempt ends while its session is still running.
    host.end_attempt(
        "al-sim-attempt-1",
        "cancelled",
        "superseded; session left running",
    );
    assert!(host.session_exists("alder-work-al-sim"));

    // The ordinary round: refresh keeps the key present, reconcile names the
    // orphan, and the suggestion names the execution verbatim.
    let findings = host.observe_and_reconcile();
    let orphan: Vec<_> = findings
        .iter()
        .filter(|finding| finding.kind == "orphan")
        .collect();
    assert_eq!(orphan.len(), 1, "no orphan surfaced: {findings:#?}");
    assert_eq!(orphan[0].attempt_id.as_deref(), Some("al-sim-attempt-1"));
    assert_eq!(orphan[0].handle.as_deref(), Some("tmux:alder-work-al-sim"));
    assert!(
        orphan[0]
            .suggested_command
            .as_deref()
            .is_some_and(|suggestion| suggestion.contains("kill")),
        "{:?}",
        orphan[0].suggested_command
    );

    // Acting on the finding kills the named session; the next round retires
    // the key and reports nothing.
    host.repair(&findings);
    assert!(!host.session_exists("alder-work-al-sim"));
    assert!(host.observe_and_reconcile().is_empty());
    // The freed deterministic name is reusable: full recovery respawns the
    // still-open work and reaches the ordinary fixpoint.
    host.recover(true);
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
        // it. Production has no such step; that is work `al-3pph8m` (formerly handoff al-handoff-vpzdqw),
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
/// nobody submitted — a missed wake. Pinned so the subset does not depend on
/// proptest finding it.
#[test]
fn an_injection_torn_before_its_enter_leaves_text_nobody_submitted() {
    let probe = Simulator::new(13);
    Driver::new(probe.clone(), config()).poll_once().unwrap();
    let trace = probe.trace();
    let inject = trace[position_of(&trace, "wake.inject") - 1].clone();
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
    // Nothing was handed over and — the point — nothing durable says
    // otherwise: the wake that was missed is recorded nowhere.
    assert_eq!(host.wakes_delivered(), 0);

    // Convergence, and with it the pane invariant: the restarted daemon does
    // not know the dirty session, restarts it, and delivers a fresh wake;
    // `assert_invariant` holds any leftover text to the rule that makes it
    // transient rather than pretending it is gone.
    host.recover(false);
    host.assert_invariant(false);
    assert!(
        host.wakes_delivered() >= 1,
        "the missed wake was never made up"
    );
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

/// A simulated crash has to cost the daemon its memory, not just its stack.
///
/// The driver keeps process-local state — the session it launched, whether the
/// next injection must bootstrap — and a real crash erases all of it. A case
/// that caught the panic and carried the same `Driver` into the next operation
/// let that state outlive the process it lived in, and the difference is not
/// academic: a daemon that still believes it owns the leader session reuses it
/// instead of restarting it, so it types the next injection straight onto the
/// text the torn one left behind.
#[test]
fn a_daemon_that_died_mid_injection_does_not_reuse_the_pane_it_dirtied() {
    // The same prefix `execute_case` runs, so boundary ordinals line up.
    let probe = spawn_probe(23);
    Driver::new(probe.clone(), config()).poll_once().unwrap();
    let inject = position_of(&probe.trace(), "wake.inject");

    // Tear the injection — text typed, Enter not — and then let the case go on
    // to fire again with no restart of its own in between. The nudge inside
    // the death operation is what makes it fire.
    let mut case = generated_case(23, vec![1], Vec::new());
    case.fault_schedule = vec![Fault::torn(inject, 0b01)];
    let digest = execute_case(&case);
    assert!(
        digest
            .trace
            .iter()
            .any(|boundary| boundary.contains("torn")),
        "the injection was never torn: {digest:#?}"
    );
}

/// One armed script means one scripted act, whoever runs it.
///
/// The script belongs to the wake, not to a session, and getting that wrong
/// has failed in both directions here. Arming only the *next* session creation
/// never reached a leader the daemon reuses, so an operation named for a death
/// produced none. Arming both the live session and the next creation fired on
/// the live one and then stayed armed for its replacement, so one operation
/// modelled two deaths and quietly changed the interleaving under test. This
/// pins both edges at once: a reused leader dies, and the leader created after
/// it is an ordinary one.
#[test]
fn one_armed_leader_death_kills_exactly_one_leader() {
    let host = Simulator::new(31);
    let mut driver = Driver::new(host.clone(), config());
    // A leader session exists and this daemon knows it, so the next fire
    // reuses it rather than restarting it.
    driver.poll_once().unwrap();
    host.run_leader_if_injected();
    host.script_leader(AgentScript::DieMidAct);
    host.nudge();
    driver.poll_once().unwrap();
    host.run_leader_if_injected();
    // The scripted leader is gone; firing again builds a replacement, which
    // acts as an ordinary leader.
    host.nudge();
    let _ = driver.poll_once();
    host.run_leader_if_injected();

    let deaths = host
        .trace()
        .iter()
        .filter(|boundary| boundary.label == "agent.die")
        .count();
    assert_eq!(
        deaths,
        1,
        "one armed script produced {deaths} deaths; trace={:#?}",
        host.trace()
    );
    host.recover(false);
    host.assert_invariant(false);
}

/// The operation named for a leader death has to actually kill a leader.
///
/// It mostly did not. Scripting the death set only what the *next* session
/// creation would run, and a daemon that already has a session reuses it — so
/// unless something happened to restart it first, the scripted death never
/// reached the running leader and the pass ended normally. That matters well
/// beyond this one operation: it is the generated cases' only source of
/// mid-pass death, so a whole class of interleavings named for it was not
/// exercising it, and the convergence evidence read stronger than it was.
///
/// This uses the generated shape at its least helpful — no noise, so nothing
/// restarts the daemon between the poll that creates the leader session and
/// the operation meant to kill it.
#[test]
fn the_leader_death_operation_kills_a_leader_that_is_already_running() {
    let case = generated_case(21, Vec::new(), Vec::new());
    let digest = execute_case(&case);
    let created = digest
        .trace
        .iter()
        .position(|boundary| boundary.contains("wake.session-create"))
        .expect("the poll before the death creates a leader session");
    let died = digest
        .trace
        .iter()
        .position(|boundary| boundary.contains("agent.die"))
        .unwrap_or_else(|| {
            panic!("no leader died in a case named for a leader death: {digest:#?}")
        });
    assert!(
        created < died,
        "the leader that died was created after the death, so the operation \
         never reached a running leader: {digest:#?}"
    );
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
        // `Tick` can leave the daemon between observations, so the generated
        // prefix itself is not a fixed point. `assert_case_converges` settles
        // each complete schedule only after its recovery loop has drained that
        // logical-time work, and asserts the shared and SimHost-local
        // invariants there before returning this replay witness.
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

#[test]
fn a_leader_stub_can_die_mid_act_without_stranding_anything() {
    let host = Simulator::new(4);
    host.script_leader(AgentScript::DieMidAct);
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    host.run_leader_if_injected();
    host.recover(false);
    host.assert_invariant(false);
}
