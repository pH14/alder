use clap::{Args, Parser, Subcommand, ValueEnum};

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
    Init(InitArgs),
    Status(OverlayArgs),
    Next(OverlayArgs),
    Show(ShowArgs),
    Add(AddArgs),
    Edit(EditArgs),
    Reopen(ReopenArgs),
    Start(StartArgs),
    Finish(FinishArgs),
    Drop(DropArgs),
    Ask(AskArgs),
    Answer(AnswerArgs),
    Refresh,
    Reconcile(ReconcileArgs),
    Debug(DebugArgs),
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Status(_) => "status",
            Self::Next(_) => "next",
            Self::Show(_) => "show",
            Self::Add(_) => "add",
            Self::Edit(_) => "edit",
            Self::Reopen(_) => "reopen",
            Self::Start(_) => "start",
            Self::Finish(_) => "finish",
            Self::Drop(_) => "drop",
            Self::Ask(_) => "ask",
            Self::Answer(_) => "answer",
            Self::Refresh => "refresh",
            Self::Reconcile(_) => "reconcile",
            Self::Debug(_) => "debug",
        }
    }
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

#[derive(Debug, Args)]
pub struct ShowArgs {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    #[command(subcommand)]
    pub resource: AddResource,
}

#[derive(Debug, Subcommand)]
pub enum AddResource {
    Work(AddWorkArgs),
    Handoff(AddHandoffArgs),
}

#[derive(Debug, Args)]
pub struct AddWorkArgs {
    #[arg(long, value_name = "FILE")]
    pub from: Option<String>,
    #[arg(long, value_name = "HANDOFF")]
    pub from_handoff: Option<String>,
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
pub struct AddHandoffArgs {
    #[arg(long)]
    pub title: String,
    #[arg(long = "ref")]
    pub artifact_ref: String,
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Debug, Args)]
pub struct EditArgs {
    #[command(subcommand)]
    pub resource: EditResource,
}

#[derive(Debug, Subcommand)]
pub enum EditResource {
    Work(EditWorkArgs),
    Attempt(EditAttemptArgs),
}

#[derive(Debug, Args)]
pub struct EditWorkArgs {
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
    #[arg(long, conflicts_with = "unblock")]
    pub block: bool,
    #[arg(long, conflicts_with = "block")]
    pub unblock: bool,
    #[arg(long)]
    pub why: Option<String>,
}

#[derive(Debug, Args)]
pub struct EditAttemptArgs {
    pub attempt: String,
    #[arg(long)]
    pub handle: Option<String>,
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
    #[arg(long, value_name = "KEY=STATUS")]
    pub check: Vec<String>,
    #[arg(long)]
    pub evidence: Option<String>,
    #[arg(long)]
    pub note: Option<String>,
    #[arg(long, value_enum)]
    pub end: Option<NonSuccessOutcome>,
    #[arg(long)]
    pub why: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReopenArgs {
    pub work: String,
    #[arg(long)]
    pub why: String,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    pub work: String,
    #[arg(long, value_name = "KEY=VALUE")]
    pub meta: Vec<String>,
}

#[derive(Debug, Args)]
pub struct FinishArgs {
    pub work: String,
    #[arg(long)]
    pub attempt: Option<String>,
    #[arg(long)]
    pub external: bool,
    #[arg(long)]
    pub evidence: Option<String>,
}

#[derive(Debug, Args)]
pub struct DropArgs {
    pub work: String,
    #[arg(long)]
    pub attempt: Option<String>,
    #[arg(long, value_enum)]
    pub outcome: Option<NonSuccessOutcome>,
    #[arg(long)]
    pub why: String,
}

#[derive(Debug, Args)]
pub struct AskArgs {
    pub work: String,
    pub question: String,
}

#[derive(Debug, Args)]
pub struct AnswerArgs {
    pub question: String,
    pub answer: String,
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
pub struct ReconcileArgs {
    #[arg(long)]
    pub no_refresh: bool,
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
