//! Benchmark the Rust convergence pipeline. BEFORE and AFTER are JSON objects
//! keyed by workspace importer path, each containing its package.json value.
use locksmith::{Lockfile, ManifestChange, OverrideTieBreak, Value, converge};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    hint::black_box,
    time::Instant,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).filter(|arg| arg != "--bench").collect();
    let usage = "cargo bench --bench pipeline -- BASE CANDIDATE BEFORE.json AFTER.json first|last";
    if args.is_empty() {
        eprintln!("{usage}");
        return Ok(());
    }
    let [base, candidate, before, after, precedence] = args.as_slice() else {
        return Err(usage.into());
    };
    let tie_break = match precedence.as_str() {
        "first" => OverrideTieBreak::First,
        "last" => OverrideTieBreak::Last,
        _ => return Err(usage.into()),
    };
    let manifests = |path: &str| -> Result<BTreeMap<String, Value>, Box<dyn std::error::Error>> {
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    };
    let before = manifests(before)?;
    let after = manifests(after)?;
    let names: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let changes: Vec<_> = names
        .into_iter()
        .map(|name| ManifestChange {
            importer: name,
            before: before.get(name),
            after: after.get(name),
        })
        .collect();
    let baseline_text = fs::read_to_string(base)?;
    let candidate_text = fs::read_to_string(candidate)?;
    let start = Instant::now();
    let base = Lockfile::parse(black_box(&baseline_text))?;
    let candidate = Lockfile::parse(black_box(&candidate_text))?;
    let parse = start.elapsed();
    let start = Instant::now();
    let result = converge(
        &base,
        &candidate,
        &changes,
        locksmith::ConvergeOptions {
            baseline_configuration: None,
            configuration: Some(&locksmith::WorkspaceConfig {
                override_tie_break: tie_break,
                ..Default::default()
            }),
            ..Default::default()
        },
    )?;
    let convergence = start.elapsed();
    let start = Instant::now();
    let output = black_box(result.to_yaml()?);
    let render = start.elapsed();
    println!(
        "{}",
        serde_json::json!({
            "baseline_bytes": baseline_text.len(), "candidate_bytes": candidate_text.len(),
            "output_bytes": output.len(), "parse_ms": parse.as_secs_f64() * 1000.0,
            "converge_ms": convergence.as_secs_f64() * 1000.0, "render_ms": render.as_secs_f64() * 1000.0
        })
    );
    Ok(())
}
