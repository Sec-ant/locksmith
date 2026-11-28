//! Command-line adapters for the in-memory locksmith library.
use std::{env, error::Error, ffi::OsString, fmt, process::ExitCode};

use args::{Cli, Command};
use clap::{CommandFactory, FromArgMatches, error::ErrorKind};
use locksmith::LockfileError;
use ui::{ColorChoice, Ui};

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod args;
mod commands;
mod git;
mod policy;
mod registry;
mod ui;
mod workspace;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Explicit CLI usage error (exit code 2).
#[derive(Debug)]
pub(crate) struct UsageError(pub(crate) String);

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for UsageError {}

/// Silent non-zero exit (exit code 1) used when `--check` or `--json` already reported the failure.
#[derive(Debug)]
pub(crate) struct ExitStatus(pub(crate) u8);

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exited with status {}", self.0)
    }
}

impl Error for ExitStatus {}

fn parallel_parse(bytes: u64) -> bool {
    bytes >= 128 * 1024 && std::thread::available_parallelism().is_ok_and(|n| n.get() >= 3)
}

/// Pre-scan argv for `--color <WHEN>` / `--color=<WHEN>` so clap's own help and parse
/// diagnostics honor the requested color mode before full argument parsing succeeds.
fn prescan_color_choice(args: &[OsString]) -> ColorChoice {
    let mut choice = ColorChoice::Auto;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        let Some(s) = arg.to_str() else {
            continue;
        };
        if s == "--" {
            break;
        }
        let value = if let Some(v) = s.strip_prefix("--color=") {
            Some(v)
        } else if s == "--color" {
            iter.next().and_then(|v| v.to_str())
        } else {
            None
        };
        if let Some(v) = value {
            match v {
                "auto" => choice = ColorChoice::Auto,
                "always" => choice = ColorChoice::Always,
                "never" => choice = ColorChoice::Never,
                _ => {}
            }
        }
    }
    choice
}

fn main() -> ExitCode {
    let raw_args: Vec<OsString> = env::args_os().collect();
    let pre_color = prescan_color_choice(&raw_args);
    let matches = match Cli::command()
        .color(pre_color.to_clap())
        .try_get_matches_from(&raw_args)
    {
        Ok(matches) => matches,
        Err(err) => {
            let code = match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => ExitCode::SUCCESS,
                _ => ExitCode::from(2),
            };
            let _ = err.print();
            return code;
        }
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(2);
        }
    };

    let ui = Ui::new(cli.color, cli.quiet, cli.verbose);

    let Some(command) = cli.command else {
        let _ = Cli::command().color(cli.color.to_clap()).print_long_help();
        return ExitCode::SUCCESS;
    };

    if let Some(dir) = &cli.directory
        && let Err(err) = env::set_current_dir(dir)
    {
        ui.error(format!("{}: {err}", dir.display()));
        return ExitCode::from(2);
    }

    let driver = matches!(
        command,
        Command::Driver(ref d) if matches!(d.command, args::DriverCommand::Run(_))
    );
    let validating = matches!(command, Command::Validate(_));
    let result = commands::run(command, &ui);
    #[cfg(feature = "bench-profile")]
    for (component, elapsed) in locksmith::profile::take_samples() {
        eprintln!("locksmith-profile\t{component}\t{}", elapsed.as_nanos());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Some(status) = error.downcast_ref::<ExitStatus>() {
                return ExitCode::from(status.0);
            }
            if error.downcast_ref::<UsageError>().is_some() {
                ui.error(&*error);
                ui.hint("pass --help for usage information");
                return ExitCode::from(2);
            }
            ui.print_error(&*error);
            match error.downcast_ref::<LockfileError>() {
                _ if driver => ExitCode::from(1),
                Some(
                    LockfileError::Conflicts(_)
                    | LockfileError::ResolutionRequired { .. }
                    | LockfileError::UnmetPeers(_),
                ) => ExitCode::from(1),
                Some(
                    LockfileError::Parse(_)
                    | LockfileError::InvalidDocument(_)
                    | LockfileError::InvalidGraph(_)
                    | LockfileError::ManifestMismatch { .. },
                ) if validating => ExitCode::from(1),
                _ => ExitCode::from(2),
            }
        }
    }
}
