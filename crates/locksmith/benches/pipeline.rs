//! `cargo bench --bench pipeline -- LOCKFILE [DONOR IMPORTER]`
//! Measures each stage independently; use an OS process profiler for peak RSS.
use std::{env, fs, hint::black_box, time::Instant};

use locksmith::{GraftOptions, Lockfile};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().skip(1).filter(|arg| arg != "--bench").collect();
    let Some(path) = args.first() else {
        eprintln!("usage: cargo bench --bench pipeline -- LOCKFILE [DONOR IMPORTER]");
        return Ok(());
    };
    let text = fs::read_to_string(path)?;
    let start = Instant::now();
    let base = Lockfile::parse(black_box(&text))?;
    let parse = start.elapsed();
    let start = Instant::now();
    base.validate()?;
    let validate = start.elapsed();
    let donor = match args.get(1) {
        Some(path) => Lockfile::parse(&fs::read_to_string(path)?)?,
        None => base.clone(),
    };
    let importer = args.get(2).map_or(".", String::as_str);
    let start = Instant::now();
    let result = locksmith::graft(&base, &donor, &GraftOptions::new([importer]))?;
    let graft = start.elapsed();
    let start = Instant::now();
    let output = black_box(result.to_yaml()?);
    let render = start.elapsed();
    println!(
        "{}",
        serde_json::json!({
            "input_bytes": text.len(), "output_bytes": output.len(),
            "parse_ms": parse.as_secs_f64() * 1000.0,
            "validate_ms": validate.as_secs_f64() * 1000.0,
            "graft_ms": graft.as_secs_f64() * 1000.0,
            "render_ms": render.as_secs_f64() * 1000.0,
        })
    );
    Ok(())
}
