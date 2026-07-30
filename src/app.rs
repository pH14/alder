use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Read},
};

use chrono::TimeDelta;
use serde_json::{Value, json};

use crate::{
    cli::{
        AttemptCommand, AttemptEditArgs, Command, DebugCommand, DebugDbCommand, DebugLogCommand,
        HandoffCommand, LoopCommand, NonSuccessOutcome, PassCommand, PassOutcomeArg,
        QuestionCommand, StatusSection, TriggerKind, WorkAddArgs, WorkCommand, WorkEditArgs,
    },
    config::{Project, initialize},
    domain::{
        AppendResult, AttemptOutcome, ChangeMode, CheckDefinition, CheckStatus, CheckUpdate, Event,
        EventPayload, GraphChangeDocument, NullableString, PassOutcome, PassTrigger, ProjectLog,
        ProjectState, Question, Snapshot, WorkStateChange, prepare_change,
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
            Command::Handoff(args) => match &args.command {
                HandoffCommand::Add(args) => {
                    let (result, id) = context.log.add_handoff(
                        args.title.clone(),
                        args.artifact_ref.clone(),
                        args.note.clone(),
                    )?;
                    Ok(mutation_output(
                        "alder.handoff.add.v0",
                        &result,
                        json!({"handoff_id": id, "state": "submitted"}),
                        format!("{id}  submitted"),
                    ))
                }
                HandoffCommand::Withdraw(args) => {
                    require_reason("--why", Some(&args.why))?;
                    let result = context
                        .log
                        .withdraw_handoff(&args.handoff, args.why.clone())?;
                    Ok(mutation_output(
                        "alder.handoff.withdraw.v0",
                        &result,
                        json!({"handoff_id": args.handoff, "state": "withdrawn"}),
                        format!("{}  withdrawn", args.handoff),
                    ))
                }
            },
            Command::Loop(args) => loop_command(&context, &args.command),
            Command::Pass(args) => match &args.command {
                PassCommand::End(args) => {
                    let wake_after = args.wake.as_deref().map(parse_duration).transpose()?;
                    let (result, id) = context.log.end_pass(
                        args.pass.as_deref(),
                        args.outcome.into(),
                        args.report.clone(),
                        wake_after,
                        args.rotate,
                        args.why.clone(),
                    )?;
                    let outcome: PassOutcome = args.outcome.into();
                    Ok(mutation_output(
                        "alder.pass.end.v0",
                        &result,
                        json!({
                            "pass_id": id,
                            "outcome": outcome.as_str(),
                            "rotate": args.rotate,
                        }),
                        format!("{id}  ended {}", outcome.as_str()),
                    ))
                }
            },
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
            let (result, id) = context.log.start(&args.work, metadata)?;
            Ok(mutation_output(
                "alder.work.start.v0",
                &result,
                json!({"work_id": args.work, "attempt_id": id}),
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
            let result = context.log.set_work_state(
                &args.work,
                WorkStateChange::Block {
                    reason: args.why.clone(),
                },
            )?;
            Ok(mutation_output(
                "alder.work.block.v0",
                &result,
                json!({"work_id": args.work, "state": "blocked"}),
                format!("{}  blocked", args.work),
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
        LoopCommand::Wake(args) => {
            let mut triggers: Vec<PassTrigger> =
                args.trigger.iter().copied().map(Into::into).collect();
            // A wake with no stated trigger came from a person at a terminal.
            if triggers.is_empty() {
                triggers.push(PassTrigger::Manual);
            }
            triggers.sort();
            triggers.dedup();
            let (result, id) = context.log.wake_loop(
                args.engine.clone(),
                args.handle.clone(),
                triggers.clone(),
            )?;
            Ok(mutation_output(
                "alder.loop.wake.v0",
                &result,
                json!({
                    "pass_id": id,
                    "engine": args.engine,
                    "handle": args.handle,
                    "triggers": trigger_names(&triggers),
                }),
                id,
            ))
        }
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
    if let Some(handoff) = args.handoff.as_deref() {
        let (result, id) = context.log.integrate_handoff(
            handoff,
            args.title.clone(),
            args.spec.clone(),
            args.priority,
            args.requires.clone(),
            checks,
        )?;
        return Ok(mutation_output(
            "alder.work.add.v0",
            &result,
            json!({"work_id": id, "handoff_id": handoff}),
            format!("{id}  integrated from {handoff}"),
        ));
    }
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
        json!({"work_id": id, "handoff_id": null}),
        id,
    ))
}

fn ensure_no_direct_work_fields(args: &WorkAddArgs) -> Result<()> {
    if args.handoff.is_some()
        || args.title.is_some()
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
        if !args.satisfied.is_empty()
            || !args.failed.is_empty()
            || args.evidence.is_some()
            || args.evidence_file.is_some()
            || args.note.is_some()
            || args.note_file.is_some()
        {
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
    if metadata.is_empty() && checks.is_empty() && note.is_none() {
        return Err(AlderError::validation(
            "attempt edit requires a handle, metadata, note, or check result",
        ));
    }
    let result = context
        .log
        .update_attempt(&args.attempt, metadata, note, checks)?;
    Ok(mutation_output(
        "alder.attempt.edit.v0",
        &result,
        json!({"attempt_id": args.attempt, "change": "updated"}),
        format!("{}  updated", args.attempt),
    ))
}

fn status(
    context: &mut Context,
    changes: Option<&str>,
    full: bool,
    sections: &[StatusSection],
) -> Result<Output> {
    let (state, hypothetical, source) = overlay_state(context, changes)?;
    let mut observations = context.projection.observations()?;
    let runs = context.projection.observation_runs()?;
    let configured = configured_kinds(context);
    for observation in &mut observations {
        let kind = observation
            .handle
            .split_once(':')
            .map(|(kind, _)| kind)
            .unwrap_or_default();
        if !configured.contains(kind) {
            observation.status = crate::projection::ObservationStatus::Unknown;
            observation.detail =
                Some("no observation command is configured for this handle kind".to_owned());
        }
    }
    let known: BTreeSet<_> = runs
        .iter()
        .filter(|run| run.success)
        .map(|run| run.kind.clone())
        .collect();
    let findings = observer::reconcile(&state, &observations, &configured, &known);
    let mut handoffs: Vec<_> = state
        .handoffs
        .values()
        .filter(|handoff| handoff.state == crate::domain::HandoffState::Submitted)
        .cloned()
        .collect();
    handoffs.sort_by_key(|handoff| handoff.submitted_seq);
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
    let loop_section = loop_section(&state);
    let counts = json!({
        "attention": findings.len(),
        "handoffs": handoffs.len(),
        "in_flight": in_flight.len(),
        "ready": ready.len(),
        "waiting_on_human": questions.len(),
        "blocked": blocked.len(),
    });
    let mut json = json!({
        "schema": "alder.status.v0",
        "head": context.snapshot.head.sequence(),
        "revision": context.snapshot.head.revision(),
        "hypothetical": hypothetical,
        "source": source,
        "loop": loop_section,
        "observations": {
            "runs": runs,
            "handles": observations,
        },
        "questions": rendered_questions,
        "counts": counts,
    });
    let object = json.as_object_mut().expect("status json is an object");
    if full {
        object.insert("attention".to_owned(), json!(findings));
        object.insert("handoffs".to_owned(), json!(handoffs));
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
                StatusSection::Handoffs => json!(handoffs),
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
    if let Some(latest) = runs.iter().map(|run| run.observed_at.as_str()).max() {
        lines[0].push_str(&format!(" · observations refreshed {latest}"));
    } else if !context.project.config.observers.is_empty() {
        lines[0].push_str(" · observations not refreshed");
    }
    let failures: Vec<_> = runs
        .iter()
        .filter(|run| !run.success)
        .map(|run| run.kind.as_str())
        .collect();
    if !failures.is_empty() {
        lines.push(format!("observation failures: {}", failures.join(", ")));
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
    let handoffs_lines = || {
        handoffs.iter().map(|handoff| {
            format!(
                "{}  {}  {}",
                handoff.id, handoff.title, handoff.artifact_ref
            )
        })
    };
    let in_flight_lines = || {
        in_flight.iter().map(|attempt| {
            let status = attempt
                .handle
                .as_deref()
                .and_then(|handle| observations.iter().find(|item| item.handle == handle))
                .map(|item| item.status.as_str())
                .unwrap_or("unknown");
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
            format!(
                "{}  {}",
                work.id,
                work.block_reason.as_deref().unwrap_or("blocked")
            )
        })
    };
    if full {
        human_section(&mut lines, "attention", attention_lines());
        human_section(&mut lines, "handoffs", handoffs_lines());
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
                StatusSection::Handoffs => human_section(&mut lines, "handoffs", handoffs_lines()),
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
                format!("handoffs  {}", handoffs.len()),
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
        StatusSection::Handoffs,
        StatusSection::InFlight,
        StatusSection::Ready,
        StatusSection::WaitingOnHuman,
        StatusSection::Blocked,
    ]
    .into_iter()
    .filter(move |section| requested.contains(section))
}

/// The loop's desired state and its two interesting passes. The driver reads
/// this section and ignores the rest of `status`. It is public so the model
/// checker and the simulator read the loop through the same projection the
/// daemon does.
pub fn loop_section(state: &ProjectState) -> Value {
    let control = &state.loop_control;
    json!({
        "paused": control.paused,
        "pause_reason": control.pause_reason,
        "engine": control.engine,
        "rotate_pending": control.rotate_pending(),
        "nudge_pending": control.nudge_pending(),
        "open_pass": state.open_pass().map(|pass| json!({
            "id": pass.id,
            "engine": pass.engine,
            "handle": pass.handle,
            "triggers": trigger_names(&pass.triggers),
            "started_at": pass.started_at,
            "at_head": pass.at_head,
        })),
        "last_pass": state.last_ended_pass().map(|pass| json!({
            "id": pass.id,
            "engine": pass.engine,
            "outcome": pass.outcome.map(PassOutcome::as_str),
            "report_line": pass.report_line(),
            "wake_at": pass.wake_at,
            "ended_at": pass.ended_at,
            // The head the log stood at when this pass ended. A reader
            // comparing it with the current head learns whether anything has
            // been appended since, without remembering anything itself.
            "ended_seq": pass.ended_seq,
        })),
    })
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
    if control.rotate_pending() {
        desired.push("rotate pending".to_owned());
    }
    if control.nudge_pending() {
        desired.push("nudge pending".to_owned());
    }
    if !desired.is_empty() {
        lines.push(desired.join(" · "));
    }
    if let Some(pass) = state.open_pass() {
        lines.push(format!(
            "open {}  {}  {}  started {}",
            pass.id,
            pass.engine,
            pass.handle,
            pass.started_at.to_rfc3339()
        ));
    }
    if let Some(pass) = state.last_ended_pass() {
        let mut line = format!(
            "last {}  {}",
            pass.id,
            pass.outcome.map(PassOutcome::as_str).unwrap_or("ended")
        );
        if let Some(report) = pass.report_line() {
            line.push_str(&format!("  {report}"));
        }
        if let Some(wake_at) = pass.wake_at {
            line.push_str(&format!("  wake {}", wake_at.to_rfc3339()));
        }
        lines.push(line);
    }
    lines
}

fn trigger_names(triggers: &[PassTrigger]) -> Vec<&'static str> {
    triggers.iter().copied().map(PassTrigger::as_str).collect()
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
        payload: EventPayload::WorkChanged {
            why: document.why,
            operations: prepared.operations,
        },
        schema: "alder.event.v0".to_owned(),
    };
    state.apply(&event)?;
    Ok((state, true, Some(path.to_owned())))
}

fn show(context: &Context, id: &str) -> Result<Output> {
    let (kind, current, related): (&str, Value, BTreeSet<String>) =
        if let Some(value) = context.snapshot.state.work.get(id) {
            let related = context
                .snapshot
                .state
                .attempts
                .values()
                .filter(|attempt| attempt.work_id == id)
                .map(|attempt| attempt.id.clone())
                .chain(
                    context
                        .snapshot
                        .state
                        .questions
                        .values()
                        .filter(|question| question.work_id == id)
                        .map(|question| question.id.clone()),
                )
                .chain(std::iter::once(id.to_owned()))
                .collect();
            ("work", serde_json::to_value(value)?, related)
        } else if let Some(value) = context.snapshot.state.attempts.get(id) {
            (
                "attempt",
                serde_json::to_value(value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else if let Some(value) = context.snapshot.state.questions.get(id) {
            (
                "question",
                question_value(&context.snapshot.state, value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else if let Some(value) = context.snapshot.state.handoffs.get(id) {
            (
                "handoff",
                serde_json::to_value(value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else if let Some(value) = context.snapshot.state.passes.get(id) {
            (
                "pass",
                serde_json::to_value(value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else {
            return Err(AlderError::not_found("object", id));
        };
    let history: Vec<_> = context
        .snapshot
        .events
        .iter()
        .filter(|event| {
            related
                .iter()
                .any(|related_id| event.payload.references(related_id))
        })
        .map(event_summary)
        .collect();
    Ok(Output::new(
        json!({
            "schema": "alder.show.v0",
            "head": context.snapshot.head.sequence(),
            "id": id,
            "kind": kind,
            "current": current,
            "history": history,
        }),
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

fn refresh(context: &Context) -> Result<Output> {
    let result = observer::refresh(
        &context.projection,
        &context.project.config.observers,
        &context.snapshot.state,
    )?;
    let mut lines = vec![format!(
        "observed {} handles: {} present, {} absent, {} unknown",
        result.present + result.absent + result.unknown,
        result.present,
        result.absent,
        result.unknown
    )];
    if !result.unbound.is_empty() {
        lines.push("unbound:".to_owned());
        lines.extend(
            result
                .unbound
                .iter()
                .map(|handle| format!("  {}  {}", handle.handle, handle.status.as_str())),
        );
    }
    if result.changed {
        lines.push("changed since the previous refresh".to_owned());
    }
    Ok(Output::new(
        json!({
            "schema": "alder.refresh.v0",
            "head": context.snapshot.head.sequence(),
            "changed": result.changed,
            "result": result,
        }),
        lines.join("\n"),
    ))
}

fn reconcile(context: &Context, refresh_first: bool) -> Result<Output> {
    let refreshed = if refresh_first {
        Some(observer::refresh(
            &context.projection,
            &context.project.config.observers,
            &context.snapshot.state,
        )?)
    } else {
        None
    };
    let observations = context.projection.observations()?;
    let runs = context.projection.observation_runs()?;
    let configured = configured_kinds(context);
    let known: BTreeSet<_> = runs
        .iter()
        .filter(|run| run.success)
        .map(|run| run.kind.clone())
        .collect();
    let findings = observer::reconcile(&context.snapshot.state, &observations, &configured, &known);
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
    let freshness = runs
        .iter()
        .map(|run| run.observed_at.as_str())
        .max()
        .map(|at| format!("observations from {at}"))
        .unwrap_or_else(|| "no stored observations".to_owned());
    let human = if refresh_first {
        findings_human
    } else {
        format!("{freshness}\n\n{findings_human}")
    };
    Ok(Output::new(
        json!({
            "schema": "alder.reconcile.v0",
            "head": context.snapshot.head.sequence(),
            "refreshed": refresh_first,
            "refresh_result": refreshed,
            "observation_runs": runs,
            "findings": findings,
        }),
        human,
    ))
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
        let result = observer::diagnose(observer_config)?;
        return Ok(Output::new(
            json!({
                "schema": "alder.debug.observations.v0",
                "kind": kind,
                "configured": true,
                "command": observer_config.list,
                "shell": "/bin/bash -o pipefail -c",
                "timeout_seconds": 20,
                "max_executions": 4,
                "stored": false,
                "result": result,
            }),
            serde_json::to_string_pretty(&result)?,
        ));
    }
    let observations = context.projection.observations()?;
    let runs = context.projection.observation_runs()?;
    let configured = configured_kinds(context);
    let referenced: BTreeSet<_> = context
        .snapshot
        .state
        .attempts
        .values()
        .filter_map(|attempt| attempt.handle.as_deref())
        .filter_map(|handle| handle.split_once(':').map(|(kind, _)| kind.to_owned()))
        .collect();
    let kinds: BTreeSet<_> = configured
        .iter()
        .chain(referenced.iter())
        .chain(runs.iter().map(|run| &run.kind))
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
                .filter(|handle| handle.handle.starts_with(&format!("{kind}:")))
                .cloned()
                .collect();
            json!({
                "kind": kind,
                "configured": observer.is_some(),
                "command": observer.map(|observer| observer.list.clone()),
                "shell": observer.map(|_| "/bin/bash -o pipefail -c"),
                "timeout_seconds": observer.map(|_| 20),
                "max_executions": observer.map(|_| 4),
                "latest_run": runs.iter().find(|run| &run.kind == kind),
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

fn mutation_output(
    schema: &str,
    result: &AppendResult,
    fields: Value,
    human: impl Into<String>,
) -> Output {
    let mut object = match fields {
        Value::Object(object) => object,
        _ => serde_json::Map::new(),
    };
    object.insert("schema".to_owned(), json!(schema));
    object.insert("head".to_owned(), json!(result.head.sequence()));
    object.insert("revision".to_owned(), json!(result.head.revision()));
    object.insert("event_id".to_owned(), json!(result.event.id));
    Output::new(Value::Object(object), human)
}

/// A question rendered with its derived visibility. `stranded` is not stored;
/// it is read back out of the work's current state every time, which is what
/// makes `work reopen` restore the question with no repair event.
fn question_value(state: &ProjectState, question: &Question) -> Result<Value> {
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

fn event_summary(event: &Event) -> Value {
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

/// Parse a wake delay such as `270s`, `20m`, `1h`, or `2d`. The stored value is
/// an absolute time, so a reader never has to know when the pass ended.
fn parse_duration(value: &str) -> Result<TimeDelta> {
    let trimmed = value.trim();
    let invalid = || {
        AlderError::with_context(
            "validation_failed",
            format!("duration `{value}` must look like 270s, 20m, 1h, or 2d"),
            json!({"duration": value}),
        )
    };
    let (digits, unit) = trimmed.split_at(trimmed.len().saturating_sub(1));
    let amount: i64 = digits.parse().map_err(|_| invalid())?;
    if amount <= 0 {
        return Err(invalid());
    }
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(invalid()),
    };
    amount
        .checked_mul(seconds)
        .map(TimeDelta::seconds)
        .ok_or_else(invalid)
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

impl From<PassOutcomeArg> for PassOutcome {
    fn from(value: PassOutcomeArg) -> Self {
        match value {
            PassOutcomeArg::Ok => Self::Ok,
            PassOutcomeArg::Crashed => Self::Crashed,
            PassOutcomeArg::Timeout => Self::Timeout,
        }
    }
}

impl From<TriggerKind> for PassTrigger {
    fn from(value: TriggerKind) -> Self {
        match value {
            TriggerKind::Log => Self::Log,
            TriggerKind::Observations => Self::Observations,
            TriggerKind::Due => Self::Due,
            TriggerKind::Manual => Self::Manual,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{Pass, PassState};

    use super::*;

    fn blank_add_work_args() -> WorkAddArgs {
        WorkAddArgs {
            from: None,
            handoff: None,
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
        args.handoff = Some("hm-handoff-1".to_owned());
        assert!(ensure_no_direct_work_fields(&args).is_err());

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
    fn wake_durations_accept_only_a_positive_amount_and_known_unit() {
        assert_eq!(parse_duration("270s").unwrap(), TimeDelta::seconds(270));
        assert_eq!(parse_duration("20m").unwrap(), TimeDelta::minutes(20));
        assert_eq!(parse_duration(" 1h ").unwrap(), TimeDelta::hours(1));
        assert_eq!(parse_duration("2d").unwrap(), TimeDelta::days(2));
        for invalid in [
            "",
            "m",
            "20",
            "0m",
            "-5m",
            "20w",
            "1.5h",
            "9223372036854775807d",
        ] {
            assert!(parse_duration(invalid).is_err(), "{invalid}");
        }

        assert!(require_reason("--why", Some(&"reason".to_owned())).is_ok());
        assert!(require_reason("--why", Some(&" ".to_owned())).is_err());
        assert!(require_reason("--why", None).is_err());
    }

    #[test]
    fn the_loop_section_reports_desired_state_and_both_interesting_passes() {
        let mut state = ProjectState::default();
        let empty = loop_section(&state);
        assert_eq!(empty["paused"], false);
        assert!(empty["engine"].is_null());
        assert!(empty["open_pass"].is_null());
        assert!(empty["last_pass"].is_null());
        assert!(loop_lines(&state).is_empty());

        state.loop_control.paused = true;
        state.loop_control.pause_reason = Some("release freeze".to_owned());
        state.loop_control.engine = Some("codex".to_owned());
        state.loop_control.rotate_requested_seq = Some(3);
        let paused = loop_section(&state);
        assert_eq!(paused["pause_reason"], "release freeze");
        assert_eq!(paused["engine"], "codex");
        assert_eq!(paused["rotate_pending"], true);
        assert_eq!(
            loop_lines(&state)[0],
            "paused · release freeze · engine codex · rotate pending"
        );

        let at = chrono::Utc::now();
        state.passes.insert(
            "hm-pass-1".to_owned(),
            Pass {
                id: "hm-pass-1".to_owned(),
                engine: "claude".to_owned(),
                handle: "tmux:alder-leader".to_owned(),
                triggers: vec![PassTrigger::Log],
                state: PassState::Ended,
                outcome: Some(PassOutcome::Ok),
                report: Some("swept the frontier\nand more".to_owned()),
                wake_at: Some(at),
                rotate: false,
                why: None,
                at_head: 4,
                started_at: at,
                started_seq: 5,
                ended_at: Some(at),
                ended_seq: Some(6),
            },
        );
        let ended = loop_section(&state);
        assert_eq!(ended["last_pass"]["id"], "hm-pass-1");
        assert_eq!(ended["last_pass"]["outcome"], "ok");
        assert_eq!(ended["last_pass"]["report_line"], "swept the frontier");
        assert_eq!(ended["last_pass"]["ended_seq"], 6);
        let lines = loop_lines(&state);
        assert!(lines[1].starts_with("last hm-pass-1  ok  swept the frontier  wake "));

        let mut open = state.passes["hm-pass-1"].clone();
        open.id = "hm-pass-2".to_owned();
        open.state = PassState::Open;
        open.outcome = None;
        open.ended_seq = None;
        state.passes.insert(open.id.clone(), open);
        let both = loop_section(&state);
        assert_eq!(both["open_pass"]["id"], "hm-pass-2");
        assert_eq!(both["open_pass"]["triggers"], json!(["log"]));
        assert_eq!(both["last_pass"]["id"], "hm-pass-1");
        assert!(loop_lines(&state)[1].starts_with("open hm-pass-2  claude  tmux:alder-leader"));
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
