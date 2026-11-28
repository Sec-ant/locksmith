use std::{
    error::Error,
    fs,
    io::{self, ErrorKind, Write},
    path::Path,
};

use clap::CommandFactory;
use clap_complete::Generator;
use clap_mangen::Man;

use crate::{
    ExitStatus, Result, UsageError,
    args::{Cli, Command, DriverCommand, MergeMode, OutputModeArgs},
    ui::{Ui, write_atomic},
};

mod converge;
mod driver;
mod inspect;
mod merge;
mod repair;
mod validate;

pub(super) fn run(command: Command, ui: &Ui) -> Result<()> {
    match command {
        Command::Merge(args) => match args.into_mode().map_err(UsageError)? {
            MergeMode::Files { files, output } => {
                let refs = [&files[0], &files[1], &files[2]];
                let [base, ours, theirs] =
                    merge::read_three(refs).map_err(|e| e as Box<dyn Error>)?;
                let result = locksmith::merge(&base, &ours, &theirs, None)?;
                emit_yaml(&result.to_yaml()?, output.as_deref(), ui)
            }
            MergeMode::Workspace {
                files,
                context,
                allow_network,
                output,
            } => {
                let refs = [&files[0], &files[1], &files[2]];
                let result = merge::from_file(refs, &context, allow_network, ui)?;
                emit_yaml(&result.to_yaml()?, output.as_deref(), ui)
            }
        },
        Command::Converge(args) => {
            let (original, result) = converge::run(&args.reference, args.allow_network, ui)?;
            let yaml = result.to_yaml()?;
            emit_transformed(
                &original,
                &yaml,
                "a/pnpm-lock.yaml",
                "b/pnpm-lock.yaml",
                Path::new("pnpm-lock.yaml"),
                &args.output_mode,
                "pnpm-lock.yaml would change after converge (re-run with --diff to inspect or --write to apply)",
                None,
                ui,
            )
        }
        Command::Repair(args) => {
            let (original, result) = repair::run(&args.reference, args.allow_network, ui)?;
            let yaml = result.to_yaml()?;
            let check_msg = format!(
                "{}:pnpm-lock.yaml needs repair (re-run with --diff to inspect, then write and commit the repaired lockfile)",
                args.reference
            );
            emit_transformed(
                &original,
                &yaml,
                &format!("{}:pnpm-lock.yaml", args.reference),
                "repaired/pnpm-lock.yaml",
                Path::new("pnpm-lock.yaml"),
                &args.output_mode,
                &check_msg,
                Some(&args.reference),
                ui,
            )
        }
        Command::Validate(args) => {
            let json = args.is_json();
            validate::run(&args.file, args.workspace, json, ui)
        }
        Command::Inspect(args) => {
            let json = args.is_json();
            inspect::run(&args.file, args.pnpm_version, json, ui)
        }
        Command::Driver(args) => match args.command {
            DriverCommand::Setup(setup) => driver::setup(&setup, ui),
            DriverCommand::Run(run_args) => {
                let files = [&run_args.base, &run_args.ours, &run_args.theirs];
                driver::run(files, &run_args.path, run_args.allow_network, ui)
            }
        },
        Command::Completions(args) => {
            let mut cmd = Cli::command();
            cmd.set_bin_name("locksmith");
            cmd.build();
            args.shell.try_generate(&cmd, &mut io::stdout().lock())?;
            Ok(())
        }
        Command::Man(args) => {
            let root = Cli::command();
            let target = match args.command.as_deref() {
                None => root,
                Some(name) => root
                    .find_subcommand(name)
                    .cloned()
                    .ok_or_else(|| UsageError(format!("unknown subcommand '{name}'")))?,
            };
            Man::new(target).render(&mut io::stdout().lock())?;
            Ok(())
        }
    }
}

fn emit_yaml(yaml: &str, output: Option<&Path>, ui: &Ui) -> Result<()> {
    match output {
        Some(path) => {
            write_atomic(path, yaml.as_bytes())?;
            ui.verbose(format!("wrote {}", path.display()));
            Ok(())
        }
        None => {
            io::stdout().lock().write_all(yaml.as_bytes())?;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_transformed(
    original: &str,
    yaml: &str,
    old_label: &str,
    new_label: &str,
    default_target: &Path,
    mode: &OutputModeArgs,
    check_message: &str,
    repair_ref: Option<&str>,
    ui: &Ui,
) -> Result<()> {
    if mode.check {
        if original == yaml {
            return Ok(());
        }
        ui.error(check_message);
        return Err(Box::new(ExitStatus(1)));
    }
    if mode.diff {
        if original != yaml {
            let mut out = io::stdout().lock();
            ui.write_unified_diff(&mut out, old_label, original, new_label, yaml)?;
        }
        return Ok(());
    }
    if mode.write {
        if let Some(reference) = repair_ref {
            match fs::read_to_string(default_target) {
                Ok(worktree) if worktree != original => {
                    return Err(format!(
                        "{} differs from {reference}:pnpm-lock.yaml; use --output FILE instead of --write to avoid overwriting worktree changes",
                        default_target.display()
                    )
                    .into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", default_target.display()).into()),
            }
        }
        write_atomic(default_target, yaml.as_bytes())?;
        ui.verbose(format!("updated {}", default_target.display()));
        return Ok(());
    }
    emit_yaml(yaml, mode.output.as_deref(), ui)
}
