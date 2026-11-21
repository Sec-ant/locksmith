use std::{
    env,
    ffi::OsStr,
    io,
    process::{Command, ExitCode},
    time::Instant,
};

const BLOCKS: usize = 12;
const REQUIRED_SLOW_BLOCKS: usize = 10;
const MIN_SLOWDOWN_PCT: f64 = 5.0;

fn elapsed_ms(command: &str, binary: &OsStr) -> io::Result<f64> {
    let start = Instant::now();
    let status = Command::new("benchmarks/cli")
        .arg(command)
        .env("LOCKSMITH_BIN", binary)
        .status()?;
    let elapsed = start.elapsed();
    if !status.success() {
        return Err(io::Error::other(format!(
            "{binary:?} {command} exited with {status}"
        )));
    }
    Ok(elapsed.as_secs_f64() * 1000.0)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    (values[(values.len() - 1) / 2] + values[values.len() / 2]) / 2.0
}

fn change_pct(base: f64, other: f64) -> f64 {
    (other / base - 1.0) * 100.0
}

fn compare(command: &str, binaries: [&OsStr; 2]) -> io::Result<bool> {
    let [base, head] = binaries;
    for binary in [base, head, head, base] {
        elapsed_ms(command, binary)?;
    }

    let mut base_ms = [0.0; BLOCKS];
    let mut head_ms = [0.0; BLOCKS];
    let mut changes = [0.0; BLOCKS];
    let mut noise = [0.0; BLOCKS];
    for block in 0..BLOCKS {
        // Alternate base/PR/PR/base and PR/base/base/PR to balance run order.
        let mut times = [[0.0; 2]; 2];
        for pair in 0..2 {
            let order = if (block + pair) % 2 == 0 {
                [0, 1]
            } else {
                [1, 0]
            };
            for build in order {
                times[build][pair] = elapsed_ms(command, binaries[build])?;
            }
        }
        let [[b1, b2], [h1, h2]] = times;
        base_ms[block] = (b1 + b2) / 2.0;
        head_ms[block] = (h1 + h2) / 2.0;
        changes[block] = change_pct(base_ms[block], head_ms[block]);
        noise[block] = change_pct(b1.min(b2), b1.max(b2));
        eprintln!(
            "{command} block {}: base [{b1:.2}, {b2:.2}] ms, PR [{h1:.2}, {h2:.2}] ms",
            block + 1
        );
    }

    noise.sort_unstable_by(f64::total_cmp);
    let noise_p90 = noise[(BLOCKS * 9).div_ceil(10) - 1];
    let threshold = MIN_SLOWDOWN_PCT.max(noise_p90);
    let slow = changes.iter().filter(|&&change| change > threshold).count();
    let regressed = slow >= REQUIRED_SLOW_BLOCKS;
    println!(
        "| `{command}` | {:.2} ms | {:.2} ms | {:+.2}% | {threshold:.2}% | {slow}/{BLOCKS} | {} |",
        median(&mut base_ms),
        median(&mut head_ms),
        median(&mut changes),
        if regressed { "Regression" } else { "Pass" }
    );
    Ok(regressed)
}

fn main() -> io::Result<ExitCode> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let [base, head] = args.as_slice() else {
        return Err(io::Error::other("usage: compare BASE_BINARY PR_BINARY"));
    };
    println!("### CLI wall time on the same runner\n");
    println!(
        "Both builds run twice per block. Times and paired changes are medians across blocks. A regression needs at least {REQUIRED_SLOW_BLOCKS}/{BLOCKS} blocks above the larger of {MIN_SLOWDOWN_PCT}% and the 90th percentile of the base build's within-block variation.\n"
    );
    println!("| Command | Base | PR | Paired change | Threshold | Blocks above | Result |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: | --- |");
    let mut regressed = false;
    for command in ["merge", "converge", "repair"] {
        regressed |= compare(command, [base, head])?;
    }
    if Command::new(base)
        .args(["help", "inspect"])
        .output()?
        .status
        .success()
    {
        regressed |= compare("inspect", [base, head])?;
    }
    Ok(if regressed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
