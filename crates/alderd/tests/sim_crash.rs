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
        if let Some(schema) = mirrored.schema {
            assert!(
                region.contains(&format!("\"{schema}\"")),
                "`alder {}` no longer answers as `{schema}` from the region \
                 that builds it; the simulated dispatcher is mirroring a CLI \
                 that moved",
                mirrored.command
            );
        }
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

/// Every key production's loop section carries, split by whether [`LoopState`]
/// — the driver's whole view of the durable log — reads it.
///
/// Both halves are asserted against the document production just built, so a
/// field added to `loop_section` fails here until somebody classifies it.
/// Everything in the read half is then *required* of the simulator, because
/// `LoopState` defaults every field it can: an omission deserialises to a
/// default rather than to an error, and the simulated loop would take a
/// decision production never takes with this guard still green.
const LOOP_READ: [&str; 7] = [
    "paused",
    "pause_reason",
    "engine",
    "rotate_pending",
    "nudge_pending",
    "open_pass",
    "last_pass",
];
const LOOP_UNREAD: [&str; 0] = [];
const OPEN_PASS_READ: [&str; 4] = ["id", "engine", "handle", "started_at"];
const OPEN_PASS_UNREAD: [&str; 2] = ["triggers", "at_head"];
const LAST_PASS_READ: [&str; 5] = ["id", "outcome", "wake_at", "ended_at", "ended_seq"];
const LAST_PASS_UNREAD: [&str; 2] = ["engine", "report_line"];

/// One object of the loop section, compared in both directions.
///
/// Production is held to its own inventory — exactly `read` plus `unread`, so a
/// new field cannot arrive unclassified. The simulator is held to inventing
/// nothing production lacks *and* to omitting nothing the driver reads.
fn assert_compared_both_ways(
    what: &str,
    simulated: &serde_json::Value,
    real: &serde_json::Value,
    read: &[&str],
    unread: &[&str],
) {
    let mut classified: Vec<&str> = read.iter().chain(unread).copied().collect();
    classified.sort_unstable();
    let emitted: Vec<&str> = real
        .as_object()
        .unwrap_or_else(|| panic!("{what}: production's value is not an object: {real}"))
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        emitted, classified,
        "{what}: production's fields moved. Each one is either read by \
         `LoopState` or deliberately not, and this test has to say which."
    );
    assert_no_invented_keys(what, simulated, real);
    for key in read {
        assert!(
            simulated.get(key).is_some(),
            "{what}: the simulator omits `{key}`, which the driver reads; \
             `LoopState` would default it and the simulated loop would decide \
             on a value production never sent"
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

    // `show <pass>` — read by the driver as /current/state and /current/outcome.
    let pass_id = state.passes.keys().next().expect("the poll opened a pass");
    let answer = alderd::effects::Effects::alder(&host, &["show", pass_id]).unwrap();
    let pass = serde_json::to_value(&state.passes[pass_id]).unwrap();
    assert_no_invented_keys("show <pass> current", &answer["current"], &pass);
    assert_still_emitted("show <pass> current", &pass, &["id", "state", "outcome"]);

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

/// The loop section, driven end to end: production's own status packer in, a
/// parsed [`LoopState`] out, compared both ways.
///
/// A scan settles none of it. `"loop"` occurs twice in `fn status` — once as
/// the key the driver reads, once as a heading in the human rendering — so a
/// search finds a match whichever one went. Inside the section, `id` appears in
/// `open_pass` and again in `last_pass`, and `engine` at the top level and
/// again inside `open_pass`.
///
/// Nor does building the document around production's section: a test that
/// supplies the `loop` key itself is supplying the very thing
/// `LoopState::from_status` looks up, and cannot notice production renaming or
/// dropping it. So `app::status_document` — the real packer, envelope and key
/// included — builds the whole document over the state the simulator answered
/// from. The two documents are then compared field by field in both
/// directions, and read back through production's own reader, whose parsed
/// results must be equal.
#[test]
fn the_simulated_status_serves_the_loop_section_production_builds() {
    // Both sub-objects have to be populated or half the comparison is vacuous,
    // so this wants one pass ended and another still open. Crashing right
    // after the second wake leaves exactly that.
    let probe = Simulator::new(19);
    Driver::new(probe.clone(), config()).poll_once().unwrap();
    probe.nudge();
    probe.schedule_faults(Vec::new());
    Driver::new(probe.clone(), config()).poll_once().unwrap();
    let wake = position_of(&probe.trace(), "pass.wake");

    let host = Simulator::new(19);
    Driver::new(host.clone(), config()).poll_once().unwrap();
    host.nudge();
    host.schedule_faults(vec![Fault::whole(wake)]);
    let mut driver = Driver::new(host.clone(), config());
    assert!(
        catch_sim_crash(|| driver.poll_once()).is_none(),
        "the wake fault did not fire; trace={:#?}",
        host.trace()
    );

    let snapshot = host.snapshot();
    let state = &snapshot.state;
    assert!(state.open_pass().is_some(), "no pass is open to compare");
    assert!(
        state.last_ended_pass().is_some(),
        "no pass has ended to compare"
    );

    // `status_document` is production's packer only for as long as `status`
    // is what calls it, and that is a question about one unique identifier —
    // the kind a source scan settles outright, unlike "which of the two
    // `"loop"` literals is the key the driver reads".
    assert!(
        function_body(CLI_SOURCE, "status").contains("status_document("),
        "`fn status` no longer builds its answer with `status_document`, so \
         driving that packer proves nothing about what the CLI hands back"
    );

    // Production's own packer, over the state the simulator just answered
    // from. `head` and the `loop` key are written by `src/app.rs`; nothing
    // here reproduces them.
    let real = alder::app::status_document(&snapshot.head, false, None, state);
    let simulated = alderd::effects::Effects::alder(&host, &["status"]).unwrap();

    // The envelope, which used to be a `MIRRORED` row scanned out of the
    // source. Two produced documents settle it outright, so the row is gone.
    // `schema` and `revision` are not read by the driver — the simulator
    // serves them for fidelity — so dropping either shows up as the simulator
    // inventing a key rather than as production losing one.
    assert_no_invented_keys("status", &simulated, &real);
    assert_eq!(
        simulated["schema"], real["schema"],
        "the simulator answers `status` as a schema production no longer claims"
    );
    // `head` and `loop` are the two the driver does read, and the head is the
    // whole log trigger. Both directions, on both.
    assert_still_emitted("status", &real, &["head", "loop"]);
    for key in ["head", "loop"] {
        assert!(
            simulated.get(key).is_some(),
            "the simulator's status carries no `{key}`, which the driver reads"
        );
    }

    assert_compared_both_ways(
        "status loop",
        &simulated["loop"],
        &real["loop"],
        &LOOP_READ,
        &LOOP_UNREAD,
    );
    assert_compared_both_ways(
        "status loop.open_pass",
        &simulated["loop"]["open_pass"],
        &real["loop"]["open_pass"],
        &OPEN_PASS_READ,
        &OPEN_PASS_UNREAD,
    );
    assert_compared_both_ways(
        "status loop.last_pass",
        &simulated["loop"]["last_pass"],
        &real["loop"]["last_pass"],
        &LAST_PASS_READ,
        &LAST_PASS_UNREAD,
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
    // fields the loop actually turns on. `wake_at` is null on both sides: no
    // command the daemon sends sets one, so no scenario this harness reaches
    // populates it. Requiring the simulator to emit the key is what catches it
    // being renamed or dropped; the equality above carries its value whenever
    // there is one.
    assert!(from_real.head > 0, "the status document reports head 0");
    let open = from_real
        .open_pass
        .expect("production reports the open pass");
    assert!(!open.id.is_empty(), "loop.open_pass.id is empty");
    assert!(!open.handle.is_empty(), "loop.open_pass.handle is empty");
    let last = from_real
        .last_pass
        .expect("production reports the last pass");
    assert!(last.outcome.is_some(), "loop.last_pass.outcome is gone");
    assert!(
        last.ended_seq.is_some(),
        "loop.last_pass.ended_seq is gone, and it is the whole log trigger"
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
    let inject = position_of(&probe.trace(), "pass.inject");

    // Tear the injection — text typed, Enter not — and then let the case go on
    // to fire again with no restart of its own in between. The one extra poll
    // matters: it times the abandoned pass out, because while a pass is open
    // the driver only awaits it and never reaches the injection path at all.
    // The nudge inside the death operation is what then makes it fire.
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

/// One armed script means one scripted pass, whoever runs it.
///
/// The script belongs to the pass, not to a session, and getting that wrong
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
    host.script_leader(AgentScript::DieMidPass);
    host.nudge();
    driver.poll_once().unwrap();
    // The scripted leader is gone; firing again builds a replacement.
    host.nudge();
    let _ = driver.poll_once();

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
        .position(|boundary| boundary.contains("pass.session-create"))
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
    host.script_leader(AgentScript::DieMidPass);
    let mut driver = Driver::new(host.clone(), config());
    driver.poll_once().unwrap();
    host.recover(false);
    host.assert_invariant(false);
}
