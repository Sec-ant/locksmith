use crate::{Result, workspace};
use locksmith::{
    Disposition, Inspection, Lockfile, PeerRules, PeerStatus, PeerVersionMatching, UnknownReason,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write},
    path::Path,
};

pub fn run(file: &Path, override_version: Option<node_semver::Version>, json: bool) -> Result<()> {
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
        Ok(Some(
            fs::read_to_string(&resolved).map_err(|e| format!("{}: {e}", resolved.display()))?,
        ))
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
        print_report(&mut out, &report)?;
    }
    out.flush()?;
    Ok(())
}

fn print_report(out: &mut impl Write, r: &Inspection) -> io::Result<()> {
    let s = &r.summary;
    writeln!(
        out,
        "{} workspaces, {} package records, {} of {} snapshots reachable",
        s.workspaces, s.packages, s.reachable_snapshots, s.snapshots
    )?;
    writeln!(out, "{} peer finding groups", r.peers.len())?;
    for g in &r.peers {
        let p = &g.requirement;
        let label = match p.status {
            PeerStatus::Missing => "missing",
            PeerStatus::OutOfRange => "out of range",
            PeerStatus::Unknown => "unknown",
            PeerStatus::Allowed => "allowed",
        };
        write!(
            out,
            "\n{label}  {} requires {} {}",
            p.requester, p.peer, p.wanted
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
            };
            write!(out, " ({reason})")?;
        }
        if matches!(p.status, PeerStatus::Allowed | PeerStatus::Unknown) {
            let disposition = match p.disposition {
                Disposition::Accepted => "accepts",
                Disposition::Rejected => "rejects",
                Disposition::Unknown => "undetermined",
            };
            write!(out, " [peer policy: {disposition}]")?;
        }
        writeln!(
            out,
            "\n  {} instances, {} direct workspaces, {} reachable workspaces",
            g.bindings.len(),
            g.direct_workspaces.len(),
            g.reachable_workspaces.len()
        )?;
        if let Some(path) = &g.example_path {
            writeln!(out, "  {}", path.join(" -> "))?;
        }
    }
    if !r.multiplicity.is_empty() {
        writeln!(out, "\nPackages with multiple locked releases or instances")?;
        for p in &r.multiplicity {
            let n: usize = p.releases.iter().map(|v| v.snapshots).sum();
            writeln!(
                out,
                "  {}  {} releases, {n} snapshots",
                p.name,
                p.releases.len()
            )?;
        }
    }
    if !r.external_links.is_empty() {
        writeln!(
            out,
            "\n{} links leave the recorded workspace graph (see --json for paths)",
            r.external_links.len()
        )?;
    }
    Ok(())
}
