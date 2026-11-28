//! Command-line adapters for the in-memory locksmith library.
use std::{error::Error, process::ExitCode};

use args::{Command, Merge};
use bpaf::{Args, ParseFailure};
use locksmith::LockfileError;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod args;

mod git;
mod registry;
mod workspace;

mod policy;

mod commands;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn parallel_parse(bytes: u64) -> bool {
    bytes >= 128 * 1024 && std::thread::available_parallelism().is_ok_and(|n| n.get() >= 3)
}

fn main() -> ExitCode {
    let command = match args::parser().run_inner(Args::current_args()) {
        Ok(command) => command,
        Err(message) => {
            message.print_message(100);
            return if matches!(message, ParseFailure::Stderr(_)) {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    let driver = matches!(command, Command::Merge(Merge::Git { .. }));
    let validating = matches!(command, Command::Validate { .. });
    let result = commands::run(command);
    #[cfg(feature = "bench-profile")]
    for (component, elapsed) in locksmith::profile::take_samples() {
        eprintln!("locksmith-profile\t{component}\t{}", elapsed.as_nanos());
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("locksmith: {error}");
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
