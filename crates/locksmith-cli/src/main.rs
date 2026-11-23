//! Command-line adapters for the in-memory locksmith library.
use std::{
    error::Error,
    io::{self, Write},
    process::ExitCode,
};

use args::{Command, Merge};
use bpaf::{Args, ParseFailure};
use locksmith::LockfileError;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod args;
mod converge;
mod driver;
mod generation;
mod git;
mod inspect;
mod merge;
mod policy;
mod registry;
mod repair;
mod workspace;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn parallel_parse(bytes: u64) -> bool {
    bytes >= 128 * 1024 && std::thread::available_parallelism().is_ok_and(|n| n.get() >= 3)
}

fn run(command: Command) -> Result<()> {
    let result = match command {
        Command::Merge(Merge::Files(inputs)) => {
            let [base, ours, theirs] =
                merge::read_three(inputs.files()).map_err(|e| e as Box<dyn Error>)?;
            locksmith::merge(&base, &ours, &theirs, None)?
        }
        Command::Merge(Merge::Workspace {
            inputs,
            context,
            allow_network,
        }) => merge::from_file(inputs.files(), &context, allow_network)?,
        Command::Merge(Merge::Git {
            inputs,
            path,
            allow_network,
            ..
        }) => return driver::run(inputs.files(), &path, allow_network),
        Command::Generate {
            allow_network,
            exhaustive,
            stdout,
            pnpm_version,
            hook_runtime,
        } => {
            return generation::run(
                allow_network,
                stdout,
                pnpm_version,
                hook_runtime,
                exhaustive,
            );
        }
        Command::Converge {
            reference,
            allow_network,
        } => converge::run(&reference, allow_network)?,
        Command::Repair {
            reference,
            allow_network,
        } => repair::run(&reference, allow_network)?,
        Command::Inspect {
            json,
            pnpm_version,
            file,
        } => return inspect::run(&file, pnpm_version, json),
        Command::Version => {
            println!("locksmith {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Command::Help { topic } => {
            let args: Vec<_> = topic.as_deref().into_iter().chain(["--help"]).collect();
            args::parser()
                .run_inner(Args::from(args.as_slice()).set_name("locksmith"))
                .expect_err("a help request produces the generated help")
                .print_message(100);
            return Ok(());
        }
    };
    io::stdout()
        .lock()
        .write_all(result.to_yaml()?.as_bytes())?;
    Ok(())
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
    match run(command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("locksmith: {error}");
            if driver
                || matches!(
                    error.downcast_ref::<LockfileError>(),
                    Some(
                        LockfileError::Conflicts(_)
                            | LockfileError::ResolutionRequired { .. }
                            | LockfileError::UnmetPeers(_)
                    )
                )
            {
                ExitCode::from(1)
            } else {
                ExitCode::from(2)
            }
        }
    }
}
