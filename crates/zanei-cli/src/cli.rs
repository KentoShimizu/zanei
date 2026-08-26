use std::path::PathBuf;

use clap::{ArgAction, ArgGroup, Args, Parser, Subcommand, ValueEnum};
use zanei_core::timeline::MIN_TIMELINE_TOKEN_BUDGET_TOKENS;

use crate::setup::{Agent, Scope};

#[derive(Debug, Parser)]
#[command(
    name = "zanei",
    version,
    about = "Private, local activity context for AI agents on macOS"
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use this configuration file instead of the default path"
    )]
    pub config: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use this event store instead of the default path"
    )]
    pub store: Option<PathBuf>,
    #[arg(long, global = true, help = "Print structured JSON when supported")]
    pub json: bool,
    #[arg(
        short = 'q',
        long,
        global = true,
        help = "Suppress progress messages and notices"
    )]
    pub quiet: bool,
    #[arg(
        short = 'v',
        long,
        global = true,
        action = ArgAction::Count,
        help = "Print diagnostic details to stderr"
    )]
    pub verbose: u8,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Diagnose required macOS permissions and guide granting")]
    Doctor(DoctorArgs),
    #[command(about = "Start background recording with launchd")]
    Start(StartArgs),
    #[command(about = "Stop recording and unregister while keeping stored data")]
    Stop,
    #[command(about = "Temporarily suspend recording")]
    Pause(PauseArgs),
    #[command(about = "Resume recording after a pause")]
    Resume,
    #[command(about = "Show daemon state, capture configuration, and store statistics")]
    Status,
    #[command(about = "Capture events in the foreground to stdout or a file")]
    Record(RecordArgs),
    #[command(about = "Retrieve raw events with filters")]
    Query(QueryArgs),
    #[command(about = "Build an LLM-ready, token-budgeted timeline")]
    Timeline(TimelineArgs),
    #[command(about = "Dump raw events for backup or external processing")]
    Export(ExportArgs),
    #[command(about = "Delete stored events manually (destructive)")]
    Purge(PurgeArgs),
    #[command(about = "List apps available for filter selection")]
    Apps(AppsArgs),
    #[command(about = "Manage capture-time allow and deny lists")]
    Filter(FilterArgs),
    #[command(about = "Initialize, show, locate, or edit configuration")]
    Config(ConfigArgs),
    #[command(about = "Run the stdio MCP server")]
    Mcp,
    #[command(about = "Install the skill and MCP registration for an agent")]
    Setup(SetupArgs),
    #[command(name = "__daemon", hide = true)]
    Daemon,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long, help = "Open System Settings for each missing permission")]
    pub fix: bool,
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[arg(long, help = "Run recording in the foreground without launchd")]
    pub foreground: bool,
}

#[derive(Debug, Args)]
pub struct PauseArgs {
    #[arg(
        long = "for",
        value_name = "TIME",
        help = "Pause for this duration; omit to pause indefinitely"
    )]
    pub duration: Option<String>,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("destination").required(true).multiple(false).args(["stream", "out"])))]
pub struct RecordArgs {
    #[arg(long, help = "Stream events to stdout as they occur")]
    pub stream: bool,
    #[arg(
        long,
        value_name = "FILE",
        help = "Write events to this file instead of stdout"
    )]
    pub out: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        default_value = "jsonl",
        help = "Write events in this output format"
    )]
    pub format: RecordFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RecordFormat {
    Jsonl,
}

#[derive(Debug, Args)]
pub struct QueryArgs {
    #[arg(
        long,
        default_value = "15m",
        help = "Start of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub since: String,
    #[arg(
        long,
        default_value = "now",
        help = "End of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub until: String,
    #[arg(
        long,
        value_name = "TYPE,...",
        help = "Filter by comma-separated event types; trailing wildcards are allowed"
    )]
    pub types: Option<String>,
    #[arg(long, help = "Filter by application name")]
    pub app: Option<String>,
    #[arg(long, help = "Filter by application bundle identifier")]
    pub bundle_id: Option<String>,
    #[arg(long, default_value_t = 500, help = "Return at most this many events")]
    pub limit: usize,
    #[arg(
        long,
        value_enum,
        default_value = "jsonl",
        help = "Write events in this output format"
    )]
    pub format: QueryFormat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum QueryFormat {
    Jsonl,
    Json,
    Table,
}

#[derive(Debug, Args)]
pub struct TimelineArgs {
    #[arg(
        long,
        default_value = "1h",
        help = "Start of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub since: String,
    #[arg(
        long,
        default_value = "now",
        help = "End of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub until: String,
    #[arg(
        long,
        value_enum,
        default_value = "md",
        help = "Write LLM-ready Markdown or structured JSON"
    )]
    pub format: TimelineOutputFormat,
    #[arg(
        long,
        default_value_t = 4_000,
        help = "Approximate token cap; content is coarsened to fit",
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new()
            .range(MIN_TIMELINE_TOKEN_BUDGET_TOKENS as u64..)
    )]
    pub token_budget: usize,
    #[arg(
        long,
        value_enum,
        default_value = "coarse",
        help = "Summarize by session or by interaction"
    )]
    pub granularity: GranularityArg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum TimelineOutputFormat {
    Md,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum GranularityArg {
    Coarse,
    Fine,
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    #[arg(
        long,
        default_value = "24h",
        help = "Start of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub since: String,
    #[arg(
        long,
        default_value = "now",
        help = "End of the time range (relative duration, RFC3339 timestamp, or now)"
    )]
    pub until: String,
    #[arg(
        long,
        value_name = "TYPE,...",
        help = "Export comma-separated event types; trailing wildcards are allowed"
    )]
    pub types: Option<String>,
    #[arg(
        long,
        value_enum,
        default_value = "jsonl",
        help = "Write events in this output format"
    )]
    pub format: ExportFormat,
    #[arg(
        long,
        value_name = "FILE",
        help = "Write output to this file instead of stdout; required for --format sqlite"
    )]
    pub out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ExportFormat {
    Jsonl,
    Json,
    /// A plain SQLite file with the store's tables (requires --out)
    Sqlite,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("selection")
        .required(true)
        .multiple(true)
        .args(["before", "all", "types"])
))]
pub struct PurgeArgs {
    #[arg(
        long,
        value_name = "TIME",
        help = "Delete events older than this time expression"
    )]
    pub before: Option<String>,
    #[arg(
        long,
        conflicts_with_all = ["before", "types", "app", "bundle_id"],
        help = "Delete every stored event; prompts unless --quiet"
    )]
    pub all: bool,
    #[arg(
        long,
        value_name = "TYPE,...",
        help = "Delete comma-separated event types; trailing wildcards are allowed"
    )]
    pub types: Option<String>,
    #[arg(long, requires = "types", conflicts_with = "bundle_id")]
    pub app: Option<String>,
    #[arg(long, requires = "types", conflicts_with = "app")]
    pub bundle_id: Option<String>,
}

#[derive(Debug, Args)]
pub struct FilterArgs {
    #[arg(value_enum, value_name = "SCOPE")]
    pub scope: Option<FilterScopeArg>,
    #[command(subcommand)]
    pub command: FilterCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum FilterScopeArg {
    TextContent,
    ContentSnapshot,
}

#[derive(Debug, Subcommand)]
pub enum FilterCommand {
    #[command(about = "Show filter lists and the active mode")]
    Show,
    #[command(about = "Manage the app deny list")]
    ExcludeApp(FilterMutationArgs),
    #[command(about = "Manage the app allow list")]
    OnlyApp(FilterMutationArgs),
    #[command(about = "Manage the website deny list")]
    ExcludeSite(FilterMutationArgs),
    #[command(about = "Manage the website allow list")]
    OnlySite(FilterMutationArgs),
}

#[derive(Debug, Args)]
pub struct FilterMutationArgs {
    #[command(subcommand)]
    pub action: FilterAction,
}

#[derive(Debug, Subcommand)]
pub enum FilterAction {
    #[command(about = "Add an entry to the selected filter list")]
    Add {
        value: Option<String>,
        #[arg(long, help = "Save an app value without verifying that it exists")]
        unverified: bool,
    },
    #[command(about = "Remove an entry from the selected filter list")]
    Remove { value: String },
}

#[derive(Debug, Args)]
pub struct AppsArgs {
    #[arg(value_name = "QUERY")]
    pub query: Option<String>,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    #[command(about = "Create a fully commented configuration template")]
    Init,
    #[command(about = "Print the configuration file path")]
    Path,
    #[command(about = "Print the effective configuration")]
    Show,
    #[command(about = "Open the configuration in $EDITOR")]
    Edit,
    #[command(about = "Set a scalar configuration value")]
    Set {
        #[arg(value_name = "DOTTED_KEY")]
        dotted_key: String,
        #[arg(value_name = "VALUE")]
        value: String,
    },
}

#[derive(Debug, Args)]
pub struct SetupArgs {
    #[arg(long, value_enum, help = "Agent integration to configure")]
    pub agent: Agent,
    #[arg(
        long,
        value_enum,
        default_value = "project",
        help = "Configure the current project or user account; user-global agents ignore it"
    )]
    pub scope: Scope,
    #[arg(long, help = "Preview planned file changes without writing")]
    pub print: bool,
}
