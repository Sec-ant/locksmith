//! Small filesystem adapter for the library; writes successful results to stdout.
//! `cargo run --example resolve -- merge BASE OURS THEIRS`
//! `cargo run --example resolve -- graft BASE DONOR IMPORTER...`
//! `cargo run --example resolve -- check LOCKFILE...`
use std::{env, error::Error, fs, process::ExitCode};

use locksmith::{GraftOptions, Lockfile, LockfileError};

fn read(path: &std::ffi::OsStr) -> Result<Lockfile, Box<dyn Error>> {
    Ok(Lockfile::parse(&fs::read_to_string(path)?)?)
}

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let output = match args.first().and_then(|arg| arg.to_str()) {
        Some("merge") if args.len() == 4 => {
            locksmith::merge(&read(&args[1])?, &read(&args[2])?, &read(&args[3])?)?
        }
        Some("graft") if args.len() >= 4 => {
            let importers: Result<Vec<_>, _> = args[3..].iter().map(|arg| {
                arg.to_str().ok_or("importer paths must be UTF-8")
            }).collect();
            locksmith::graft(&read(&args[1])?, &read(&args[2])?, &GraftOptions::new(importers?))?
        }
        Some("check") if args.len() >= 2 => {
            for path in &args[1..] {
                let lockfile = read(path)?;
                lockfile.validate()?;
                assert_eq!(lockfile.to_yaml()?, fs::read_to_string(path)?);
                eprintln!("{}: valid, unchanged bytes preserved", path.display());
            }
            return Ok(());
        }
        Some("inspect") if args.len() == 3 => {
            let lockfile = read(&args[1])?;
            let closure = lockfile.dependency_closure(args[2].to_str().ok_or("importer must be UTF-8")?)?;
            println!("{}", serde_json::json!({"importers":closure.importers.len(),"packages":closure.packages.len(),"snapshots":closure.snapshots.len(),"variants":closure.variants()}));
            return Ok(());
        }
        _ => return Err("usage: resolve merge BASE OURS THEIRS | graft BASE DONOR IMPORTER... | check LOCKFILE...".into()),
    };
    print!("{}", output.to_yaml()?);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            if matches!(
                error.downcast_ref::<LockfileError>(),
                Some(LockfileError::Conflicts(_))
            ) {
                ExitCode::from(1)
            } else {
                ExitCode::from(2)
            }
        }
    }
}
