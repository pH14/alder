use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Read},
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use crate::{
    cli::{
        AttemptCommand, AttemptEditArgs, Command, DebugCommand, DebugDbCommand, DebugLogCommand,
        LoopCommand, NonSuccessOutcome, ObservationCommand, QuestionCommand, StatusSection,
        WorkAddArgs, WorkCommand, WorkEditArgs,
    },
    config::{Project, initialize},
    domain::{
        AppendResult, Attempt, AttemptOutcome, ChangeMode, CheckDefinition, CheckStatus,
        CheckUpdate, Event, EventPayload, GraphChangeDocument, Head, NullableString,
        ObservationAppend, ObservationKey, ProjectLog, ProjectState, Question, Snapshot,
        WorkEventPayload, WorkStateChange, prepare_change,
    },
    error::{AlderError, Result},
    observer,
    projection::Projection,
};
use alder_log::GitLog;

#[derive(Debug)]
pub struct Output {
    pub json: Value,
    pub human: String,
}

impl Output {
    fn new(json: Value, human: impl Into<String>) -> Self {
        Self {
            json,
            human: human.into(),
        }
    }
}

pub struct App;

struct Context {
    project: Project,
    log: ProjectLog<GitLog>,
    projection: Projection,
    snapshot: Snapshot,
}

struct RefreshApplication {
    runs: Vec<observer::ObserverRunResult>,
    appended: usize,
    retired: usize,
    head: Head,
}

impl App {
    pub fn run(command: &Command) -> Result<Output> {
        if let Command::Init(args) = command {
            return Self::init(args);
        }
        let mut context = load_context()?;
        match command {
            Command::Init(_) => unreachable!(),
            Command::Status(args) => status(
                &mut context,
                args.changes.as_deref(),
                args.full,
                &args.section,
            ),
            Command::Next(args) => next(&mut context, args.changes.as_deref()),
            Command::Show(args) => show(&context, &args.id),
            Command::Observations => observations(&context),
            Command::Refresh => refresh(&context),
            Command::Reconcile(args) => reconcile(&context, !args.no_refresh),
            Command::Work(args) => work(&context, &args.command),
            Command::Attempt(args) => match &args.command {
                AttemptCommand::Edit(args) => attempt_edit(&context, args),
                AttemptCommand::End(args) => {
                    let outcome: AttemptOutcome = args.outcome.into();
                    require_reason("--why", Some(&args.why))?;
                    let result =
                        context
                            .log
                            .end_attempt(&args.attempt, outcome, args.why.clone())?;
                    Ok(mutation_output(
                        "alder.attempt.end.v0",
                        &result,
                        json!({"attempt_id": args.attempt, "outcome": format_outcome(outcome)}),
                        format!("{}  ended {}", args.attempt, format_outcome(outcome)),
                    ))
                }
            },
            Command::Question(args) => match &args.command {
                QuestionCommand::Answer(args) => {
                    let result = context.log.answer(&args.question, args.answer.clone())?;
                    Ok(mutation_output(
                        "alder.question.answer.v0",
                        &result,
                        json!({"question_id": args.question}),
                        format!("{}  answered", args.question),
                    ))
                }
            },
            Command::Observation(args) => match &args.command {
                ObservationCommand::Report(args) => observation_mutation(
                    context.log.report_observation(
                        ObservationKey {
                            observer: args.observer.clone(),
                            subject: args.subject.clone(),
                            field: args.field.clone(),
                        },
                        args.level.clone(),
                    )?,
                    "alder.observation.report.v0",
                    "reported",
                    json!({
                        "observer": args.observer,
                        "subject": args.subject,
                        "field": args.field,
                        "level": args.level,
                    }),
                ),
                ObservationCommand::Retire(args) => observation_mutation(
                    context.log.retire_observation(ObservationKey {
                        observer: args.observer.clone(),
                        subject: args.subject.clone(),
                        field: args.field.clone(),
                    })?,
                    "alder.observation.retire.v0",
                    "retired",
                    json!({
                        "observer": args.observer,
                        "subject": args.subject,
                        "field": args.field,
                    }),
                ),
            },
            Command::Loop(args) => loop_command(&context, &args.command),
            Command::Debug(args) => debug(&context, &args.command),
        }
    }

    fn init(args: &crate::cli::InitArgs) -> Result<Output> {
        let cwd = env::current_dir()?;
        let result = initialize(&cwd, &args.prefix, &args.remote, &args.reference)?;
        let status = if result.already_initialized {
            "already_initialized"
        } else {
            "initialized"
        };
        Ok(Output::new(
            json!({
                "schema": "alder.init.v0",
                "status": status,
                "path": result.project.config_path,
                "prefix": result.project.config.prefix,
                "remote": result.project.config.store.remote,
                "ref": result.project.config.store.reference,
                "head": result.head_seq,
            }),
            if result.already_initialized {
                format!(
                    "already initialized {} · {} {}",
                    result.project.config_path.display(),
                    result.project.config.store.remote,
                    result.project.config.store.reference
                )
            } else {
                format!(
                    "initialized {} · {} {}",
                    result.project.config_path.display(),
                    result.project.config.store.remote,
                    result.project.config.store.reference
                )
            },
        ))
    }
}

fn load_context() -> Result<Context> {
    let cwd = env::current_dir()?;
    let project = Project::discover(&cwd)?;
    let actor = actor();
    let marker = project.append_marker();
    let log = ProjectLog::new(project.store(), &project.config.prefix, actor)
        // The marker is a hint: a failed touch must never fail the append.
        .with_on_append(move || {
            let _ = fs::write(&marker, b"");
        });
    let snapshot = log.snapshot()?;
    let projection = Projection::new(project.state_db());
    projection.sync(&snapshot.head, &snapshot.events, &snapshot.state)?;
    Ok(Context {
        project,
        log,
        projection,
        snapshot,
    })
}

fn actor() -> String {
    env::var("ALDER_ACTOR")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn work(context: &Context, command: &WorkCommand) -> Result<Output> {
    match command {
        WorkCommand::Add(args) => work_add(context, args),
        WorkCommand::Edit(args) => work_edit(context, args),
        WorkCommand::Start(args) => {
            let metadata = parse_metadata(&args.meta)?;
            let (result, id) = context.log.start(&args.work, args.tier.clone(), metadata)?;
            Ok(mutation_output(
                "alder.work.start.v0",
                &result,
                json!({"work_id": args.work, "attempt_id": id, "tier": args.tier}),
                id,
            ))
        }
        WorkCommand::Finish(args) => {
            if args.external && args.attempt.is_some() {
                return Err(AlderError::validation(
                    "--external cannot be combined with --attempt",
                ));
            }
            if !args.external && args.evidence.is_some() {
                return Err(AlderError::validation(
                    "--evidence is accepted only with --external",
                ));
            }
            let stranded = context.snapshot.state.unanswered_questions(&args.work);
            let result = context.log.finish(
                &args.work,
                args.attempt.clone(),
                args.external,
                args.evidence.clone(),
            )?;
            Ok(mutation_output(
                "alder.work.finish.v0",
                &result,
                json!({
                    "work_id": args.work,
                    "attempt_id": args.attempt,
                    "external": args.external,
                    "stranded_questions": stranded,
                }),
                format!("{}  done{}", args.work, stranded_note(&stranded)),
            ))
        }
        WorkCommand::Drop(args) => {
            let stranded = context.snapshot.state.unanswered_questions(&args.work);
            let result = context.log.drop_work(
                &args.work,
                args.attempt.clone(),
                args.outcome.map(Into::into),
                args.why.clone(),
            )?;
            let downstream = context.snapshot.state.downstream(&args.work);
            Ok(mutation_output(
                "alder.work.drop.v0",
                &result,
                json!({
                    "work_id": args.work,
                    "attempt_id": args.attempt,
                    "outcome": args.outcome.map(|outcome| format_outcome(outcome.into())),
                    "affected_downstream": downstream,
                    "stranded_questions": stranded,
                }),
                format!(
                    "{}  dropped{}{}",
                    args.work,
                    if downstream.is_empty() {
                        String::new()
                    } else {
                        format!(" · affects {}", downstream.join(", "))
                    },
                    stranded_note(&stranded),
                ),
            ))
        }
        WorkCommand::Reopen(args) => {
            require_reason("--why", Some(&args.why))?;
            let result = context.log.reopen(&args.work, args.why.clone())?;
            Ok(mutation_output(
                "alder.work.reopen.v0",
                &result,
                json!({"work_id": args.work}),
                format!("{}  reopened", args.work),
            ))
        }
        WorkCommand::Block(args) => {
            require_reason("--why", Some(&args.why))?;
            let until = args.until.as_deref().map(parse_instant).transpose()?;
            let result = context.log.set_work_state(
                &args.work,
                WorkStateChange::Block {
                    reason: args.why.clone(),
                    until,
                },
            )?;
            Ok(mutation_output(
                "alder.work.block.v0",
                &result,
                json!({
                    "work_id": args.work,
                    "state": "blocked",
                    "until": until.map(|until| until.to_rfc3339()),
                }),
                match until {
                    Some(until) => {
                        format!("{}  blocked until {}", args.work, until.to_rfc3339())
                    }
                    None => format!("{}  blocked", args.work),
                },
            ))
        }
        WorkCommand::Unblock(args) => {
            require_reason("--why", Some(&args.why))?;
            let result = context.log.set_work_state(
                &args.work,
                WorkStateChange::Unblock {
                    reason: args.why.clone(),
                },
            )?;
            Ok(mutation_output(
                "alder.work.unblock.v0",
                &result,
                json!({"work_id": args.work, "state": "open"}),
                format!("{}  open", args.work),
            ))
        }
        WorkCommand::Ask(args) => {
            let (result, id) = context.log.ask(&args.work, args.question.clone())?;
            Ok(mutation_output(
                "alder.work.ask.v0",
                &result,
                json!({"work_id": args.work, "question_id": id}),
                id,
            ))
        }
    }
}

fn loop_command(context: &Context, command: &LoopCommand) -> Result<Output> {
    match command {
        LoopCommand::Pause(args) => {
            let result = context.log.pause_loop(args.why.clone())?;
            Ok(mutation_output(
                "alder.loop.pause.v0",
                &result,
                json!({"paused": true, "why": args.why}),
                "loop paused",
            ))
        }
        LoopCommand::Resume => {
            let result = context.log.resume_loop()?;
            Ok(mutation_output(
                "alder.loop.resume.v0",
                &result,
                json!({"paused": false}),
                "loop resumed",
            ))
        }
        LoopCommand::Use(args) => {
            let result = context.log.select_engine(args.engine.clone())?;
            Ok(mutation_output(
                "alder.loop.use.v0",
                &result,
                json!({"engine": args.engine}),
                format!("loop engine {}", args.engine),
            ))
        }
        LoopCommand::Rotate(args) => {
            let result = context.log.request_rotation(args.why.clone())?;
            Ok(mutation_output(
                "alder.loop.rotate.v0",
                &result,
                json!({"rotate_pending": true, "why": args.why}),
                "rotation requested",
            ))
        }
        LoopCommand::Nudge(args) => {
            let result = context.log.request_nudge(args.why.clone())?;
            Ok(mutation_output(
                "alder.loop.nudge.v0",
                &result,
                json!({"nudge_pending": true, "why": args.why}),
                "nudge requested",
            ))
        }
    }
}

fn work_add(context: &Context, args: &WorkAddArgs) -> Result<Output> {
    if let Some(path) = args.from.as_deref() {
        ensure_no_direct_work_fields(args)?;
        let document = read_change(path)?;
        let prepared =
            context
                .log
                .allocate_change(&context.snapshot, &document, ChangeMode::AddOnly)?;
        let mappings = prepared.mappings.clone();
        let result = context
            .log
            .commit_change(&context.snapshot, &document, prepared)?;
        let human = mappings
            .iter()
            .map(|(local, id)| format!("{local:<20} {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(mutation_output(
            "alder.work.add.v0",
            &result,
            json!({"work": mappings.iter().map(|(local, id)| json!({"local": local, "work_id": id})).collect::<Vec<_>>()}),
            human,
        ));
    }
    let checks = parse_checks(&args.check)?;
    let title = args
        .title
        .clone()
        .ok_or_else(|| AlderError::validation("work add requires --title"))?;
    let (result, id) = context.log.add_work(
        title,
        args.spec.clone(),
        args.priority,
        args.requires.clone(),
        checks,
    )?;
    Ok(mutation_output(
        "alder.work.add.v0",
        &result,
        json!({"work_id": id}),
        id,
    ))
}

fn ensure_no_direct_work_fields(args: &WorkAddArgs) -> Result<()> {
    if args.title.is_some()
        || args.spec.is_some()
        || args.priority != 0
        || !args.requires.is_empty()
        || !args.check.is_empty()
    {
        Err(AlderError::validation(
            "--from cannot be combined with direct work fields",
        ))
    } else {
        Ok(())
    }
}

fn work_edit(context: &Context, args: &WorkEditArgs) -> Result<Output> {
    if let Some(path) = args.from.as_deref() {
        if args.work.is_some() || has_single_edit_fields(args) {
            return Err(AlderError::validation(
                "--from cannot be combined with a work ID or direct edit fields",
            ));
        }
        let document = read_change(path)?;
        let prepared =
            context
                .log
                .allocate_change(&context.snapshot, &document, ChangeMode::Edit)?;
        let mappings = prepared.mappings.clone();
        let edited: Vec<_> = document.edit.iter().map(|edit| edit.id.clone()).collect();
        let result = context
            .log
            .commit_change(&context.snapshot, &document, prepared)?;
        let mut lines: Vec<_> = mappings
            .iter()
            .map(|(local, id)| format!("{local:<20} {id}  added"))
            .collect();
        lines.extend(edited.iter().map(|id| format!("{id:<20} edited")));
        return Ok(mutation_output(
            "alder.work.edit.v0",
            &result,
            json!({
                "added": mappings.iter().map(|(local, id)| json!({"local": local, "work_id": id})).collect::<Vec<_>>(),
                "edited": edited,
            }),
            lines.join("\n"),
        ));
    }
    let id = args
        .work
        .clone()
        .ok_or_else(|| AlderError::validation("work edit requires a work ID or --from"))?;
    if !has_actual_edit_fields(args) {
        return Err(AlderError::validation(
            "work edit requires at least one field change",
        ));
    }
    if args.spec.is_some() && args.clear_spec {
        return Err(AlderError::validation(
            "--spec and --clear-spec cannot be combined",
        ));
    }
    let why = args
        .why
        .clone()
        .filter(|why| !why.trim().is_empty())
        .ok_or_else(|| AlderError::validation("work edit requires --why"))?;
    let spec = if args.clear_spec {
        Some(NullableString(None))
    } else {
        args.spec.clone().map(|value| NullableString(Some(value)))
    };
    let document = GraphChangeDocument {
        why: Some(why),
        add: vec![],
        edit: vec![crate::domain::EditWorkInput {
            id: id.clone(),
            title: args.title.clone(),
            spec,
            priority: args.priority,
            add_requires: args.add_requires.clone(),
            remove_requires: args.remove_requires.clone(),
            add_checks: parse_checks(&args.add_check)?,
            remove_checks: args.remove_check.clone(),
        }],
    };
    let prepared = context
        .log
        .allocate_change(&context.snapshot, &document, ChangeMode::Edit)?;
    let result = context
        .log
        .commit_change(&context.snapshot, &document, prepared)?;
    Ok(mutation_output(
        "alder.edit.work.v0",
        &result,
        json!({"added": [], "edited": [id]}),
        format!("{id}  edited"),
    ))
}

fn has_single_edit_fields(args: &WorkEditArgs) -> bool {
    has_actual_edit_fields(args) || args.why.is_some()
}

fn has_actual_edit_fields(args: &WorkEditArgs) -> bool {
    args.title.is_some()
        || args.spec.is_some()
        || args.clear_spec
        || args.priority.is_some()
        || !args.add_requires.is_empty()
        || !args.remove_requires.is_empty()
        || !args.add_check.is_empty()
        || !args.remove_check.is_empty()
}

fn attempt_edit(context: &Context, args: &AttemptEditArgs) -> Result<Output> {
    let metadata = parse_metadata(&args.meta)?;
    if let Some(handle) = args.handle.clone() {
        if handle_edit_has_progress_fields(args) {
            return Err(AlderError::validation(
                "--handle can be combined only with --meta",
            ));
        }
        let result = context
            .log
            .bind_attempt(&args.attempt, handle.clone(), metadata)?;
        return Ok(mutation_output(
            "alder.attempt.edit.v0",
            &result,
            json!({"attempt_id": args.attempt, "change": "bound", "handle": handle}),
            format!("{}  bound {}", args.attempt, handle),
        ));
    }
    let checks = parse_check_results(
        &args.satisfied,
        &args.failed,
        args.evidence.as_deref(),
        args.evidence_file.as_deref(),
    )?;
    let note = read_text_file(
        args.note.as_deref(),
        args.note_file.as_deref(),
        "--note-file",
    )?;
    if args.tier.is_none() && metadata.is_empty() && checks.is_empty() && note.is_none() {
        return Err(AlderError::validation(
            "attempt edit requires a handle, tier, metadata, note, or check result",
        ));
    }
    let result =
        context
            .log
            .update_attempt(&args.attempt, args.tier.clone(), metadata, note, checks)?;
    Ok(mutation_output(
        "alder.attempt.edit.v0",
        &result,
        json!({"attempt_id": args.attempt, "change": "updated"}),
        format!("{}  updated", args.attempt),
    ))
}

fn handle_edit_has_progress_fields(args: &AttemptEditArgs) -> bool {
    args.tier.is_some()
        || !args.satisfied.is_empty()
        || !args.failed.is_empty()
        || args.evidence.is_some()
        || args.evidence_file.is_some()
        || args.note.is_some()
        || args.note_file.is_some()
}

/// The part of `alder status --json` every answer carries: the read envelope,
/// and the loop section filed under the key the driver reads it from. `status`
/// inserts the observation, question, and count sections — and whichever
/// listings were asked for — on top of this.
///
/// Public, and split out of `status`, so a test can drive the real packer over
/// a state it built rather than assembling the document itself. `alderd`'s
/// `LoopState::from_status` looks `loop` and `head` up by name, and a test that
/// writes those keys by hand cannot notice production renaming or dropping
/// either. Neither can a scan of this file: `status` spells `"loop"` a second
/// time for the human rendering, so a search finds a match whichever
/// occurrence went.
pub fn status_document(
    state: &ProjectState,
    head: &Head,
    hypothetical: bool,
    source: Option<&str>,
) -> Value {
    json!({
        "schema": "alder.status.v0",
        "head": head.sequence(),
        "revision": head.revision(),
        "hypothetical": hypothetical,
        "source": source,
        "loop": loop_section(state),
    })
}

/// The active-attempt listing a targeted `alder status --section in_flight`
/// answer carries. It is derived from the current state each time, in the same
/// order as the full status document.
pub fn in_flight_section(state: &ProjectState) -> Value {
    json!(in_flight_attempts(state))
}

fn in_flight_attempts(state: &ProjectState) -> Vec<Attempt> {
    let mut in_flight: Vec<_> = state
        .attempts
        .values()
        .filter(|attempt| {
            matches!(
                attempt.state,
                crate::domain::AttemptState::Starting | crate::domain::AttemptState::Active
            )
        })
        .cloned()
        .collect();
    in_flight.sort_by_key(|attempt| attempt.started_seq);
    in_flight
}

fn status(
    context: &mut Context,
    changes: Option<&str>,
    full: bool,
    sections: &[StatusSection],
) -> Result<Output> {
    let (state, hypothetical, source) = overlay_state(context, changes)?;
    let observations: Vec<_> = state.observations.values().cloned().collect();
    // The log fold, never SQLite, is the current observation picture, and
    // attention derives from it alone: only findings the fold can decide.
    // Not-yet-observed stays quiet — a reader learns absence from an explicit
    // level, never from silence — so kinds needing a local observer run
    // (unspawned, observation_unknown) never appear here.
    let mut findings: Vec<observer::ReconcileFinding> =
        observer::reconcile(&state, &BTreeSet::new(), &BTreeSet::new())
            .into_iter()
            .filter(|finding| matches!(finding.kind.as_str(), "missing" | "orphan" | "finished"))
            .collect();
    // A deferral whose deadline has passed demands review. Nothing unblocks by
    // itself — the fold is a pure function of the log and cannot read a clock —
    // so the expired block surfaces here, where things demanding action live,
    // until someone reviews it and unblocks or re-blocks the item.
    findings.extend(expired_block_findings(&state, chrono::Utc::now()));
    let in_flight = in_flight_attempts(&state);
    let ready: Vec<_> = state.ready().into_iter().cloned().collect();
    let mut all_questions: Vec<_> = state.questions.values().cloned().collect();
    all_questions.sort_by_key(|question| question.asked_seq);
    // Stranded questions are excluded: their work is done or dropped, so
    // nobody is waiting on the answer. They stay visible through `show`.
    let questions: Vec<_> = all_questions
        .iter()
        .filter(|question| question.answer.is_none() && state.stranded(question).is_none())
        .cloned()
        .collect();
    let rendered_questions = all_questions
        .iter()
        .map(|question| question_value(&state, question))
        .collect::<Result<Vec<_>>>()?;
    let answered_blocked: Vec<_> = all_questions
        .iter()
        .filter(|question| {
            question.answer.is_some()
                && state
                    .work
                    .get(&question.work_id)
                    .is_some_and(|work| work.state == crate::domain::WorkState::Blocked)
        })
        .cloned()
        .collect();
    let mut blocked: Vec<_> = state
        .work
        .values()
        .filter(|work| work.state == crate::domain::WorkState::Blocked)
        .cloned()
        .collect();
    blocked.sort_by_key(|work| work.opened_seq);
    let counts = json!({
        "attention": findings.len(),
        "in_flight": in_flight.len(),
        "ready": ready.len(),
        "waiting_on_human": questions.len(),
        "blocked": blocked.len(),
    });
    let mut json = status_document(
        &state,
        &context.snapshot.head,
        hypothetical,
        source.as_deref(),
    );
    let object = json.as_object_mut().expect("status json is an object");
    object.insert("observations".to_owned(), json!({"snapshot": observations}));
    object.insert("questions".to_owned(), json!(rendered_questions));
    object.insert("counts".to_owned(), counts);
    if full {
        object.insert("attention".to_owned(), json!(findings));
        object.insert("in_flight".to_owned(), json!(in_flight));
        object.insert("ready".to_owned(), json!(ready));
        object.insert("waiting_on_human".to_owned(), json!(questions));
        object.insert("blocked".to_owned(), json!(blocked));
        let recent_events: Vec<_> = context
            .snapshot
            .events
            .iter()
            .rev()
            .take(10)
            .rev()
            .map(event_summary)
            .collect();
        object.insert("recent_events".to_owned(), json!(recent_events));
    } else {
        for section in selected_status_sections(sections) {
            let value = match section {
                StatusSection::Attention => json!(findings),
                StatusSection::InFlight => json!(in_flight),
                StatusSection::Ready => json!(ready),
                StatusSection::WaitingOnHuman => json!(questions),
                StatusSection::Blocked => json!(blocked),
            };
            object.insert(section.as_str().to_owned(), value);
        }
    }
    let mut lines = Vec::new();
    if hypothetical {
        lines.push(format!(
            "hypothetical · based on head {} · {} · not written",
            context.snapshot.head.sequence(),
            source.clone().unwrap_or_default()
        ));
    } else {
        lines.push(format!("head {}", context.snapshot.head.sequence()));
    }
    human_section(&mut lines, "loop", loop_lines(&state));
    let attention_lines = || {
        findings.iter().map(|finding| {
            format!(
                "{}  {}",
                finding
                    .attempt_id
                    .as_deref()
                    .or(finding.handle.as_deref())
                    .unwrap_or("-"),
                finding.detail
            )
        })
    };
    let in_flight_lines = || {
        in_flight.iter().map(|attempt| {
            let status = liveness_level(&state, &attempt.id).unwrap_or("unknown");
            format!(
                "{}  {}  {}  {}",
                attempt.work_id,
                attempt.id,
                attempt.handle.as_deref().unwrap_or("unbound"),
                status
            )
        })
    };
    let ready_lines = || {
        ready
            .iter()
            .map(|work| format!("{}  {}  priority {}", work.id, work.title, work.priority))
    };
    let waiting_on_human_lines = || {
        questions
            .iter()
            .map(|question| format!("{}  {}", question.id, question.text))
    };
    let blocked_lines = || {
        blocked.iter().map(|work| {
            let mut line = format!(
                "{}  {}",
                work.id,
                work.block_reason.as_deref().unwrap_or("blocked")
            );
            if let Some(until) = work.block_until {
                line.push_str(&format!(" · until {}", until.to_rfc3339()));
            }
            line
        })
    };
    if full {
        human_section(&mut lines, "attention", attention_lines());
        human_section(&mut lines, "in flight", in_flight_lines());
        human_section(&mut lines, "ready", ready_lines());
        human_section(&mut lines, "waiting on human", waiting_on_human_lines());
        human_section(
            &mut lines,
            "answered questions still blocked",
            answered_blocked.iter().map(|question| {
                format!(
                    "{}  {}  answer: {}",
                    question.id,
                    question.work_id,
                    question.answer.as_deref().unwrap_or_default()
                )
            }),
        );
        human_section(&mut lines, "blocked", blocked_lines());
    } else if !sections.is_empty() {
        for section in selected_status_sections(sections) {
            match section {
                StatusSection::Attention => {
                    human_section(&mut lines, "attention", attention_lines())
                }
                StatusSection::InFlight => {
                    human_section(&mut lines, "in flight", in_flight_lines())
                }
                StatusSection::Ready => human_section(&mut lines, "ready", ready_lines()),
                StatusSection::WaitingOnHuman => {
                    human_section(&mut lines, "waiting on human", waiting_on_human_lines())
                }
                StatusSection::Blocked => human_section(&mut lines, "blocked", blocked_lines()),
            }
        }
    } else {
        human_section(
            &mut lines,
            "counts",
            [
                format!("attention  {}", findings.len()),
                format!("in flight  {}", in_flight.len()),
                format!("ready  {}", ready.len()),
                format!("waiting on human  {}", questions.len()),
                format!("blocked  {}", blocked.len()),
            ],
        );
    }
    Ok(Output::new(json, lines.join("\n")))
}

fn selected_status_sections(
    requested: &[StatusSection],
) -> impl Iterator<Item = StatusSection> + '_ {
    [
        StatusSection::Attention,
        StatusSection::InFlight,
        StatusSection::Ready,
        StatusSection::WaitingOnHuman,
        StatusSection::Blocked,
    ]
    .into_iter()
    .filter(move |section| requested.contains(section))
}

/// The loop's desired state. The driver reads this section — plus the head at
/// the top of the document — and ignores the rest of `status`. It is public so
/// the model checker and the simulator read the loop through the same
/// projection the daemon does. [`status_document`] files this value under the
/// `loop` key, letting simulator tests compare the full production document
/// instead of grepping source literals.
///
/// The section carries only durable statements about the loop: whether it is
/// paused, which engine is desired, the sequence of the latest rotation and
/// nudge requests, and the earliest review deadline any blocked work item
/// carries. The log never mentions its own readers, so "has a request been
/// acted on" is not here — each driver compares the request sequences with the
/// last head it acted on, kept in that driver's machine-local notes.
pub fn loop_section(state: &ProjectState) -> Value {
    let control = &state.loop_control;
    // Every blocked item's deadline, sorted, alongside `review_at` (the
    // earliest, kept for the human status line and existing consumers). The
    // driver checks each one, so an item still blocked past its own deadline
    // does not swallow the wake a later deadline is owed.
    let mut review_deadlines: Vec<DateTime<Utc>> = state
        .work
        .values()
        .filter(|work| work.state == crate::domain::WorkState::Blocked)
        .filter_map(|work| work.block_until)
        .collect();
    review_deadlines.sort_unstable();
    json!({
        "paused": control.paused,
        "pause_reason": control.pause_reason,
        "engine": control.engine,
        "rotate_requested_seq": control.rotate_requested_seq,
        "nudge_requested_seq": control.nudge_requested_seq,
        "review_at": state.next_review_at(),
        "review_deadlines": review_deadlines,
    })
}

/// The attention findings for deferrals whose deadline has passed.
///
/// `now` is a parameter rather than a clock read so the derivation stays a
/// pure function a test can pin: an expired `--until` is
/// "unblocked-pending-review", meaning the item stays blocked in the fold and
/// this finding is what puts the review in front of the executor.
pub fn expired_block_findings(
    state: &ProjectState,
    now: DateTime<Utc>,
) -> Vec<observer::ReconcileFinding> {
    let mut blocked: Vec<_> = state
        .work
        .values()
        .filter(|work| work.state == crate::domain::WorkState::Blocked)
        .filter(|work| work.block_until.is_some_and(|until| until <= now))
        .collect();
    blocked.sort_by_key(|work| work.opened_seq);
    blocked
        .into_iter()
        .map(|work| observer::ReconcileFinding {
            kind: "block_expired".to_owned(),
            attempt_id: None,
            handle: None,
            status: "blocked".to_owned(),
            detail: format!(
                "`{}` was deferred until {} and that time has passed — review it",
                work.id,
                work.block_until.expect("filtered above").to_rfc3339()
            ),
            suggested_command: Some(format!(
                "alder work unblock {} --why \"deferral reviewed: …\"",
                work.id
            )),
            metadata: json!({"work_id": work.id, "until": work.block_until}),
        })
        .collect()
}

fn loop_lines(state: &ProjectState) -> Vec<String> {
    let control = &state.loop_control;
    let mut lines = Vec::new();
    let mut desired = Vec::new();
    if control.paused {
        desired.push(match control.pause_reason.as_deref() {
            Some(reason) => format!("paused · {reason}"),
            None => "paused".to_owned(),
        });
    }
    if let Some(engine) = control.engine.as_deref() {
        desired.push(format!("engine {engine}"));
    }
    if !desired.is_empty() {
        lines.push(desired.join(" · "));
    }
    if let Some(review_at) = state.next_review_at() {
        lines.push(format!("next review {}", review_at.to_rfc3339()));
    }
    lines
}

fn next(context: &mut Context, changes: Option<&str>) -> Result<Output> {
    let (state, hypothetical, source) = overlay_state(context, changes)?;
    let ready: Vec<_> = state.ready().into_iter().cloned().collect();
    let json = json!({
        "schema": "alder.next.v0",
        "head": context.snapshot.head.sequence(),
        "revision": context.snapshot.head.revision(),
        "hypothetical": hypothetical,
        "source": source,
        "work": ready,
    });
    let mut lines = Vec::new();
    if hypothetical {
        lines.push(format!(
            "hypothetical · based on head {} · {} · not written",
            context.snapshot.head.sequence(),
            source.unwrap_or_default()
        ));
    }
    lines.extend(
        ready
            .iter()
            .map(|work| format!("{}  {}  priority {}", work.id, work.title, work.priority)),
    );
    Ok(Output::new(json, lines.join("\n")))
}

fn overlay_state(
    context: &Context,
    changes: Option<&str>,
) -> Result<(ProjectState, bool, Option<String>)> {
    let Some(path) = changes else {
        return Ok((context.snapshot.state.clone(), false, None));
    };
    let document = read_change(path)?;
    let mut next_number = 0usize;
    let prepared = prepare_change(
        &context.snapshot.state,
        &document,
        ChangeMode::Hypothetical,
        |_, local| {
            next_number = next_number.saturating_add(1);
            format!(
                "${}",
                local
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("new-{next_number}"))
            )
        },
    )?;
    let mut state = context.snapshot.state.clone();
    let event = Event {
        id: "hypothetical".to_owned(),
        seq: context.snapshot.head.sequence().saturating_add(1),
        at: chrono::Utc::now(),
        actor: "hypothetical".to_owned(),
        payload: EventPayload::Work(WorkEventPayload::WorkChanged {
            why: document.why,
            operations: prepared.operations,
        }),
        schema: "alder.event.v0".to_owned(),
    };
    state.apply(&event)?;
    Ok((state, true, Some(path.to_owned())))
}

/// The JSON document returned by `alder show`.
///
/// It has no I/O of its own: the state and the complete event history it
/// renders are passed in. That lets consumers which already own an in-memory
/// project state use the same document builder as the CLI rather than keeping
/// a second copy of this agent-facing contract.
pub fn show_document(
    state: &ProjectState,
    events: &[Event],
    head: &Head,
    id: &str,
) -> Result<Value> {
    let (kind, current, related): (&str, Value, BTreeSet<String>) =
        if let Some(value) = state.work.get(id) {
            let related = state
                .attempts
                .values()
                .filter(|attempt| attempt.work_id == id)
                .map(|attempt| attempt.id.clone())
                .chain(
                    state
                        .questions
                        .values()
                        .filter(|question| question.work_id == id)
                        .map(|question| question.id.clone()),
                )
                .chain(std::iter::once(id.to_owned()))
                .collect();
            ("work", serde_json::to_value(value)?, related)
        } else if let Some(value) = state.attempts.get(id) {
            (
                "attempt",
                serde_json::to_value(value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else if let Some(value) = state.questions.get(id) {
            (
                "question",
                question_value(state, value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else {
            return Err(AlderError::not_found("object", id));
        };
    let history: Vec<_> = events
        .iter()
        .filter(|event| {
            related
                .iter()
                .any(|related_id| event.payload.references(related_id))
        })
        .map(event_summary)
        .collect();
    Ok(json!({
        "schema": "alder.show.v0",
        "head": head.sequence(),
        "id": id,
        "kind": kind,
        "current": current,
        "history": history,
    }))
}

fn show(context: &Context, id: &str) -> Result<Output> {
    let document = show_document(
        &context.snapshot.state,
        &context.snapshot.events,
        &context.snapshot.head,
        id,
    )?;
    let current = document["current"].clone();
    let history = document["history"]
        .as_array()
        .expect("show document history is an array")
        .clone();
    Ok(Output::new(
        document,
        format!(
            "{}\n\nhistory\n{}",
            serde_json::to_string_pretty(&current)?,
            history
                .iter()
                .map(|event| format!("  #{}  {}  {}", event["seq"], event["type"], event["at"]))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    ))
}

fn observations(context: &Context) -> Result<Output> {
    let observations: Vec<_> = context.snapshot.state.observations.values().collect();
    let human = if observations.is_empty() {
        "no current observations".to_owned()
    } else {
        observations
            .iter()
            .map(|observation| {
                format!(
                    "{}  {}  {}  {}",
                    observation.key.observer,
                    observation.key.subject,
                    observation.key.field,
                    observation.level,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(Output::new(
        json!({
            "schema": "alder.observations.v0",
            "head": context.snapshot.head.sequence(),
            "revision": context.snapshot.head.revision(),
            "observations": observations,
        }),
        human,
    ))
}

fn observation_mutation(
    result: ObservationAppend,
    schema: &str,
    verb: &str,
    observation: Value,
) -> Result<Output> {
    let subject = observation["subject"].as_str().unwrap_or_default();
    let field = observation["field"].as_str().unwrap_or_default();
    let observer = observation["observer"].as_str().unwrap_or_default();
    let human = format!("{observer} {subject} {field}  {verb}");
    match result {
        ObservationAppend::Appended(result) => Ok(mutation_output(
            schema,
            &result,
            json!({"appended": true, "observation": observation}),
            human,
        )),
        ObservationAppend::Unchanged { head } => Ok(Output::new(
            json!({
                "schema": schema,
                "head": head.sequence(),
                "revision": head.revision(),
                "appended": false,
                "observation": observation,
            }),
            format!("{human} (unchanged)"),
        )),
    }
}

/// The small read envelope alderd's simulator uses to model a refresh. A
/// real refresh now reports whether durable levels were appended; callers
/// that already own an observation result can still pack the stable envelope.
pub fn refresh_document(head: &Head, changed: bool, result: &Value) -> Value {
    json!({
        "schema": "alder.refresh.v0",
        "head": head.sequence(),
        "revision": head.revision(),
        "changed": changed,
        "result": result,
    })
}

fn refresh(context: &Context) -> Result<Output> {
    let result = apply_refresh(context)?;
    let changed = result.appended > 0;
    Ok(Output::new(
        refresh_document(&result.head, changed, &refresh_result_value(&result)),
        if changed {
            format!(
                "recorded {} observation changes ({} retired)",
                result.appended, result.retired
            )
        } else {
            "no observation changes".to_owned()
        },
    ))
}

fn refresh_result_value(result: &RefreshApplication) -> Value {
    let levels = result
        .runs
        .iter()
        .flat_map(|run| run.normalized.iter())
        .filter(|observation| observation.field == "liveness")
        .map(|observation| observation.level.as_str());
    let mut present = 0;
    let mut absent = 0;
    let mut unknown = 0;
    for level in levels {
        match level {
            "present" => present += 1,
            "absent" => absent += 1,
            _ => unknown += 1,
        }
    }
    json!({
        "runs": result.runs,
        "present": present,
        "absent": absent,
        "unknown": unknown,
        "unbound": [],
        "changed": result.appended > 0,
        "appended": result.appended,
        "retired": result.retired,
    })
}

fn apply_refresh(context: &Context) -> Result<RefreshApplication> {
    // The fold supplies each probe observer's targets: live attempts'
    // handles, plus ended attempts' handles whose liveness key is still
    // current — the orphan watch.
    let state = context.log.snapshot()?.state;
    let runs = observer::observe(&context.project.config.observers, &state)?;
    let mut appended = 0;
    let mut retired = 0;
    for run in &runs {
        if !run.success {
            continue;
        }
        let probe = context
            .project
            .config
            .observers
            .iter()
            .any(|observer| observer.observer == run.kind && observer.probe.is_some());
        // The observer subsystem reads open attempts and their handles from
        // the fold and reports per attempt; the planning itself is a pure
        // function shared with the harnesses. Probe answers and list
        // snapshots carry different completeness semantics, so each plans
        // through its own derivation.
        let state = context.log.snapshot()?.state;
        let changes = if probe {
            observer::plan_probe_run(&state, &run.kind, &run.normalized)
        } else {
            observer::plan_observer_run(&state, &run.kind, &run.normalized)
        };
        for change in changes {
            match change.level {
                Some(level) => {
                    if matches!(
                        context.log.report_observation(change.key, level)?,
                        ObservationAppend::Appended(_)
                    ) {
                        appended += 1;
                    }
                }
                None => {
                    if matches!(
                        context.log.retire_observation(change.key)?,
                        ObservationAppend::Appended(_)
                    ) {
                        appended += 1;
                        retired += 1;
                    }
                }
            }
        }
    }
    let snapshot = context.log.snapshot()?;
    Ok(RefreshApplication {
        runs,
        appended,
        retired,
        head: snapshot.head,
    })
}

fn reconcile(context: &Context, refresh_first: bool) -> Result<Output> {
    let refreshed = if refresh_first {
        Some(apply_refresh(context)?)
    } else {
        None
    };
    let snapshot = if refresh_first {
        context.log.snapshot()?
    } else {
        context.snapshot.clone()
    };
    let configured = configured_kinds(context);
    let known: BTreeSet<_> = refreshed
        .as_ref()
        .map(|result| {
            result
                .runs
                .iter()
                .filter(|run| run.success)
                .map(|run| run.kind.clone())
                .collect()
        })
        .unwrap_or_default();
    let findings = observer::reconcile(&snapshot.state, &configured, &known);
    let findings_human = if findings.is_empty() {
        "no reconciliation findings".to_owned()
    } else {
        findings
            .iter()
            .map(|finding| {
                let mut line = format!(
                    "{}  {}",
                    finding
                        .attempt_id
                        .as_deref()
                        .or(finding.handle.as_deref())
                        .unwrap_or("-"),
                    finding.detail
                );
                if let Some(suggestion) = finding.suggested_command.as_deref() {
                    line.push_str(&format!("\n  suggested: {suggestion}"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let human = if refresh_first {
        findings_human
    } else {
        format!("folded observation snapshot\n\n{findings_human}")
    };
    Ok(Output::new(
        json!({
            "schema": "alder.reconcile.v0",
            "head": snapshot.head.sequence(),
            "refreshed": refresh_first,
            "refresh_result": refreshed.as_ref().map(refresh_result_value),
            "observation_runs": [],
            "findings": findings,
        }),
        human,
    ))
}

/// The newest current liveness level recorded about one attempt, whichever
/// observer reported it. The key's subject is the attempt ID, so this is a
/// direct fold lookup with no handle parsing.
fn liveness_level<'a>(state: &'a ProjectState, attempt_id: &str) -> Option<&'a str> {
    state
        .observations
        .values()
        .filter(|observation| {
            observation.key.field == "liveness" && observation.key.subject == attempt_id
        })
        .max_by_key(|observation| observation.reported_seq)
        .map(|observation| observation.level.as_str())
}

fn debug(context: &Context, command: &DebugCommand) -> Result<Output> {
    match command {
        DebugCommand::Log(args) => debug_log(context, &args.command),
        DebugCommand::Db(args) => match args.command {
            DebugDbCommand::Rebuild => {
                context.projection.rebuild(
                    &context.snapshot.head,
                    &context.snapshot.events,
                    &context.snapshot.state,
                )?;
                Ok(Output::new(
                    json!({
                        "schema": "alder.debug.db.v0",
                        "operation": "rebuild",
                        "head": context.snapshot.head.sequence(),
                        "path": context.projection.path(),
                    }),
                    format!(
                        "rebuilt {} at head {}",
                        context.projection.path().display(),
                        context.snapshot.head.sequence()
                    ),
                ))
            }
            DebugDbCommand::Verify => {
                let result = context
                    .projection
                    .verify(&context.snapshot.head, &context.snapshot.state)?;
                Ok(Output::new(
                    json!({"schema": "alder.debug.db.v0", "operation": "verify", "result": result}),
                    format!(
                        "projection valid at head {}",
                        context.snapshot.head.sequence()
                    ),
                ))
            }
        },
        DebugCommand::Query(args) => {
            let result = context.projection.raw_query(&args.sql)?;
            Ok(Output::new(
                json!({"schema": "alder.debug.query.v0", "head": context.snapshot.head.sequence(), "result": result}),
                serde_json::to_string_pretty(&result)?,
            ))
        }
        DebugCommand::Observations(args) => debug_observations(context, args),
    }
}

fn debug_log(context: &Context, command: &DebugLogCommand) -> Result<Output> {
    match command {
        DebugLogCommand::Head => Ok(Output::new(
            json!({
                "schema": "alder.debug.log.v0",
                "operation": "head",
                "head": context.snapshot.head.sequence(),
                "revision": context.snapshot.head.revision(),
            }),
            debug_log_head(context),
        )),
        DebugLogCommand::Tail => {
            let events: Vec<_> = context
                .snapshot
                .events
                .iter()
                .rev()
                .take(10)
                .rev()
                .map(event_summary)
                .collect();
            let mut lines = vec![debug_log_head(context)];
            lines.extend(events.iter().map(|event| {
                format!(
                    "#{}  {}  {}",
                    event["seq"],
                    event["type"].as_str().expect("event types are strings"),
                    event["id"].as_str().expect("event IDs are strings")
                )
            }));
            Ok(Output::new(
                json!({"schema": "alder.debug.log.v0", "operation": "tail", "events": events}),
                lines.join("\n"),
            ))
        }
        DebugLogCommand::Show { seq } => {
            let event = context
                .snapshot
                .events
                .iter()
                .find(|event| event.seq == *seq)
                .ok_or_else(|| AlderError::not_found("event sequence", &seq.to_string()))?;
            Ok(Output::new(
                json!({"schema": "alder.debug.log.v0", "operation": "show", "event": event}),
                serde_json::to_string_pretty(event)?,
            ))
        }
        DebugLogCommand::Verify => {
            ProjectState::fold(&context.snapshot.events)?;
            Ok(Output::new(
                json!({
                    "schema": "alder.debug.log.v0",
                    "operation": "verify",
                    "valid": true,
                    "events": context.snapshot.events.len(),
                    "head": context.snapshot.head.sequence(),
                }),
                format!(
                    "log valid · {} events · head {}",
                    context.snapshot.events.len(),
                    context.snapshot.head.sequence()
                ),
            ))
        }
    }
}

fn debug_log_head(context: &Context) -> String {
    format!(
        "head {}  {}",
        context.snapshot.head.sequence(),
        context.snapshot.head.revision().unwrap_or("empty")
    )
}

fn debug_observations(
    context: &Context,
    args: &crate::cli::DebugObservationsArgs,
) -> Result<Output> {
    if args.run {
        let kind = args
            .kind
            .as_deref()
            .ok_or_else(|| AlderError::validation("--run requires an observation kind"))?;
        let observer_config = context
            .project
            .config
            .observers
            .iter()
            .find(|observer| observer.observer == kind)
            .ok_or_else(|| {
                AlderError::with_context(
                    "observer_unconfigured",
                    format!("observer `{kind}` is not configured"),
                    json!({"kind": kind}),
                )
            })?;
        let result = observer::diagnose(observer_config, &context.snapshot.state)?;
        return Ok(Output::new(
            json!({
                "schema": "alder.debug.observations.v0",
                "kind": kind,
                "configured": true,
                "mode": observer_config.mode(),
                "command": observer_config.command(),
                "shell": "/bin/bash -o pipefail -c",
                "timeout_seconds": 20,
                "max_executions": 4,
                "stored": false,
                "result": result,
            }),
            serde_json::to_string_pretty(&result)?,
        ));
    }
    let observations: Vec<_> = context
        .snapshot
        .state
        .observations
        .values()
        .cloned()
        .collect();
    let configured = configured_kinds(context);
    // A handle is opaque, so nothing is inferred from it; the durably
    // referenced kinds are exactly the observer names in the folded picture.
    let referenced: BTreeSet<_> = observations
        .iter()
        .map(|observation| observation.key.observer.clone())
        .collect();
    let kinds: BTreeSet<_> = configured
        .iter()
        .chain(referenced.iter())
        .cloned()
        .collect();
    let details: Vec<_> = kinds
        .iter()
        .filter(|kind| args.kind.as_ref().is_none_or(|selected| selected == *kind))
        .map(|kind| {
            let observer = context
                .project
                .config
                .observers
                .iter()
                .find(|observer| &observer.observer == kind);
            let objects: Vec<_> = observations
                .iter()
                .filter(|observation| &observation.key.observer == kind)
                .cloned()
                .collect();
            json!({
                "kind": kind,
                "configured": observer.is_some(),
                "mode": observer.map(|observer| observer.mode()),
                "command": observer.map(|observer| observer.command().to_owned()),
                "shell": observer.map(|_| "/bin/bash -o pipefail -c"),
                "timeout_seconds": observer.map(|_| 20),
                "max_executions": observer.map(|_| 4),
                "latest_run": null,
                "objects": objects,
            })
        })
        .collect();
    if args.kind.is_some() && details.is_empty() {
        return Err(AlderError::not_found(
            "observation kind",
            args.kind.as_deref().unwrap_or_default(),
        ));
    }
    Ok(Output::new(
        json!({"schema": "alder.debug.observations.v0", "kinds": details}),
        serde_json::to_string_pretty(&details)?,
    ))
}

fn configured_kinds(context: &Context) -> BTreeSet<String> {
    context
        .project
        .config
        .observers
        .iter()
        .map(|observer| observer.observer.clone())
        .collect()
}

/// The common document envelope emitted by every successful mutation.
///
/// The event and head are facts an append returned; packing them is otherwise
/// pure, which keeps consumers from recreating this stable JSON shape.
pub fn mutation_document(head: &Head, schema: &str, event_id: &str, fields: &Value) -> Value {
    let mut object = match fields {
        Value::Object(object) => object.clone(),
        _ => serde_json::Map::new(),
    };
    object.insert("schema".to_owned(), json!(schema));
    object.insert("head".to_owned(), json!(head.sequence()));
    object.insert("revision".to_owned(), json!(head.revision()));
    object.insert("event_id".to_owned(), json!(event_id));
    Value::Object(object)
}

fn mutation_output(
    schema: &str,
    result: &AppendResult,
    fields: Value,
    human: impl Into<String>,
) -> Output {
    Output::new(
        mutation_document(&result.head, schema, &result.event.id, &fields),
        human,
    )
}

/// A question rendered with its derived visibility. `stranded` is not stored;
/// it is read back out of the work's current state every time, which is what
/// makes `work reopen` restore the question with no repair event.
pub fn question_value(state: &ProjectState, question: &Question) -> Result<Value> {
    let mut value = serde_json::to_value(question)?;
    let stranded = state
        .stranded(question)
        .map(|work_state| format!("work {}", work_state.as_str()));
    if let Value::Object(object) = &mut value {
        object.insert("stranded".to_owned(), json!(stranded));
    }
    Ok(value)
}

/// A terminal transition strands the work's unanswered questions. Naming them
/// in the result puts that consequence in front of the caller at the moment of
/// the decision, rather than leaving it to be noticed later.
fn stranded_note(questions: &[String]) -> String {
    if questions.is_empty() {
        String::new()
    } else {
        format!(" · also strands {}", questions.join(", "))
    }
}

pub fn event_summary(event: &Event) -> Value {
    json!({
        "seq": event.seq,
        "id": event.id,
        "at": event.at,
        "actor": event.actor,
        "type": event.payload.type_name(),
    })
}

fn read_change(source: &str) -> Result<GraphChangeDocument> {
    let bytes = if source == "-" {
        let mut bytes = Vec::new();
        io::stdin().read_to_end(&mut bytes)?;
        bytes
    } else {
        fs::read(source).map_err(|error| {
            AlderError::with_context(
                "input_unavailable",
                format!("cannot read graph change `{source}`: {error}"),
                json!({"path": source}),
            )
        })?
    };
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn parse_checks(values: &[String]) -> Result<Vec<CheckDefinition>> {
    values
        .iter()
        .map(|value| {
            let (key, description) = value.split_once(':').ok_or_else(|| {
                AlderError::validation(format!(
                    "check `{value}` must have the form KEY:DESCRIPTION"
                ))
            })?;
            Ok(CheckDefinition {
                key: key.to_owned(),
                description: description.to_owned(),
            })
        })
        .collect()
}

fn parse_metadata(values: &[String]) -> Result<BTreeMap<String, Value>> {
    let mut metadata = BTreeMap::new();
    for value in values {
        let (key, raw) = value.split_once('=').ok_or_else(|| {
            AlderError::validation(format!("metadata `{value}` must have the form KEY=VALUE"))
        })?;
        if key.trim().is_empty() {
            return Err(AlderError::validation("metadata keys cannot be empty"));
        }
        if metadata.contains_key(key) {
            return Err(AlderError::validation(format!(
                "metadata key `{key}` was provided more than once"
            )));
        }
        let parsed = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
        metadata.insert(key.to_owned(), parsed);
    }
    Ok(metadata)
}

/// A check result names its check and its verdict in the flag itself, so one
/// invocation cannot mix a check key with an unrelated status word.
fn parse_check_results(
    satisfied: &[String],
    failed: &[String],
    evidence: Option<&str>,
    evidence_file: Option<&str>,
) -> Result<Vec<CheckUpdate>> {
    if satisfied.is_empty() && failed.is_empty() {
        if evidence.is_some() {
            return Err(AlderError::validation(
                "--evidence is accepted only with --satisfied or --failed",
            ));
        }
        if evidence_file.is_some() {
            return Err(AlderError::validation(
                "--evidence-file is accepted only with --satisfied or --failed",
            ));
        }
        return Ok(Vec::new());
    }
    let evidence_flag = if evidence_file.is_some() {
        "--evidence-file"
    } else {
        "--evidence"
    };
    let evidence = read_text_file(evidence, evidence_file, "--evidence-file")?
        .filter(|evidence| !evidence.trim().is_empty())
        .ok_or_else(|| {
            AlderError::validation(format!("--satisfied and --failed require {evidence_flag}"))
        })?;
    let mut seen = BTreeSet::new();
    let mut updates = Vec::new();
    for (keys, status) in [
        (satisfied, CheckStatus::Satisfied),
        (failed, CheckStatus::Failed),
    ] {
        for key in keys {
            if key.trim().is_empty() {
                return Err(AlderError::validation("a check key cannot be empty"));
            }
            if !seen.insert(key.clone()) {
                return Err(AlderError::validation(format!(
                    "check `{key}` was given more than one result"
                )));
            }
            updates.push(CheckUpdate {
                key: key.clone(),
                status,
                evidence: evidence.to_owned(),
            });
        }
    }
    Ok(updates)
}

/// File-valued flags use a file only as local input. The durable event gets
/// the text itself, so replay, readers, and repairs never need that path or a
/// shared filesystem.
fn read_text_file(
    inline: Option<&str>,
    source: Option<&str>,
    flag: &str,
) -> Result<Option<String>> {
    match (inline, source) {
        (Some(text), None) => Ok(Some(text.to_owned())),
        (None, Some(path)) => fs::read_to_string(path).map(Some).map_err(|error| {
            AlderError::with_context(
                "input_unavailable",
                format!("cannot read {flag} `{path}`: {error}"),
                json!({"path": path}),
            )
        }),
        (None, None) => Ok(None),
        // Clap rejects this before the application is reached. Keeping the
        // branch explicit makes the locality boundary hold if this API is
        // ever constructed directly in a test or another front end.
        (Some(_), Some(_)) => Err(AlderError::validation(format!(
            "{flag} cannot be combined with its inline form"
        ))),
    }
}

/// Parse an RFC 3339 instant such as `2026-08-04T15:00:00Z`. The stored value
/// is an absolute time, so a reader never has to know when it was written.
fn parse_instant(value: &str) -> Result<DateTime<Utc>> {
    value.trim().parse::<DateTime<Utc>>().map_err(|error| {
        AlderError::with_context(
            "validation_failed",
            format!("`{value}` is not an RFC 3339 instant such as 2026-08-04T15:00:00Z: {error}"),
            json!({"until": value}),
        )
    })
}

fn require_reason(flag: &str, why: Option<&String>) -> Result<()> {
    if why.is_some_and(|why| !why.trim().is_empty()) {
        Ok(())
    } else {
        Err(AlderError::validation(format!("{flag} cannot be empty")))
    }
}

fn human_section<I>(lines: &mut Vec<String>, title: &str, entries: I)
where
    I: IntoIterator<Item = String>,
{
    let entries: Vec<_> = entries.into_iter().collect();
    if entries.is_empty() {
        return;
    }
    lines.push(String::new());
    lines.push(title.to_owned());
    lines.extend(entries.into_iter().map(|entry| format!("  {entry}")));
}

fn format_outcome(outcome: AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Succeeded => "succeeded",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Cancelled => "cancelled",
        AttemptOutcome::Lost => "lost",
        AttemptOutcome::NotStarted => "not-started",
    }
}

impl From<NonSuccessOutcome> for AttemptOutcome {
    fn from(value: NonSuccessOutcome) -> Self {
        match value {
            NonSuccessOutcome::Failed => Self::Failed,
            NonSuccessOutcome::Cancelled => Self::Cancelled,
            NonSuccessOutcome::Lost => Self::Lost,
            NonSuccessOutcome::NotStarted => Self::NotStarted,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::AttemptState;

    use super::*;

    fn blank_add_work_args() -> WorkAddArgs {
        WorkAddArgs {
            from: None,
            title: None,
            spec: None,
            priority: 0,
            requires: Vec::new(),
            check: Vec::new(),
        }
    }

    fn blank_edit_work_args() -> WorkEditArgs {
        WorkEditArgs {
            work: None,
            from: None,
            title: None,
            spec: None,
            clear_spec: false,
            priority: None,
            add_requires: Vec::new(),
            remove_requires: Vec::new(),
            add_check: Vec::new(),
            remove_check: Vec::new(),
            why: None,
        }
    }

    #[test]
    fn bulk_add_rejects_every_direct_work_field() {
        assert!(ensure_no_direct_work_fields(&blank_add_work_args()).is_ok());

        let mut args = blank_add_work_args();
        args.title = Some("title".to_owned());
        assert!(ensure_no_direct_work_fields(&args).is_err());

        let mut args = blank_add_work_args();
        args.spec = Some("spec".to_owned());
        assert!(ensure_no_direct_work_fields(&args).is_err());

        let mut args = blank_add_work_args();
        args.priority = 1;
        assert!(ensure_no_direct_work_fields(&args).is_err());

        let mut args = blank_add_work_args();
        args.requires.push("hm-1".to_owned());
        assert!(ensure_no_direct_work_fields(&args).is_err());

        let mut args = blank_add_work_args();
        args.check.push("test:passes".to_owned());
        assert!(ensure_no_direct_work_fields(&args).is_err());
    }

    #[test]
    fn edit_field_detection_covers_every_field() {
        let blank = blank_edit_work_args();
        assert!(!has_actual_edit_fields(&blank));
        assert!(!has_single_edit_fields(&blank));

        let mut why_only = blank_edit_work_args();
        why_only.why = Some("reason".to_owned());
        assert!(!has_actual_edit_fields(&why_only));
        assert!(has_single_edit_fields(&why_only));

        let mut cases = Vec::new();

        let mut args = blank_edit_work_args();
        args.title = Some("title".to_owned());
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.spec = Some("spec".to_owned());
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.clear_spec = true;
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.priority = Some(1);
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.add_requires.push("hm-1".to_owned());
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.remove_requires.push("hm-1".to_owned());
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.add_check.push("test:passes".to_owned());
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.remove_check.push("test".to_owned());
        cases.push(args);

        for args in cases {
            assert!(has_actual_edit_fields(&args));
            assert!(has_single_edit_fields(&args));
        }
    }

    #[test]
    fn handle_binding_rejects_each_kind_of_progress_update() {
        let mut args = AttemptEditArgs {
            attempt: "hm-1-attempt-1".to_owned(),
            handle: Some("tmux:worker".to_owned()),
            tier: None,
            meta: Vec::new(),
            satisfied: Vec::new(),
            failed: Vec::new(),
            evidence: None,
            evidence_file: None,
            note: None,
            note_file: None,
        };
        assert!(!handle_edit_has_progress_fields(&args));

        for update in [
            |args: &mut AttemptEditArgs| args.satisfied.push("test".to_owned()),
            |args: &mut AttemptEditArgs| args.failed.push("test".to_owned()),
            |args: &mut AttemptEditArgs| args.evidence = Some("proof".to_owned()),
            |args: &mut AttemptEditArgs| args.evidence_file = Some("proof.txt".to_owned()),
            |args: &mut AttemptEditArgs| args.note = Some("working".to_owned()),
            |args: &mut AttemptEditArgs| args.note_file = Some("note.txt".to_owned()),
        ] {
            update(&mut args);
            assert!(handle_edit_has_progress_fields(&args));
            args.satisfied.clear();
            args.failed.clear();
            args.evidence = None;
            args.evidence_file = None;
            args.note = None;
            args.note_file = None;
        }
    }

    #[test]
    fn in_flight_section_contains_only_starting_and_active_attempts() {
        let mut state = ProjectState::default();
        for (id, state_value, started_seq) in [
            ("hm-1-attempt-1", AttemptState::Starting, 2),
            ("hm-1-attempt-2", AttemptState::Active, 3),
            ("hm-1-attempt-3", AttemptState::Ended, 1),
        ] {
            state.attempts.insert(
                id.to_owned(),
                Attempt {
                    tier: None,
                    id: id.to_owned(),
                    work_id: "hm-1".to_owned(),
                    state: state_value,
                    outcome: None,
                    handle: None,
                    metadata: BTreeMap::new(),
                    note: None,
                    started_seq,
                    bound_seq: None,
                    updated_seq: started_seq,
                    ended_seq: None,
                    checks: BTreeMap::new(),
                },
            );
        }

        let section = in_flight_section(&state);
        assert_eq!(
            section
                .as_array()
                .unwrap()
                .iter()
                .map(|attempt| attempt["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["hm-1-attempt-1", "hm-1-attempt-2"]
        );
    }

    #[test]
    fn parsers_preserve_structured_values_and_reject_ambiguous_input() {
        let checks = parse_checks(&["test:the test passes".to_owned()]).unwrap();
        assert_eq!(
            checks,
            vec![CheckDefinition {
                key: "test".to_owned(),
                description: "the test passes".to_owned(),
            }]
        );
        assert!(parse_checks(&["test".to_owned()]).is_err());

        let metadata = parse_metadata(&[
            "count=2".to_owned(),
            "ready=true".to_owned(),
            "label=worker".to_owned(),
        ])
        .unwrap();
        assert_eq!(metadata["count"], json!(2));
        assert_eq!(metadata["ready"], json!(true));
        assert_eq!(metadata["label"], json!("worker"));
        assert!(parse_metadata(&["=value".to_owned()]).is_err());
        assert!(parse_metadata(&["missing-separator".to_owned()]).is_err());
        assert!(parse_metadata(&["key=1".to_owned(), "key=2".to_owned()]).is_err());
    }

    #[test]
    fn check_results_name_their_verdict_and_require_evidence() {
        let none: Vec<String> = Vec::new();
        assert!(
            parse_check_results(&none, &none, None, None)
                .unwrap()
                .is_empty()
        );
        assert!(parse_check_results(&none, &none, Some("proof"), None).is_err());
        assert!(parse_check_results(&["test".to_owned()], &none, None, None).is_err());
        assert!(parse_check_results(&["test".to_owned()], &none, Some(" "), None).is_err());
        assert!(parse_check_results(&[" ".to_owned()], &none, Some("proof"), None).is_err());
        assert!(
            parse_check_results(
                &["test".to_owned()],
                &["test".to_owned()],
                Some("proof"),
                None
            )
            .is_err()
        );
        assert!(
            parse_check_results(
                &["test".to_owned(), "test".to_owned()],
                &none,
                Some("proof"),
                None
            )
            .is_err()
        );

        let updates = parse_check_results(
            &["tests".to_owned()],
            &["review".to_owned()],
            Some("CI 42"),
            None,
        )
        .unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].key, "tests");
        assert_eq!(updates[0].status, CheckStatus::Satisfied);
        assert_eq!(updates[0].evidence, "CI 42");
        assert_eq!(updates[1].key, "review");
        assert_eq!(updates[1].status, CheckStatus::Failed);
    }

    #[test]
    fn review_instants_accept_only_rfc3339_and_reasons_cannot_be_blank() {
        assert_eq!(
            parse_instant("2026-08-04T15:00:00Z").unwrap().to_rfc3339(),
            "2026-08-04T15:00:00+00:00"
        );
        assert_eq!(
            parse_instant(" 2026-08-04T15:00:00+02:00 ")
                .unwrap()
                .to_rfc3339(),
            "2026-08-04T13:00:00+00:00"
        );
        for invalid in ["", "3pm", "20m", "2026-08-04", "2026-08-04 15:00"] {
            assert!(parse_instant(invalid).is_err(), "{invalid}");
        }

        assert!(require_reason("--why", Some(&"reason".to_owned())).is_ok());
        assert!(require_reason("--why", Some(&" ".to_owned())).is_err());
        assert!(require_reason("--why", None).is_err());
    }

    fn blocked_work(id: &str, until: Option<&str>) -> crate::domain::Work {
        crate::domain::Work {
            id: id.to_owned(),
            title: id.to_owned(),
            spec: None,
            priority: 0,
            state: crate::domain::WorkState::Blocked,
            block_reason: Some("deferred".to_owned()),
            block_until: until.map(|value| value.parse().expect("a test instant parses")),
            outcome: None,
            opened_seq: 1,
            changed_seq: 1,
            requires: Vec::new(),
            checks: Vec::new(),
        }
    }

    #[test]
    fn the_loop_section_reports_desired_state_and_the_next_review() {
        let mut state = ProjectState::default();
        let empty = loop_section(&state);
        assert_eq!(empty["paused"], false);
        assert!(empty["engine"].is_null());
        assert!(empty["rotate_requested_seq"].is_null());
        assert!(empty["nudge_requested_seq"].is_null());
        assert!(empty["review_at"].is_null());
        assert_eq!(empty["review_deadlines"], json!([]));
        assert!(loop_lines(&state).is_empty());

        state.loop_control.paused = true;
        state.loop_control.pause_reason = Some("release freeze".to_owned());
        state.loop_control.engine = Some("codex".to_owned());
        state.loop_control.rotate_requested_seq = Some(3);
        state.loop_control.nudge_requested_seq = Some(5);
        let paused = loop_section(&state);
        assert_eq!(paused["pause_reason"], "release freeze");
        assert_eq!(paused["engine"], "codex");
        assert_eq!(paused["rotate_requested_seq"], 3);
        assert_eq!(paused["nudge_requested_seq"], 5);
        assert_eq!(
            loop_lines(&state)[0],
            "paused · release freeze · engine codex"
        );

        // The earliest deferral deadline over all blocked work is the loop's
        // next review rendezvous.
        state.work.insert(
            "hm-a".to_owned(),
            blocked_work("hm-a", Some("2026-08-04T15:00:00Z")),
        );
        state.work.insert(
            "hm-b".to_owned(),
            blocked_work("hm-b", Some("2026-08-04T12:00:00Z")),
        );
        let deferred = loop_section(&state);
        assert_eq!(deferred["review_at"], "2026-08-04T12:00:00Z");
        // Every deadline is served, sorted, not just the earliest.
        assert_eq!(
            deferred["review_deadlines"],
            json!(["2026-08-04T12:00:00Z", "2026-08-04T15:00:00Z"])
        );
        assert_eq!(
            loop_lines(&state)[1],
            "next review 2026-08-04T12:00:00+00:00"
        );
    }

    #[test]
    fn an_expired_deferral_is_an_attention_finding_and_an_unexpired_one_is_not() {
        let mut state = ProjectState::default();
        state.work.insert(
            "hm-a".to_owned(),
            blocked_work("hm-a", Some("2026-08-04T12:00:00Z")),
        );
        state
            .work
            .insert("hm-b".to_owned(), blocked_work("hm-b", None));

        let before: DateTime<Utc> = "2026-08-04T11:59:59Z".parse().unwrap();
        assert!(expired_block_findings(&state, before).is_empty());

        let after: DateTime<Utc> = "2026-08-04T12:00:00Z".parse().unwrap();
        let findings = expired_block_findings(&state, after);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "block_expired");
        assert!(findings[0].detail.contains("hm-a"));
        assert!(
            findings[0]
                .suggested_command
                .as_deref()
                .unwrap()
                .starts_with("alder work unblock hm-a")
        );

        // Unblocking — reviewing — clears the finding; nothing unblocks by
        // itself.
        state.work.get_mut("hm-a").unwrap().state = crate::domain::WorkState::Open;
        state.work.get_mut("hm-a").unwrap().block_until = None;
        assert!(expired_block_findings(&state, after).is_empty());
    }

    #[test]
    fn human_helpers_render_every_domain_outcome() {
        let mut lines = vec!["head 1".to_owned()];
        human_section(&mut lines, "empty", Vec::<String>::new());
        assert_eq!(lines, vec!["head 1"]);
        human_section(
            &mut lines,
            "items",
            ["first".to_owned(), "second".to_owned()],
        );
        assert_eq!(lines, vec!["head 1", "", "items", "  first", "  second"]);

        assert_eq!(format_outcome(AttemptOutcome::Succeeded), "succeeded");
        assert_eq!(format_outcome(AttemptOutcome::Failed), "failed");
        assert_eq!(format_outcome(AttemptOutcome::Cancelled), "cancelled");
        assert_eq!(format_outcome(AttemptOutcome::Lost), "lost");
        assert_eq!(format_outcome(AttemptOutcome::NotStarted), "not-started");

        assert_eq!(
            AttemptOutcome::from(NonSuccessOutcome::Failed),
            AttemptOutcome::Failed
        );
        assert_eq!(
            AttemptOutcome::from(NonSuccessOutcome::Cancelled),
            AttemptOutcome::Cancelled
        );
        assert_eq!(
            AttemptOutcome::from(NonSuccessOutcome::Lost),
            AttemptOutcome::Lost
        );
        assert_eq!(
            AttemptOutcome::from(NonSuccessOutcome::NotStarted),
            AttemptOutcome::NotStarted
        );
    }
}
