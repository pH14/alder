use clap::{Args, Parser, Subcommand, ValueEnum};

/// The grammar has two halves. Queries are global because a reader wants one
/// answer about the project, not one answer per noun. Every mutation names the
/// noun whose ID it takes, so the noun always identifies the argument type.
#[derive(Debug, Parser)]
#[command(name = "alder", version, about)]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create or verify `.alder/config.json`.
    Init(InitArgs),
    /// The default context pack.
    Status(StatusArgs),
    /// Actionable work in priority order.
    Next(OverlayArgs),
    /// Current state and history for any Alder object.
    Show(ShowArgs),
    /// The folded current observation picture.
    Observations,
    /// Run configured observation commands and append changed levels.
    Refresh,
    /// Compare durable attempts with observed reality.
    Reconcile(ReconcileArgs),
    /// Work items: the durable requirements Alder coordinates.
    Work(WorkArgs),
    /// Attempts: one external execution of one work item.
    Attempt(AttemptArgs),
    /// Questions: an asynchronous human decision one work item needs.
    Question(QuestionArgs),
    /// Observations: current external beliefs keyed by observer, subject, and field.
    Observation(ObservationArgs),
    /// The driving loop and its controls.
    Loop(LoopArgs),
    /// Diagnostics kept out of the ordinary workflow.
    Debug(DebugArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long)]
    pub prefix: String,
    #[arg(long, default_value = "origin")]
    pub remote: String,
    #[arg(long = "ref", default_value = "refs/heads/alder")]
    pub reference: String,
}

#[derive(Debug, Args)]
pub struct OverlayArgs {
    #[arg(long = "with", value_name = "CHANGES")]
    pub changes: Option<String>,
}

/// `status` is the index by default: the loop line plus a per-section count.
/// `--full` expands every section back in, `--section` expands requested
/// sections.
#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long = "with", value_name = "CHANGES")]
    pub changes: Option<String>,
    /// Show every section in full, as well as recent events.
    #[arg(long)]
    pub full: bool,
    /// Show one or more sections in full, replacing the counts index.
    #[arg(long, value_enum, action = clap::ArgAction::Append)]
    pub section: Vec<StatusSection>,
}

/// The five sections `status` counts by default. Names match the JSON keys
/// they expand, not the CLI's usual kebab-case, so a caller can round-trip
/// `--section <name>` straight from a `counts` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum StatusSection {
    Attention,
    InFlight,
    Ready,
    WaitingOnHuman,
    Blocked,
}

impl StatusSection {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusSection::Attention => "attention",
            StatusSection::InFlight => "in_flight",
            StatusSection::Ready => "ready",
            StatusSection::WaitingOnHuman => "waiting_on_human",
            StatusSection::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct ReconcileArgs {
    #[arg(long)]
    pub no_refresh: bool,
}

#[derive(Debug, Args)]
pub struct ObservationArgs {
    #[command(subcommand)]
    pub command: ObservationCommand,
}

#[derive(Debug, Subcommand)]
pub enum ObservationCommand {
    /// Report one current level. Repeating it unchanged appends nothing.
    Report(ObservationReportArgs),
    /// Retire a key that no longer exists. Repeating it appends nothing.
    Retire(ObservationRetireArgs),
}

#[derive(Debug, Args)]
pub struct ObservationReportArgs {
    pub observer: String,
    pub subject: String,
    pub field: String,
    pub level: String,
}

#[derive(Debug, Args)]
pub struct ObservationRetireArgs {
    pub observer: String,
    pub subject: String,
    pub field: String,
}

#[derive(Debug, Args)]
pub struct WorkArgs {
    #[command(subcommand)]
    pub command: WorkCommand,
}

#[derive(Debug, Subcommand)]
pub enum WorkCommand {
    /// Admit work, optionally from a graph-change document.
    Add(WorkAddArgs),
    /// Change fields, dependencies, or checks. Never changes state.
    Edit(WorkEditArgs),
    /// Record an attempt before its worker is launched.
    Start(WorkStartArgs),
    /// Complete work through its attempt or with external evidence.
    Finish(WorkFinishArgs),
    /// Drop work, ending its active attempt when it has one.
    Drop(WorkDropArgs),
    /// Return terminal work to open with the same identity.
    Reopen(WorkReasonArgs),
    /// Block work on something outside the Alder graph.
    Block(WorkBlockArgs),
    /// Return blocked work to open.
    Unblock(WorkReasonArgs),
    /// Ask a human decision, blocking the work.
    Ask(WorkAskArgs),
}

#[derive(Debug, Args)]
pub struct WorkAddArgs {
    #[arg(long, value_name = "FILE")]
    pub from: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub spec: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub priority: i64,
    #[arg(long)]
    pub requires: Vec<String>,
    #[arg(long, value_name = "KEY:DESCRIPTION")]
    pub check: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WorkEditArgs {
    pub work: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub from: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub spec: Option<String>,
    #[arg(long)]
    pub clear_spec: bool,
    #[arg(long)]
    pub priority: Option<i64>,
    #[arg(long)]
    pub add_requires: Vec<String>,
    #[arg(long)]
    pub remove_requires: Vec<String>,
    #[arg(long, value_name = "KEY:DESCRIPTION")]
    pub add_check: Vec<String>,
    #[arg(long, value_name = "KEY")]
    pub remove_check: Vec<String>,
    #[arg(long)]
    pub why: Option<String>,
}

#[derive(Debug, Args)]
pub struct WorkStartArgs {
    pub work: String,
    /// The runner's rung name for this attempt. Opaque to Alder: any
    /// non-empty name is legal, and its meaning lives outside the log.
    #[arg(long, value_name = "NAME")]
    pub tier: Option<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
}

#[derive(Debug, Args)]
pub struct WorkFinishArgs {
    pub work: String,
    #[arg(long)]
    pub attempt: Option<String>,
    #[arg(long)]
    pub external: bool,
    #[arg(long)]
    pub evidence: Option<String>,
}

#[derive(Debug, Args)]
pub struct WorkDropArgs {
    pub work: String,
    #[arg(long)]
    pub attempt: Option<String>,
    #[arg(long, value_enum)]
    pub outcome: Option<NonSuccessOutcome>,
    #[arg(long)]
    pub why: String,
}

#[derive(Debug, Args)]
pub struct WorkReasonArgs {
    pub work: String,
    #[arg(long)]
    pub why: String,
}

#[derive(Debug, Args)]
pub struct WorkBlockArgs {
    pub work: String,
    #[arg(long)]
    pub why: String,
    /// Review deadline as an RFC 3339 instant, such as 2026-08-04T15:00:00Z.
    /// Stored on the work item; the driver wakes the leader at that time, and
    /// an expired deadline surfaces in `status` for review — nothing unblocks
    /// by itself.
    #[arg(long, value_name = "RFC3339")]
    pub until: Option<String>,
}

#[derive(Debug, Args)]
pub struct WorkAskArgs {
    pub work: String,
    pub question: String,
}

#[derive(Debug, Args)]
pub struct AttemptArgs {
    #[command(subcommand)]
    pub command: AttemptCommand,
}

#[derive(Debug, Subcommand)]
pub enum AttemptCommand {
    /// Bind a handle, record progress, or record a check result.
    Edit(AttemptEditArgs),
    /// End a non-successful attempt.
    End(AttemptEndArgs),
}

#[derive(Debug, Args)]
pub struct AttemptEditArgs {
    pub attempt: String,
    /// An opaque foreign name for the execution. Alder stores it verbatim
    /// and never parses it.
    #[arg(long, value_name = "HANDLE")]
    pub handle: Option<String>,
    /// The runner's rung name. Opaque to Alder; any non-empty name is legal.
    #[arg(long, value_name = "NAME")]
    pub tier: Option<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
    #[arg(long, value_name = "CHECK")]
    pub satisfied: Vec<String>,
    #[arg(long, value_name = "CHECK")]
    pub failed: Vec<String>,
    #[arg(long, conflicts_with = "evidence_file")]
    pub evidence: Option<String>,
    /// Read check evidence from this local file; its contents, not its path,
    /// are appended to the log.
    #[arg(long, value_name = "PATH", conflicts_with = "evidence")]
    pub evidence_file: Option<String>,
    #[arg(long, conflicts_with = "note_file")]
    pub note: Option<String>,
    /// Read this milestone note from a local file; its contents, not its
    /// path, are appended to the log.
    #[arg(long, value_name = "PATH", conflicts_with = "note")]
    pub note_file: Option<String>,
}

#[derive(Debug, Args)]
pub struct AttemptEndArgs {
    pub attempt: String,
    #[arg(long, value_enum)]
    pub outcome: NonSuccessOutcome,
    #[arg(long)]
    pub why: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum NonSuccessOutcome {
    Failed,
    Cancelled,
    Lost,
    NotStarted,
}

#[derive(Debug, Args)]
pub struct QuestionArgs {
    #[command(subcommand)]
    pub command: QuestionCommand,
}

#[derive(Debug, Subcommand)]
pub enum QuestionCommand {
    /// Record a human decision. Answering never unblocks the work.
    Answer(QuestionAnswerArgs),
}

#[derive(Debug, Args)]
pub struct QuestionAnswerArgs {
    pub question: String,
    pub answer: String,
}

#[derive(Debug, Args)]
pub struct LoopArgs {
    #[command(subcommand)]
    pub command: LoopCommand,
}

#[derive(Debug, Subcommand)]
pub enum LoopCommand {
    /// Ask the driver to stop waking the leader.
    Pause(OptionalReasonArgs),
    /// Ask the driver to resume waking the leader.
    Resume,
    /// Set the desired engine name. Alder never validates it.
    Use(LoopUseArgs),
    /// Ask the next wake to start a fresh session.
    Rotate(OptionalReasonArgs),
    /// Ask the driver to wake the leader now, ahead of any schedule.
    Nudge(OptionalReasonArgs),
}

#[derive(Debug, Args)]
pub struct OptionalReasonArgs {
    #[arg(long)]
    pub why: Option<String>,
}

#[derive(Debug, Args)]
pub struct LoopUseArgs {
    pub engine: String,
}

#[derive(Debug, Args)]
pub struct DebugArgs {
    #[command(subcommand)]
    pub command: DebugCommand,
}

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    Log(DebugLogArgs),
    Db(DebugDbArgs),
    Query(DebugQueryArgs),
    Observations(DebugObservationsArgs),
}

#[derive(Debug, Args)]
pub struct DebugLogArgs {
    #[command(subcommand)]
    pub command: DebugLogCommand,
}

#[derive(Debug, Subcommand)]
pub enum DebugLogCommand {
    Head,
    Tail,
    Show { seq: u64 },
    Verify,
}

#[derive(Debug, Args)]
pub struct DebugDbArgs {
    #[command(subcommand)]
    pub command: DebugDbCommand,
}

#[derive(Debug, Subcommand)]
pub enum DebugDbCommand {
    Rebuild,
    Verify,
}

#[derive(Debug, Args)]
pub struct DebugQueryArgs {
    pub sql: String,
}

#[derive(Debug, Args)]
pub struct DebugObservationsArgs {
    pub kind: Option<String>,
    #[arg(long)]
    pub run: bool,
}
