use std::{
    error::Error,
    io::{self, Write},
};

use bpaf::Args;

use crate::{
    Result,
    args::{self, Command, Merge},
};

mod converge;
mod driver;
mod inspect;
mod merge;
mod repair;
mod validate;

pub(super) fn run(command: Command) -> Result<()> {
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
        Command::Converge {
            reference,
            allow_network,
        } => converge::run(&reference, allow_network)?,
        Command::Repair {
            reference,
            allow_network,
        } => repair::run(&reference, allow_network)?,
        Command::Validate { file, workspace } => return validate::run(&file, workspace),
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
