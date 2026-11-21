//! Command grammar shared by ordinary invocations, Git and the help command.
use std::ffi::OsString;

use bpaf::Bpaf;

const MERGE_USAGE: &str = "Usage: locksmith merge BASE OURS THEIRS [--context FILE [--allow-network]]\n       locksmith merge --git-driver %O %A %B %P [--allow-network]";

/// Merge, repair, converge and inspect pnpm lockfiles.
#[derive(Bpaf, Clone, Debug)]
#[bpaf(options, generate(parser), fallback_to_usage)]
pub enum Command {
    /// Print the Locksmith version
    #[bpaf(short('V'), long("version"))]
    Version,

    /// Combine three lockfile revisions. File merges write YAML to stdout.
    #[bpaf(command, usage(MERGE_USAGE))]
    Merge(#[bpaf(external(merge), fallback_with(|| Err(MERGE_USAGE)))] Merge),

    /// Reduce lockfile drift after an install. Write the converged YAML to stdout.
    #[bpaf(command)]
    Converge {
        /// Query the registry for missing dependency ranges
        #[bpaf(long)]
        allow_network: bool,
        /// Git ref containing the baseline lockfile and manifests
        #[bpaf(positional("REF"))]
        reference: String,
    },

    /// Repair a committed lockfile using the manifests at the same ref. Write YAML to stdout.
    #[bpaf(command)]
    Repair {
        /// Query the configured registry for missing release metadata
        #[bpaf(long)]
        allow_network: bool,
        /// Commit to repair (default: HEAD)
        #[bpaf(positional("REF"), fallback(String::from("HEAD")))]
        reference: String,
    },

    /// Inspect peer bindings and package instances in one project lockfile.
    #[bpaf(command)]
    Inspect {
        /// Print a machine-readable report with bindings and workspace paths
        #[bpaf(long)]
        json: bool,
        /// Use this exact pnpm version for peer policy
        #[bpaf(long, argument("VERSION"), optional)]
        pnpm_version: Option<node_semver::Version>,
        /// Lockfile to inspect (default: pnpm-lock.yaml)
        #[bpaf(
            positional("FILE"),
            fallback(std::path::PathBuf::from("pnpm-lock.yaml"))
        )]
        file: std::path::PathBuf,
    },

    #[bpaf(command, hide)]
    Help {
        #[bpaf(positional::<String>("COMMAND"), guard(|name| matches!(name.as_str(), "merge" | "converge" | "repair" | "inspect"), "expected merge, converge, repair or inspect"), optional)]
        topic: Option<String>,
    },
}

#[derive(Bpaf, Clone, Debug)]
pub enum Merge {
    Git {
        /// Use Git's merge inputs and replace OURS on success
        #[bpaf(long("git-driver"), req_flag(()))]
        _driver: (),
        /// Query the configured registry for missing release metadata
        #[bpaf(long)]
        allow_network: bool,
        #[bpaf(external)]
        inputs: Inputs,
        /// Workspace-relative lockfile path supplied by Git
        #[bpaf(positional("PATH"))]
        path: String,
    },
    Workspace {
        /// Read merged workspace intent from JSON
        #[bpaf(long, argument("FILE"))]
        context: OsString,
        /// Query the configured registry for missing release metadata
        #[bpaf(long)]
        allow_network: bool,
        #[bpaf(external)]
        inputs: Inputs,
    },
    Files(#[bpaf(external(inputs))] Inputs),
}

#[derive(Bpaf, Clone, Debug)]
pub struct Inputs {
    /// Common ancestor lockfile
    #[bpaf(positional("BASE"))]
    base: OsString,
    /// Our lockfile
    #[bpaf(positional("OURS"))]
    ours: OsString,
    /// Their lockfile
    #[bpaf(positional("THEIRS"))]
    theirs: OsString,
}

impl Inputs {
    pub fn files(&self) -> [&OsString; 3] {
        [&self.base, &self.ours, &self.theirs]
    }
}
