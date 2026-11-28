//! Command grammar shared by ordinary invocations, Git, completions and man pages.
use std::{ffi::OsString, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use crate::ui::{ColorChoice, clap_styles};

const ROOT_EXAMPLES: &str = "\
Examples:
  locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
  locksmith converge origin/main --diff
  locksmith converge origin/main --write
  locksmith repair --write
  locksmith validate --workspace
  locksmith inspect --json
  locksmith driver setup";

const MERGE_EXAMPLES: &str = "\
Examples:
  locksmith merge base.yaml ours.yaml theirs.yaml > merged.yaml
  locksmith merge base.yaml ours.yaml theirs.yaml -o merged.yaml
  locksmith merge base.yaml ours.yaml theirs.yaml --context ctx.json --allow-network";

const CONVERGE_EXAMPLES: &str = "\
Examples:
  locksmith converge origin/main > converged.yaml
  locksmith converge origin/main --diff
  locksmith converge origin/main --write
  locksmith converge origin/main --check";

const REPAIR_EXAMPLES: &str = "\
Examples:
  locksmith repair HEAD > repaired.yaml
  locksmith repair --diff
  locksmith repair --write
  locksmith repair --check";

const VALIDATE_EXAMPLES: &str = "\
Examples:
  locksmith validate
  locksmith validate --workspace
  locksmith validate --workspace --json
  locksmith validate --workspace --format json";

const INSPECT_EXAMPLES: &str = "\
Examples:
  locksmith inspect
  locksmith inspect --json
  locksmith inspect --format json --pnpm-version 10.33.2 pnpm-lock.yaml";

const DRIVER_SETUP_EXAMPLES: &str = "\
Examples:
  locksmith driver setup
  locksmith driver setup --apply
  locksmith driver setup --global --bin locksmith --apply";

const DRIVER_RUN_EXAMPLES: &str = "\
Examples:
  git config merge.locksmith.driver 'locksmith driver run %O %A %B %P'";

/// Output format for reporting subcommands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ReportFormat {
    /// Human-readable text output.
    #[default]
    Text,
    /// Machine-readable JSON output.
    Json,
}

/// Merge, converge, repair, validate, and inspect pnpm lockfiles.
#[derive(Parser, Clone, Debug)]
#[command(
    name = "locksmith",
    version,
    about = "Merge, converge, repair, validate, and inspect pnpm lockfiles.",
    long_about = "Merge, converge, repair, validate, and inspect pnpm lockfiles.\n\n\
Locksmith operates offline and non-destructively by default. Commands that produce\n\
a lockfile write YAML to stdout unless --output, --write, --diff, or --check is given.",
    after_long_help = ROOT_EXAMPLES,
    styles = clap_styles(),
    propagate_version = false,
    disable_help_subcommand = false,
)]
pub struct Cli {
    /// Run as if locksmith was started in DIR instead of the current working directory
    #[arg(
        short = 'C',
        long = "directory",
        value_name = "DIR",
        global = true,
        help_heading = "Global options"
    )]
    pub directory: Option<PathBuf>,

    /// Control colored output
    #[arg(
        long,
        value_enum,
        default_value_t = ColorChoice::Auto,
        value_name = "WHEN",
        global = true,
        help_heading = "Global options"
    )]
    pub color: ColorChoice,

    /// Suppress non-essential warnings and progress output on stderr
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "verbose",
        help_heading = "Global options"
    )]
    pub quiet: bool,

    /// Show detailed diagnostics and registry lookup activity on stderr
    #[arg(
        short,
        long,
        global = true,
        conflicts_with = "quiet",
        help_heading = "Global options"
    )]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum Command {
    /// Combine three lockfile revisions
    #[command(after_long_help = MERGE_EXAMPLES)]
    Merge(MergeArgs),

    /// Reduce lockfile drift after an install
    #[command(after_long_help = CONVERGE_EXAMPLES)]
    Converge(ConvergeArgs),

    /// Repair a committed lockfile using the manifests at the same ref
    #[command(after_long_help = REPAIR_EXAMPLES)]
    Repair(RepairArgs),

    /// Check a pnpm lockfile's structure, references, and optionally its workspace
    #[command(after_long_help = VALIDATE_EXAMPLES)]
    Validate(ValidateArgs),

    /// Inspect peer bindings and package instances in a lockfile
    #[command(after_long_help = INSPECT_EXAMPLES)]
    Inspect(InspectArgs),

    /// Configure or run the Git merge driver for pnpm-lock.yaml
    Driver(DriverArgs),

    /// Generate shell completion scripts
    Completions(CompletionsArgs),

    /// Print a roff manual page for locksmith or a subcommand
    Man(ManArgs),
}

#[derive(Args, Clone, Debug)]
pub struct MergeArgs {
    /// Common ancestor lockfile
    #[arg(value_name = "BASE")]
    pub base: OsString,

    /// Current branch lockfile
    #[arg(value_name = "OURS")]
    pub ours: OsString,

    /// Incoming branch lockfile
    #[arg(value_name = "THEIRS")]
    pub theirs: OsString,

    /// Read merged workspace intent from JSON
    #[arg(long, value_name = "FILE", help_heading = "Merge options")]
    pub context: Option<OsString>,

    /// Query the configured registry for missing release metadata (requires --context)
    #[arg(long, help_heading = "Merge options")]
    pub allow_network: bool,

    /// Write merged YAML atomically to FILE instead of stdout
    #[arg(short, long, value_name = "FILE", help_heading = "Output options")]
    pub output: Option<PathBuf>,
}

#[derive(Args, Clone, Debug)]
pub struct OutputModeArgs {
    /// Write resulting YAML atomically to FILE instead of stdout
    #[arg(
        short,
        long,
        value_name = "FILE",
        conflicts_with_all = ["write", "diff", "check"],
        help_heading = "Output options"
    )]
    pub output: Option<PathBuf>,

    /// Update pnpm-lock.yaml in place atomically
    #[arg(
        short,
        long,
        conflicts_with_all = ["output", "diff", "check"],
        help_heading = "Output options"
    )]
    pub write: bool,

    /// Print a unified diff instead of the full lockfile YAML
    #[arg(
        long,
        conflicts_with_all = ["output", "write", "check"],
        help_heading = "Output options"
    )]
    pub diff: bool,

    /// Exit with code 1 if changes would be made, without writing output
    #[arg(
        long,
        conflicts_with_all = ["output", "write", "diff"],
        help_heading = "Output options"
    )]
    pub check: bool,
}

#[derive(Args, Clone, Debug)]
pub struct ConvergeArgs {
    /// Git ref containing the baseline lockfile and manifests
    #[arg(value_name = "REF")]
    pub reference: String,

    /// Query the configured registry for missing dependency ranges and release metadata
    #[arg(long)]
    pub allow_network: bool,

    #[command(flatten)]
    pub output_mode: OutputModeArgs,
}

#[derive(Args, Clone, Debug)]
pub struct RepairArgs {
    /// Commit to repair
    #[arg(value_name = "REF", default_value = "HEAD")]
    pub reference: String,

    /// Query the configured registry for missing release metadata
    #[arg(long)]
    pub allow_network: bool,

    #[command(flatten)]
    pub output_mode: OutputModeArgs,
}

#[derive(Args, Clone, Debug)]
pub struct ValidateArgs {
    /// Lockfile to validate
    #[arg(value_name = "FILE", default_value = "pnpm-lock.yaml")]
    pub file: PathBuf,

    /// Compare the lockfile with the workspace rooted beside it
    #[arg(long)]
    pub workspace: bool,

    /// Output format (default: text)
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        conflicts_with = "json",
        help_heading = "Output options"
    )]
    pub format: Option<ReportFormat>,

    /// Shorthand for --format json
    #[arg(long, conflicts_with = "format", help_heading = "Output options")]
    pub json: bool,
}

impl ValidateArgs {
    pub fn is_json(&self) -> bool {
        self.json || self.format == Some(ReportFormat::Json)
    }
}

#[derive(Args, Clone, Debug)]
pub struct InspectArgs {
    /// Lockfile to inspect
    #[arg(value_name = "FILE", default_value = "pnpm-lock.yaml")]
    pub file: PathBuf,

    /// Use this exact pnpm version for peer policy
    #[arg(long, value_name = "VERSION")]
    pub pnpm_version: Option<node_semver::Version>,

    /// Output format (default: text)
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        conflicts_with = "json",
        help_heading = "Output options"
    )]
    pub format: Option<ReportFormat>,

    /// Shorthand for --format json
    #[arg(long, conflicts_with = "format", help_heading = "Output options")]
    pub json: bool,
}

impl InspectArgs {
    pub fn is_json(&self) -> bool {
        self.json || self.format == Some(ReportFormat::Json)
    }
}

#[derive(Args, Clone, Debug)]
pub struct DriverArgs {
    #[command(subcommand)]
    pub command: DriverCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum DriverCommand {
    /// Print or apply the Git merge driver configuration for pnpm-lock.yaml
    #[command(after_long_help = DRIVER_SETUP_EXAMPLES)]
    Setup(DriverSetupArgs),

    /// Run as Git's low-level merge driver (%O %A %B %P)
    #[command(after_long_help = DRIVER_RUN_EXAMPLES)]
    Run(DriverRunArgs),
}

#[derive(Args, Clone, Debug)]
pub struct DriverSetupArgs {
    /// Apply the git config setting instead of only printing setup instructions
    #[arg(long)]
    pub apply: bool,

    /// Configure the driver in global ~/.gitconfig instead of the local repository
    #[arg(long)]
    pub global: bool,

    /// Path or command name for the locksmith executable in git config
    #[arg(long, value_name = "PATH")]
    pub bin: Option<String>,

    /// Lockfile path pattern for .gitattributes
    #[arg(long, value_name = "PATTERN", default_value = "/pnpm-lock.yaml")]
    pub lockfile: String,

    /// Pass --allow-network in the configured Git merge driver command
    #[arg(long)]
    pub allow_network: bool,
}

#[derive(Args, Clone, Debug)]
pub struct DriverRunArgs {
    /// Common ancestor lockfile (Git %O)
    #[arg(value_name = "BASE")]
    pub base: OsString,

    /// Current branch lockfile replaced atomically on success (Git %A)
    #[arg(value_name = "OURS")]
    pub ours: OsString,

    /// Incoming branch lockfile (Git %B)
    #[arg(value_name = "THEIRS")]
    pub theirs: OsString,

    /// Repository-relative lockfile path supplied by Git (Git %P)
    #[arg(value_name = "PATH")]
    pub path: String,

    /// Query the configured registry for missing release metadata
    #[arg(long)]
    pub allow_network: bool,
}

#[derive(Args, Clone, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum, value_name = "SHELL")]
    pub shell: Shell,
}

#[derive(Args, Clone, Debug)]
pub struct ManArgs {
    /// Subcommand to generate a manual page for (omit for the top-level locksmith page)
    #[arg(value_name = "COMMAND")]
    pub command: Option<String>,
}

/// Validated merge invocation mode.
pub enum MergeMode {
    Files {
        files: [OsString; 3],
        output: Option<PathBuf>,
    },
    Workspace {
        files: [OsString; 3],
        context: OsString,
        allow_network: bool,
        output: Option<PathBuf>,
    },
}

impl MergeArgs {
    pub fn into_mode(self) -> std::result::Result<MergeMode, String> {
        let files = [self.base, self.ours, self.theirs];
        if let Some(context) = self.context {
            return Ok(MergeMode::Workspace {
                files,
                context,
                allow_network: self.allow_network,
                output: self.output,
            });
        }
        if self.allow_network {
            return Err(
                "--allow-network requires --context FILE (plain three-file merge has no workspace context)"
                    .to_string(),
            );
        }
        Ok(MergeMode::Files {
            files,
            output: self.output,
        })
    }
}
