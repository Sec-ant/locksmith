use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::Path,
};

use anstyle::{AnsiColor, Style};
use locksmith::{
    Disposition, Inspection, Lockfile, PeerRules, PeerStatus, PeerVersionMatching, UnknownReason,
};

use crate::{
    Result,
    ui::{Ui, plural},
    workspace,
};

pub fn run(
    file: &Path,
    override_version: Option<node_semver::Version>,
    json: bool,
    ui: &Ui,
) -> Result<()> {
    let lock = Lockfile::try_from(
        fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?,
    )?;
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()?;
    let read = |relative: &str| -> Result<Option<String>> {
        let path = dir.join(relative);
        let resolved = match path.canonicalize() {
            Ok(resolved) if resolved.starts_with(&dir) => resolved,
            Ok(_) => return Ok(None),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        workspace::read_regular_file(&resolved)
            .map_err(|e| format!("{}: {e}", resolved.display()).into())
    };
    let root = workspace::read_manifest(".", read)?.unwrap_or_default();
    let version = override_version.or_else(|| {
        locksmith::WorkspaceConfig::project_pnpm_version(&root, lock.recorded_pnpm_version())
    });
    let (config, _) = workspace::settings(&root, version.as_ref(), read)?;
    let rules = PeerRules::from_pnpm(&config, PeerVersionMatching::for_pnpm(version.as_ref()))?;
    let mut manifests = BTreeMap::new();
    for id in lock.linked_peer_projects() {
        if let Some(manifest) = workspace::read_manifest(&id, read)? {
            manifests.insert(id, manifest);
        }
    }
    let report = locksmith::inspect(&lock, &rules, Some(&manifests))?;
    let mut out = io::BufWriter::new(io::stdout().lock());
    if json {
        serde_json::to_writer(&mut out, &report)?;
        writeln!(out)?;
    } else {
        print_report(&mut out, &report, ui.stdout_color)?;
    }
    out.flush()?;
    Ok(())
}

fn print_report(out: &mut impl Write, r: &Inspection, colored: bool) -> io::Result<()> {
    let bold = if colored {
        Style::new().bold()
    } else {
        Style::new()
    };
    let dim = if colored {
        Style::new().dimmed()
    } else {
        Style::new()
    };
    let cyan = if colored {
        Style::new().fg_color(Some(AnsiColor::Cyan.into()))
    } else {
        Style::new()
    };

    let s = &r.summary;
    writeln!(
        out,
        "{}{} {}, {} package {}, {} of {} snapshots reachable{}",
        bold,
        s.workspaces,
        plural(s.workspaces, "workspace", "workspaces"),
        s.packages,
        plural(s.packages, "record", "records"),
        s.reachable_snapshots,
        s.snapshots,
        bold.render_reset(),
    )?;
    writeln!(
        out,
        "{} peer finding {}",
        r.peers.len(),
        plural(r.peers.len(), "group", "groups")
    )?;
    for g in &r.peers {
        let p = &g.requirement;
        let (label, badge_color) = match p.status {
            PeerStatus::Missing => ("missing", AnsiColor::Red),
            PeerStatus::OutOfRange => ("out of range", AnsiColor::Yellow),
            PeerStatus::Unknown => ("unknown", AnsiColor::Magenta),
            PeerStatus::Allowed => ("allowed", AnsiColor::Green),
            _ => ("unknown", AnsiColor::Magenta),
        };
        let badge = if colored {
            Style::new().fg_color(Some(badge_color.into())).bold()
        } else {
            Style::new()
        };
        write!(
            out,
            "\n{badge}{label:<12}{reset}  {bold}{}{bold_reset} requires {} {}",
            p.requester,
            p.peer,
            p.wanted,
            reset = badge.render_reset(),
            bold_reset = bold.render_reset(),
        )?;
        if let Some(v) = &p.resolved_version {
            write!(out, ", bound to {} {v}", p.provider)?;
        }
        if let Some(rule) = &p.exception {
            write!(out, " ({rule})")?;
        }
        if let Some(reason) = p.reason {
            let reason = match reason {
                UnknownReason::ProviderVersion => "provider version unavailable",
                UnknownReason::OpaqueSpecifier => "specifier has no verifiable version range",
                UnknownReason::PackageManager => "resolver behavior depends on pnpm version",
                _ => "unknown reason",
            };
            write!(out, " ({reason})")?;
        }
        if matches!(p.status, PeerStatus::Allowed | PeerStatus::Unknown) {
            let disposition = match p.disposition {
                Disposition::Accepted => "accepts",
                Disposition::Rejected => "rejects",
                Disposition::Unknown => "undetermined",
                _ => "unknown",
            };
            write!(out, " [peer policy: {disposition}]")?;
        }
        writeln!(
            out,
            "\n  {dim}{} {}, {} direct {}, {} reachable {}{dim_reset}",
            g.bindings.len(),
            plural(g.bindings.len(), "instance", "instances"),
            g.direct_workspaces.len(),
            plural(g.direct_workspaces.len(), "workspace", "workspaces"),
            g.reachable_workspaces.len(),
            plural(g.reachable_workspaces.len(), "workspace", "workspaces"),
            dim_reset = dim.render_reset(),
        )?;
        if let Some(path) = &g.example_path {
            let sep = format!("{cyan} -> {reset}", reset = cyan.render_reset());
            writeln!(out, "  {}", path.join(&sep))?;
        }
    }
    if !r.multiplicity.is_empty() {
        writeln!(
            out,
            "\n{bold}Packages with multiple locked releases or instances{reset}",
            reset = bold.render_reset()
        )?;
        let width = r
            .multiplicity
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(0)
            .min(48);
        for p in &r.multiplicity {
            let n: usize = p.releases.iter().map(|v| v.snapshots).sum();
            writeln!(
                out,
                "  {:<width$}  {dim}{} {}, {n} {}{dim_reset}",
                p.name,
                p.releases.len(),
                plural(p.releases.len(), "release", "releases"),
                plural(n, "snapshot", "snapshots"),
                dim_reset = dim.render_reset(),
            )?;
        }
    }
    if !r.external_links.is_empty() {
        writeln!(
            out,
            "\n{} {} leave the recorded workspace graph (see --json for paths)",
            r.external_links.len(),
            plural(r.external_links.len(), "link", "links")
        )?;
    }
    Ok(())
}
