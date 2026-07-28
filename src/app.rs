use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, Read},
};

use serde_json::{Value, json};

use crate::{
    cli::{
        AddResource, AddWorkArgs, Command, DebugCommand, DebugDbCommand, DebugLogCommand,
        EditAttemptArgs, EditResource, EditWorkArgs, NonSuccessOutcome,
    },
    config::{Project, initialize},
    domain::{
        AppendResult, AttemptOutcome, ChangeMode, CheckDefinition, CheckStatus, CheckUpdate, Event,
        EventPayload, GraphChangeDocument, Ledger, NullableString, ProjectState, Snapshot,
        prepare_change,
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
    ledger: Ledger<GitLog>,
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
            Command::Status(args) => status(&mut context, args.changes.as_deref()),
            Command::Next(args) => next(&mut context, args.changes.as_deref()),
            Command::Show(args) => show(&context, &args.id),
            Command::Add(args) => match &args.resource {
                AddResource::Work(args) => add_work(&context, args),
                AddResource::Handoff(args) => {
                    let (result, id) = context.ledger.add_handoff(
                        args.title.clone(),
                        args.artifact_ref.clone(),
                        args.note.clone(),
                    )?;
                    Ok(mutation_output(
                        "alder.add.handoff.v0",
                        &result,
                        json!({"handoff_id": id, "state": "submitted"}),
                        format!("{id}  submitted"),
                    ))
                }
            },
            Command::Edit(args) => match &args.resource {
                EditResource::Work(args) => edit_work(&context, args),
                EditResource::Attempt(args) => edit_attempt(&context, args),
            },
            Command::Reopen(args) => {
                let result = context.ledger.reopen(&args.work, args.why.clone())?;
                Ok(mutation_output(
                    "alder.reopen.v0",
                    &result,
                    json!({"work_id": args.work}),
                    format!("{}  reopened", args.work),
                ))
            }
            Command::Start(args) => {
                let metadata = parse_metadata(&args.meta)?;
                let (result, id) = context.ledger.start(&args.work, metadata)?;
                Ok(mutation_output(
                    "alder.start.v0",
                    &result,
                    json!({"work_id": args.work, "attempt_id": id}),
                    id,
                ))
            }
            Command::Finish(args) => {
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
                let result = context.ledger.finish(
                    &args.work,
                    args.attempt.clone(),
                    args.external,
                    args.evidence.clone(),
                )?;
                Ok(mutation_output(
                    "alder.finish.v0",
                    &result,
                    json!({
                        "work_id": args.work,
                        "attempt_id": args.attempt,
                        "external": args.external,
                    }),
                    format!("{}  done", args.work),
                ))
            }
            Command::Drop(args) => {
                let result = context.ledger.drop_work(
                    &args.work,
                    args.attempt.clone(),
                    args.outcome.map(Into::into),
                    args.why.clone(),
                )?;
                let downstream = context.snapshot.state.downstream(&args.work);
                Ok(mutation_output(
                    "alder.drop.v0",
                    &result,
                    json!({
                        "work_id": args.work,
                        "attempt_id": args.attempt,
                        "outcome": args.outcome.map(|outcome| format_outcome(outcome.into())),
                        "affected_downstream": downstream,
                    }),
                    format!(
                        "{}  dropped{}",
                        args.work,
                        if downstream.is_empty() {
                            String::new()
                        } else {
                            format!(" · affects {}", downstream.join(", "))
                        }
                    ),
                ))
            }
            Command::Ask(args) => {
                let (result, id) = context.ledger.ask(&args.work, args.question.clone())?;
                Ok(mutation_output(
                    "alder.ask.v0",
                    &result,
                    json!({"work_id": args.work, "question_id": id}),
                    id,
                ))
            }
            Command::Answer(args) => {
                let result = context.ledger.answer(&args.question, args.answer.clone())?;
                Ok(mutation_output(
                    "alder.answer.v0",
                    &result,
                    json!({"question_id": args.question}),
                    format!("{}  answered", args.question),
                ))
            }
            Command::Refresh => refresh(&context),
            Command::Reconcile(args) => reconcile(&context, !args.no_refresh),
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
    let ledger = Ledger::new(project.store(), &project.config.prefix, actor);
    let snapshot = ledger.snapshot()?;
    let projection = Projection::new(project.state_db());
    projection.sync(&snapshot.head, &snapshot.events, &snapshot.state)?;
    Ok(Context {
        project,
        ledger,
        projection,
        snapshot,
    })
}

fn actor() -> String {
    env::var("ALDER_ACTOR")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn add_work(context: &Context, args: &AddWorkArgs) -> Result<Output> {
    if let Some(path) = args.from.as_deref() {
        ensure_no_direct_work_fields(args)?;
        let document = read_change(path)?;
        let prepared =
            context
                .ledger
                .allocate_change(&context.snapshot, &document, ChangeMode::AddOnly)?;
        let mappings = prepared.mappings.clone();
        let result = context
            .ledger
            .commit_change(&context.snapshot, &document, prepared)?;
        let human = mappings
            .iter()
            .map(|(local, id)| format!("{local:<20} {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(mutation_output(
            "alder.add.work.v0",
            &result,
            json!({"work": mappings.iter().map(|(local, id)| json!({"local": local, "work_id": id})).collect::<Vec<_>>()}),
            human,
        ));
    }
    let checks = parse_checks(&args.check)?;
    if let Some(handoff) = args.from_handoff.as_deref() {
        let (result, id) = context.ledger.integrate_handoff(
            handoff,
            args.title.clone(),
            args.spec.clone(),
            args.priority,
            args.requires.clone(),
            checks,
        )?;
        return Ok(mutation_output(
            "alder.add.work.v0",
            &result,
            json!({"work_id": id, "handoff_id": handoff}),
            format!("{id}  integrated from {handoff}"),
        ));
    }
    let title = args
        .title
        .clone()
        .ok_or_else(|| AlderError::validation("add work requires --title"))?;
    let (result, id) = context.ledger.add_work(
        title,
        args.spec.clone(),
        args.priority,
        args.requires.clone(),
        checks,
    )?;
    Ok(mutation_output(
        "alder.add.work.v0",
        &result,
        json!({"work_id": id, "handoff_id": null}),
        id,
    ))
}

fn ensure_no_direct_work_fields(args: &AddWorkArgs) -> Result<()> {
    if args.from_handoff.is_some()
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

fn edit_work(context: &Context, args: &EditWorkArgs) -> Result<Output> {
    if let Some(path) = args.from.as_deref() {
        if args.work.is_some() || has_single_edit_fields(args) {
            return Err(AlderError::validation(
                "--from cannot be combined with a work ID or direct edit fields",
            ));
        }
        let document = read_change(path)?;
        let prepared =
            context
                .ledger
                .allocate_change(&context.snapshot, &document, ChangeMode::Edit)?;
        let mappings = prepared.mappings.clone();
        let edited: Vec<_> = document.edit.iter().map(|edit| edit.id.clone()).collect();
        let result = context
            .ledger
            .commit_change(&context.snapshot, &document, prepared)?;
        let mut lines: Vec<_> = mappings
            .iter()
            .map(|(local, id)| format!("{local:<20} {id}  added"))
            .collect();
        lines.extend(edited.iter().map(|id| format!("{id:<20} edited")));
        return Ok(mutation_output(
            "alder.edit.work.v0",
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
        .ok_or_else(|| AlderError::validation("edit work requires a work ID or --from"))?;
    if !has_actual_edit_fields(args) {
        return Err(AlderError::validation(
            "edit work requires at least one field change",
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
        .ok_or_else(|| AlderError::validation("edit work requires --why"))?;
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
            block: args.block,
            unblock: args.unblock,
        }],
    };
    let prepared =
        context
            .ledger
            .allocate_change(&context.snapshot, &document, ChangeMode::Edit)?;
    let result = context
        .ledger
        .commit_change(&context.snapshot, &document, prepared)?;
    Ok(mutation_output(
        "alder.edit.work.v0",
        &result,
        json!({"added": [], "edited": [id]}),
        format!("{id}  edited"),
    ))
}

fn has_single_edit_fields(args: &EditWorkArgs) -> bool {
    has_actual_edit_fields(args) || args.why.is_some()
}

fn has_actual_edit_fields(args: &EditWorkArgs) -> bool {
    args.title.is_some()
        || args.spec.is_some()
        || args.clear_spec
        || args.priority.is_some()
        || !args.add_requires.is_empty()
        || !args.remove_requires.is_empty()
        || !args.add_check.is_empty()
        || !args.remove_check.is_empty()
        || args.block
        || args.unblock
}

fn edit_attempt(context: &Context, args: &EditAttemptArgs) -> Result<Output> {
    let metadata = parse_metadata(&args.meta)?;
    if let Some(outcome) = args.end {
        if args.handle.is_some()
            || !metadata.is_empty()
            || !args.check.is_empty()
            || args.evidence.is_some()
            || args.note.is_some()
        {
            return Err(AlderError::validation(
                "--end cannot be combined with progress or binding fields",
            ));
        }
        let why = args
            .why
            .clone()
            .filter(|why| !why.trim().is_empty())
            .ok_or_else(|| AlderError::validation("--end requires --why"))?;
        let result = context
            .ledger
            .end_attempt(&args.attempt, outcome.into(), why)?;
        return Ok(mutation_output(
            "alder.edit.attempt.v0",
            &result,
            json!({"attempt_id": args.attempt, "change": "ended", "outcome": format_outcome(outcome.into())}),
            format!("{}  ended {}", args.attempt, format_outcome(outcome.into())),
        ));
    }
    if args.why.is_some() {
        return Err(AlderError::validation("--why is accepted only with --end"));
    }
    if let Some(handle) = args.handle.clone() {
        if !args.check.is_empty() || args.evidence.is_some() || args.note.is_some() {
            return Err(AlderError::validation(
                "--handle can be combined only with --meta",
            ));
        }
        let result = context
            .ledger
            .bind_attempt(&args.attempt, handle.clone(), metadata)?;
        return Ok(mutation_output(
            "alder.edit.attempt.v0",
            &result,
            json!({"attempt_id": args.attempt, "change": "bound", "handle": handle}),
            format!("{}  bound {}", args.attempt, handle),
        ));
    }
    let checks = parse_check_updates(&args.check, args.evidence.as_deref())?;
    if metadata.is_empty() && checks.is_empty() && args.note.is_none() {
        return Err(AlderError::validation(
            "edit attempt requires a handle, metadata, note, check, or end outcome",
        ));
    }
    let result =
        context
            .ledger
            .update_attempt(&args.attempt, metadata, args.note.clone(), checks)?;
    Ok(mutation_output(
        "alder.edit.attempt.v0",
        &result,
        json!({"attempt_id": args.attempt, "change": "updated"}),
        format!("{}  updated", args.attempt),
    ))
}

fn status(context: &mut Context, changes: Option<&str>) -> Result<Output> {
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
    let questions: Vec<_> = all_questions
        .iter()
        .filter(|question| question.answer.is_none())
        .cloned()
        .collect();
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
    let recent_events: Vec<_> = context
        .snapshot
        .events
        .iter()
        .rev()
        .take(10)
        .rev()
        .map(event_summary)
        .collect();
    let json = json!({
        "schema": "alder.status.v0",
        "head": context.snapshot.head.sequence(),
        "revision": context.snapshot.head.revision(),
        "hypothetical": hypothetical,
        "source": source,
        "observations": {
            "runs": runs,
            "handles": observations,
        },
        "attention": findings,
        "handoffs": handoffs,
        "in_flight": in_flight,
        "ready": ready,
        "waiting_on_human": questions,
        "questions": all_questions,
        "blocked": blocked,
        "recent_events": recent_events,
    });
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
    human_section(
        &mut lines,
        "attention",
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
        }),
    );
    human_section(
        &mut lines,
        "handoffs",
        handoffs.iter().map(|handoff| {
            format!(
                "{}  {}  {}",
                handoff.id, handoff.title, handoff.artifact_ref
            )
        }),
    );
    human_section(
        &mut lines,
        "in flight",
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
        }),
    );
    human_section(
        &mut lines,
        "ready",
        ready
            .iter()
            .map(|work| format!("{}  {}  priority {}", work.id, work.title, work.priority)),
    );
    human_section(
        &mut lines,
        "waiting on human",
        questions
            .iter()
            .map(|question| format!("{}  {}", question.id, question.text)),
    );
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
    human_section(
        &mut lines,
        "blocked",
        blocked.iter().map(|work| {
            format!(
                "{}  {}",
                work.id,
                work.block_reason.as_deref().unwrap_or("blocked")
            )
        }),
    );
    Ok(Output::new(json, lines.join("\n")))
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
                serde_json::to_value(value)?,
                BTreeSet::from([id.to_owned()]),
            )
        } else if let Some(value) = context.snapshot.state.handoffs.get(id) {
            (
                "handoff",
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
    Ok(Output::new(
        json!({
            "schema": "alder.refresh.v0",
            "head": context.snapshot.head.sequence(),
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

fn parse_check_updates(values: &[String], evidence: Option<&str>) -> Result<Vec<CheckUpdate>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }
    let evidence = evidence
        .filter(|evidence| !evidence.trim().is_empty())
        .ok_or_else(|| AlderError::validation("--check requires --evidence"))?;
    values
        .iter()
        .map(|value| {
            let (key, status) = value.split_once('=').ok_or_else(|| {
                AlderError::validation(format!("check `{value}` must have the form KEY=STATUS"))
            })?;
            let status = match status {
                "pending" => CheckStatus::Pending,
                "satisfied" => CheckStatus::Satisfied,
                "failed" => CheckStatus::Failed,
                _ => {
                    return Err(AlderError::validation(format!(
                        "unknown check status `{status}`"
                    )));
                }
            };
            Ok(CheckUpdate {
                key: key.to_owned(),
                status,
                evidence: evidence.to_owned(),
            })
        })
        .collect()
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
    use super::*;

    fn blank_add_work_args() -> AddWorkArgs {
        AddWorkArgs {
            from: None,
            from_handoff: None,
            title: None,
            spec: None,
            priority: 0,
            requires: Vec::new(),
            check: Vec::new(),
        }
    }

    fn blank_edit_work_args() -> EditWorkArgs {
        EditWorkArgs {
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
            block: false,
            unblock: false,
            why: None,
        }
    }

    #[test]
    fn bulk_add_rejects_every_direct_work_field() {
        assert!(ensure_no_direct_work_fields(&blank_add_work_args()).is_ok());

        let mut args = blank_add_work_args();
        args.from_handoff = Some("hm-handoff-1".to_owned());
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

        let mut args = blank_edit_work_args();
        args.block = true;
        cases.push(args);

        let mut args = blank_edit_work_args();
        args.unblock = true;
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
    fn check_updates_cover_every_status_and_require_evidence() {
        assert!(parse_check_updates(&[], None).unwrap().is_empty());
        assert!(parse_check_updates(&["test=pending".to_owned()], None).is_err());
        assert!(parse_check_updates(&["test=pending".to_owned()], Some(" ")).is_err());
        assert!(parse_check_updates(&["malformed".to_owned()], Some("proof")).is_err());
        assert!(parse_check_updates(&["test=unknown".to_owned()], Some("proof")).is_err());

        let updates = parse_check_updates(
            &[
                "one=pending".to_owned(),
                "two=satisfied".to_owned(),
                "three=failed".to_owned(),
            ],
            Some("proof"),
        )
        .unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0].key, "one");
        assert_eq!(updates[0].status, CheckStatus::Pending);
        assert_eq!(updates[0].evidence, "proof");
        assert_eq!(updates[1].status, CheckStatus::Satisfied);
        assert_eq!(updates[2].status, CheckStatus::Failed);
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
