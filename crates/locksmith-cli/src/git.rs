//! Read the committed merge inputs without consulting a partially updated index.
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
};

use super::Result;

pub fn output(args: &[&str]) -> Result<String> {
    checked(Command::new("git").args(args))
}

fn checked(command: &mut Command) -> Result<String> {
    checked_output(command.output()?)
}

fn checked_output(output: Output) -> Result<String> {
    if !output.status.success() {
        return Err(failure(&output).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn failure(output: &Output) -> String {
    format!(
        "git exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn merge_base(ours: &str, theirs: &str) -> Result<String> {
    let base = output(&["merge-base", "--all", ours, theirs])?;
    if !oid(base.trim()) {
        return Err("Git did not identify a unique merge base".into());
    }
    Ok(base.trim().to_owned())
}

pub struct DriverInputs {
    pub revisions: [String; 3],
    replay: bool,
}

fn replay_commit() -> Result<String> {
    let dir = output(&["rev-parse", "--absolute-git-dir"])?;
    let dir = std::path::Path::new(dir.trim_end());
    let saved = if dir.join("rebase-merge").is_dir() {
        let done = std::fs::read_to_string(dir.join("rebase-merge/done"))?;
        let mut fields = done.lines().last().unwrap_or_default().split_whitespace();
        match fields.next() {
            Some("pick" | "p" | "edit" | "e" | "reword" | "r") => fields.next(),
            _ => None,
        }
        .ok_or("cannot identify the commit in this rebase step")?
        .to_owned()
    } else if dir.join("rebase-apply").is_dir() {
        std::fs::read_to_string(dir.join("rebase-apply/original-commit"))?
            .trim()
            .to_owned()
    } else {
        return Err("Git did not identify the incoming merge commit".into());
    };
    if !oid(&saved) {
        return Err("invalid rebase commit".into());
    }
    Ok(saved)
}

pub fn revisions(files: [&OsString; 3], path: &str) -> Result<DriverInputs> {
    // git merge exports one GITHEAD_<object id> for each incoming head.
    let mut heads = env::vars_os().filter_map(|(key, _)| {
        let head = key.to_str()?.strip_prefix("GITHEAD_")?;
        oid(head).then(|| head.to_owned())
    });
    let incoming = heads.next();
    if heads.next().is_some() {
        return Err("the driver requires a two-head git merge".into());
    }
    let current = output(&["rev-parse", "--verify", "HEAD"])?
        .trim()
        .to_owned();
    let replay = incoming.is_none();
    let (base, incoming) = if let Some(incoming) = incoming {
        (merge_base(&current, &incoming)?, incoming)
    } else {
        let incoming = replay_commit()?;
        let parents = output(&["rev-list", "--parents", "-1", &incoming])?;
        let fields: Vec<_> = parents.split_whitespace().collect();
        let [_, parent] = fields.as_slice() else {
            return Err("expected a single-parent rebase commit".into());
        };
        (parent.to_string(), incoming)
    };
    let revisions = [base, current, incoming];
    let hashes = checked(
        Command::new("git")
            .args(["hash-object", "--no-filters", "--"])
            .args(files),
    )?;
    let names = revisions.each_ref().map(|r| format!("{r}:{path}"));
    let expected = output(&["rev-parse", &names[0], &names[1], &names[2]])?;
    if hashes != expected {
        return Err(format!("Git's lock inputs do not match the merge revisions at {path}").into());
    }
    Ok(DriverInputs { revisions, replay })
}

fn mainline_tips() -> Result<Vec<String>> {
    let configured = Command::new("git")
        .args(["config", "--null", "--get", "locksmith.mainline"])
        .output()?;
    match configured.status.code() {
        Some(0) => {
            let value = String::from_utf8(configured.stdout)?;
            let reference = value.trim_end_matches('\0');
            let tip = output(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ])
            .map_err(|err| format!("invalid locksmith.mainline {reference:?}: {err}"))?;
            return Ok(vec![tip.trim_end().to_owned()]);
        }
        Some(1) => (),
        _ => return Err(failure(&configured).into()),
    }

    let remote = Command::new("git")
        .args(["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .output()?;
    let refs = match remote.status.code() {
        Some(0) => {
            let remote = String::from_utf8(remote.stdout)?.trim().to_owned();
            let name = remote
                .strip_prefix("refs/remotes/origin/")
                .ok_or("invalid origin/HEAD")?;
            vec![format!("refs/heads/{name}"), remote]
        }
        Some(1) => return Ok(Vec::new()),
        _ => return Err(failure(&remote).into()),
    };
    let tips = checked(
        Command::new("git")
            .args(["for-each-ref", "--format=%(refname) %(objectname)"])
            .args(&refs),
    )?;
    Ok(tips
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(r, _)| refs.iter().any(|wanted| wanted == r))
        .map(|(_, id)| id.to_owned())
        .collect())
}

/// Prefer a side on mainline history; keep ours for other ties.
pub fn preferred_target(inputs: &DriverInputs) -> Result<locksmith::MergeTarget> {
    let tips = mainline_tips()?;
    let [_, ours, theirs] = &inputs.revisions;
    let on_main = |commit: &str| -> Result<bool> {
        for tip in &tips {
            if commit == tip.as_str() {
                return Ok(true);
            }
            let result = Command::new("git")
                .args(["merge-base", "--is-ancestor", commit, tip])
                .output()?;
            match result.status.code() {
                Some(0) => return Ok(true),
                Some(1) => (),
                _ => return Err(failure(&result).into()),
            }
        }
        Ok(false)
    };
    Ok(
        if !tips.contains(ours) && (tips.contains(theirs) || (on_main(theirs)? && !on_main(ours)?))
        {
            locksmith::MergeTarget::Theirs
        } else {
            locksmith::MergeTarget::Ours
        },
    )
}

pub fn merged_tree(inputs: &DriverInputs) -> Result<(String, BTreeSet<String>)> {
    let [base, ours, theirs] = &inputs.revisions;
    // Git 2.38 can preview a merge but cannot specify its base explicitly.
    // Give HEAD's tree the original parent for this preview only. No ref moves.
    let preview_head = if inputs.replay {
        output(&[
            "-c",
            "user.name=locksmith",
            "-c",
            "user.email=locksmith@localhost",
            "commit-tree",
            &format!("{ours}^{{tree}}"),
            "-p",
            base,
            "-m",
            "locksmith rebase preview",
        ])?
    } else {
        ours.clone()
    };
    let mut command = Command::new("git");
    // The preview must neither call us recursively nor execute other external
    // drivers twice. Their overlapping inputs remain conflicts in the preview.
    let drivers = Command::new("git")
        .args([
            "config",
            "-z",
            "--name-only",
            "--get-regexp",
            r"^merge\..*\.driver$",
        ])
        .output()?;
    if !matches!(drivers.status.code(), Some(0 | 1)) {
        return Err(failure(&drivers).into());
    }
    for key in std::str::from_utf8(&drivers.stdout)?.split_terminator('\0') {
        command.arg("-c").arg(format!("{key}=false"));
    }
    let output = command
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            preview_head.trim(),
            theirs,
        ])
        .output()?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(failure(&output).into());
    }
    let mut records = std::str::from_utf8(&output.stdout)?.split_terminator('\0');
    let tree = records
        .next()
        .filter(|s| oid(s))
        .ok_or("invalid git merge-tree result")?;
    Ok((
        tree.into(),
        records
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    ))
}

pub fn workspace_paths(tree: &str, prefix: &str) -> Result<String> {
    let scope = if prefix.is_empty() { "." } else { prefix };
    output(&[
        "ls-tree",
        "--full-tree",
        "-rz",
        "--name-only",
        tree,
        "--",
        scope,
    ])
}

/// In a blobless clone, cat-file lazily fetches one missing object per request.
/// Fetch just the selected manifests together, leaving Git's usual promisor
/// selection in charge when multiple remotes are configured.
pub fn prefetch_manifests(tree: &str, prefix: &str, paths: &BTreeMap<&str, &str>) -> Result<()> {
    if paths.is_empty() || env::var_os("GIT_NO_LAZY_FETCH").is_some() {
        return Ok(());
    }
    let remotes = Command::new("git")
        .args([
            "config",
            "--null",
            "--type=bool",
            "--get-regexp",
            r"^remote\..*\.promisor$",
        ])
        .output()?;
    if remotes.status.code() == Some(1) {
        return Ok(());
    }
    if !remotes.status.success() {
        return Err(failure(&remotes).into());
    }
    let mut names = std::str::from_utf8(&remotes.stdout)?
        .split_terminator('\0')
        .filter_map(|record| {
            record
                .strip_suffix(".promisor\ntrue")?
                .strip_prefix("remote.")
        });
    let Some(remote) = names.next() else {
        return Ok(());
    };
    if names.next().is_some() {
        return Ok(());
    }

    let mut query = format!("{tree}^{{tree}}\n--\n");
    for path in paths.values() {
        if path.contains(['\r', '\n']) || prefix.contains(['\r', '\n']) {
            return Err(format!("unsupported configuration path {prefix}{path:?}").into());
        }
        query.push_str(&format!(":(top,literal){prefix}{path}\n"));
    }
    let listing = with_input(
        &[
            "rev-list",
            "--objects",
            "--no-object-names",
            "--missing=print",
            "--stdin",
        ],
        query.as_bytes(),
    )?;
    let objects = checked_output(listing)?;
    let mut missing = String::new();
    for id in objects.lines().filter_map(|line| line.strip_prefix('?')) {
        if !oid(id) {
            return Err("invalid missing Git object ID".into());
        }
        missing.push_str(id);
        missing.push('\n');
    }
    if !missing.is_empty() {
        // A batch can miss objects served by another promisor. cat-file reads
        // each required manifest next, using Git's native remote selection.
        with_input(
            &[
                "-c",
                "fetch.negotiationAlgorithm=noop",
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-prune",
                "--refmap=",
                "--recurse-submodules=no",
                "--filter=blob:none",
                "--stdin",
                "--",
                remote,
            ],
            missing.as_bytes(),
        )?;
    }
    Ok(())
}

fn with_input(args: &[&str], input: &[u8]) -> Result<Output> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let written = child
        .stdin
        .take()
        .expect("piped Git stdin")
        .write_all(input);
    let output = child.wait_with_output()?;
    if output.status.success() {
        written?;
    }
    Ok(output)
}

/// The single cat-file process reads just the configuration blobs requested.
pub struct Blobs {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Blobs {
    pub fn open() -> Result<Self> {
        let mut child = Command::new("git")
            .args(["cat-file", "--batch", "--follow-symlinks"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().expect("piped git stdin");
        let output = BufReader::new(child.stdout.take().expect("piped git stdout"));
        Ok(Self {
            child,
            input,
            output,
        })
    }

    pub fn read(&mut self, tree: &str, path: &str) -> Result<Option<String>> {
        if path.contains(['\n', '\r', '\0']) {
            return Err(format!("unsupported configuration path {path:?}").into());
        }
        let input = &mut self.input;
        writeln!(input, "{tree}:{path}")?;
        input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        if header.trim_end() == format!("{tree}:{path} missing") {
            return Ok(None);
        }
        let mut fields = header.split_whitespace();
        if !fields.next().is_some_and(oid) || fields.next() != Some("blob") {
            return Err(format!("expected a Git blob at {tree}:{path}").into());
        }
        let length = fields
            .next()
            .ok_or("missing git blob length")?
            .parse::<usize>()?;
        let mut bytes = vec![0; length];
        self.output.read_exact(&mut bytes)?;
        let mut delimiter = [0];
        self.output.read_exact(&mut delimiter)?;
        if delimiter[0] != b'\n' {
            return Err("invalid git cat-file response".into());
        }
        Ok(Some(
            String::from_utf8(bytes).map_err(|e| format!("{path}: {e}"))?,
        ))
    }
}
impl Drop for Blobs {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
