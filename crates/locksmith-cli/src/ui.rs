//! Terminal styling, diagnostics, progress, atomic writes and diff rendering.
use std::{
    env,
    error::Error,
    fmt,
    io::{self, IsTerminal, Write},
    path::Path,
};

use anstyle::{AnsiColor, Style};
use clap::{ValueEnum, builder::Styles};
use locksmith::LockfileError;
use similar::TextDiff;
use tempfile::NamedTempFile;

use crate::Result;

const DEFAULT_ISSUE_LIMIT: usize = 10;

/// When to emit ANSI colors.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ColorChoice {
    /// Use color when writing to an interactive terminal unless disabled by NO_COLOR or TERM=dumb.
    #[default]
    Auto,
    /// Always emit ANSI color codes.
    Always,
    /// Never emit ANSI color codes.
    Never,
}

/// Shared terminal presentation settings for a single CLI invocation.
#[derive(Clone, Copy, Debug)]
pub struct Ui {
    pub stdout_color: bool,
    pub stderr_color: bool,
    pub stderr_is_tty: bool,
    pub quiet: bool,
    pub verbose: bool,
}

impl Ui {
    pub fn new(color: ColorChoice, quiet: bool, verbose: bool) -> Self {
        let stdout_is_tty = io::stdout().is_terminal();
        let stderr_is_tty = io::stderr().is_terminal();
        Self {
            stdout_color: resolve_color(color, stdout_is_tty),
            stderr_color: resolve_color(color, stderr_is_tty),
            stderr_is_tty,
            quiet,
            verbose,
        }
    }

    pub fn error(&self, message: impl fmt::Display) {
        let style = Style::new().fg_color(Some(AnsiColor::Red.into())).bold();
        self.write_stderr_label("error", style, message);
    }

    pub fn warning(&self, message: impl fmt::Display) {
        if self.quiet {
            return;
        }
        let style = Style::new().fg_color(Some(AnsiColor::Yellow.into())).bold();
        self.write_stderr_label("warning", style, message);
    }

    pub fn hint(&self, message: impl fmt::Display) {
        if self.quiet {
            return;
        }
        let style = Style::new().fg_color(Some(AnsiColor::Cyan.into())).bold();
        self.write_stderr_label("hint", style, message);
    }

    pub fn verbose(&self, message: impl fmt::Display) {
        if !self.verbose || self.quiet {
            return;
        }
        let style = Style::new().fg_color(Some(AnsiColor::Blue.into())).bold();
        self.write_stderr_label("info", style, message);
    }

    pub fn progress(&self, message: impl fmt::Display) {
        if !self.stderr_is_tty || self.quiet {
            return;
        }
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{message}");
        let _ = err.flush();
    }

    pub fn clear_progress(&self) {
        if !self.stderr_is_tty || self.quiet {
            return;
        }
        let mut err = io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K");
        let _ = err.flush();
    }

    pub fn print_error(&self, error: &(dyn Error + 'static)) {
        self.clear_progress();
        match error.downcast_ref::<LockfileError>() {
            Some(LockfileError::Conflicts(conflicts)) if conflicts.len() > 1 => {
                self.error(format!(
                    "lockfile merge has {} {}",
                    conflicts.len(),
                    plural(conflicts.len(), "conflict", "conflicts")
                ));
                self.print_issue_list(conflicts);
            }
            Some(LockfileError::InvalidGraph(issues)) if issues.len() > 1 => {
                self.error(format!(
                    "invalid dependency graph ({} {})",
                    issues.len(),
                    plural(issues.len(), "issue", "issues")
                ));
                self.print_issue_list(issues);
            }
            Some(LockfileError::UnmetPeers(issues)) if issues.len() > 1 => {
                self.error(format!(
                    "peer policy rejects {} {}",
                    issues.len(),
                    plural(issues.len(), "issue", "issues")
                ));
                self.print_issue_list(issues);
            }
            _ => self.error(error),
        }
    }

    fn print_issue_list<T: fmt::Display>(&self, items: &[T]) {
        let limit = if self.verbose {
            items.len()
        } else {
            DEFAULT_ISSUE_LIMIT
        };
        let mut err = io::stderr().lock();
        for item in items.iter().take(limit) {
            let _ = writeln!(err, "  - {item}");
        }
        drop(err);
        if items.len() > limit {
            self.hint(format!(
                "{} more omitted; re-run with --verbose to list all",
                items.len() - limit
            ));
        }
    }

    fn write_stderr_label(&self, label: &str, style: Style, message: impl fmt::Display) {
        let mut err = io::stderr().lock();
        if self.stderr_color {
            let _ = writeln!(err, "{style}{label}:{style:#} {message}");
        } else {
            let _ = writeln!(err, "{label}: {message}");
        }
    }

    pub fn write_unified_diff(
        &self,
        out: &mut impl Write,
        old_label: &str,
        old: &str,
        new_label: &str,
        new: &str,
    ) -> io::Result<()> {
        let diff = TextDiff::from_lines(old, new);
        let rendered = diff
            .unified_diff()
            .context_radius(3)
            .header(old_label, new_label)
            .to_string();
        if !self.stdout_color {
            return out.write_all(rendered.as_bytes());
        }
        let header = Style::new().bold();
        let hunk = Style::new().fg_color(Some(AnsiColor::Cyan.into()));
        let added = Style::new().fg_color(Some(AnsiColor::Green.into()));
        let removed = Style::new().fg_color(Some(AnsiColor::Red.into()));
        for line in rendered.split_inclusive('\n') {
            if line.starts_with("--- ") || line.starts_with("+++ ") {
                write!(out, "{header}{line}{header:#}")?;
            } else if line.starts_with("@@ ") {
                write!(out, "{hunk}{line}{hunk:#}")?;
            } else if line.starts_with('+') {
                write!(out, "{added}{line}{added:#}")?;
            } else if line.starts_with('-') {
                write!(out, "{removed}{line}{removed:#}")?;
            } else {
                out.write_all(line.as_bytes())?;
            }
        }
        Ok(())
    }
}

pub fn plural(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn clap_styles() -> Styles {
    use clap::builder::styling::{AnsiColor, Effects, Style};

    Styles::styled()
        .header(
            Style::new()
                .fg_color(Some(AnsiColor::Green.into()))
                .effects(Effects::BOLD),
        )
        .usage(
            Style::new()
                .fg_color(Some(AnsiColor::Green.into()))
                .effects(Effects::BOLD),
        )
        .literal(
            Style::new()
                .fg_color(Some(AnsiColor::Cyan.into()))
                .effects(Effects::BOLD),
        )
        .placeholder(Style::new().fg_color(Some(AnsiColor::Cyan.into())))
        .valid(
            Style::new()
                .fg_color(Some(AnsiColor::Green.into()))
                .effects(Effects::BOLD),
        )
        .invalid(
            Style::new()
                .fg_color(Some(AnsiColor::Red.into()))
                .effects(Effects::BOLD),
        )
        .error(
            Style::new()
                .fg_color(Some(AnsiColor::Red.into()))
                .effects(Effects::BOLD),
        )
}

fn resolve_color(choice: ColorChoice, is_tty: bool) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            if env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
                return false;
            }
            if env::var_os("TERM").is_some_and(|v| v == "dumb") {
                return false;
            }
            if env::var_os("FORCE_COLOR").is_some_and(|v| !v.is_empty()) {
                return true;
            }
            is_tty
        }
    }
}
