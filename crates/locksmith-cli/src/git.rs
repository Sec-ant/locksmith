//! Read the committed merge inputs without consulting a partially updated index.
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
};

use super::Result;

mod tree;
use tree::TreeEntry;
pub(crate) use tree::{ObjectKind, entries};

pub fn output(args: &[&str]) -> Result<String> {
    checked(Command::new("git").args(args))
}

pub fn revision_and_prefix(reference: &str) -> Result<(String, String)> {
    let resolved = output(&[
        "rev-parse",
        "--show-prefix",
        "--verify",
        "--end-of-options",
        &format!("{reference}^{{commit}}"),
    ])?;
    // The prefix may itself contain newlines, so split at the verified OID.
    let (prefix, revision) = resolved
        .trim_end_matches('\n')
        .rsplit_once('\n')
        .ok_or("invalid Git revision and prefix output")?;
    Ok((revision.to_owned(), prefix.to_owned()))
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

pub fn merged_tree(inputs: &DriverInputs) -> Result<(String, BTreeSet<Vec<u8>>)> {
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
    let mut records = output.stdout.split(|&b| b == 0);
    let tree = records
        .next()
        .and_then(|field| std::str::from_utf8(field).ok())
        .filter(|s| oid(s))
        .ok_or("invalid git merge-tree result")?;
    Ok((
        tree.into(),
        records
            .filter(|field| !field.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    ))
}

/// Keep NUL-delimited Git path lists as bytes until the caller selects its inputs.
pub fn output_bytes(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        return Err(failure(&output).into());
    }
    Ok(output.stdout)
}

pub fn stream_paths(
    args: &[&str],
    visit: impl FnOnce(&mut BufReader<ChildStdout>) -> Result<()>,
) -> Result<()> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut paths = BufReader::new(child.stdout.take().expect("piped Git stdout"));
    std::thread::scope(|scope| {
        // Drain stderr while the caller consumes stdout.
        let child = scope.spawn(move || child.wait_with_output());
        let visited = visit(&mut paths);
        if visited.is_err() {
            // Keep Git's exit status meaningful if the caller stops reading early.
            let _ = io::copy(&mut paths, &mut io::sink());
        }
        drop(paths);
        let output = child.join().expect("Git output reader panicked")?;
        if !output.status.success() {
            return Err(failure(&output).into());
        }
        visited
    })
}

fn lazy_fetch_disabled() -> Result<bool> {
    if env::var_os("GIT_NO_LAZY_FETCH").is_none() {
        return Ok(false);
    }
    let value = output(&[
        "--config-env=locksmith.noLazyFetch=GIT_NO_LAZY_FETCH",
        "config",
        "--type=bool",
        "--get",
        "locksmith.noLazyFetch",
    ])?;
    Ok(value.trim() == "true")
}

/// Fetch missing selected blobs together on a blobless clone, including link targets.
pub fn prefetch_manifests<'a>(objects: impl ExactSizeIterator<Item = &'a str>) -> Result<()> {
    if objects.len() == 0 || lazy_fetch_disabled()? {
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

    let mut missing: BTreeSet<_> = objects.collect();
    let query = missing.iter().copied().collect::<Vec<_>>().join("\n");
    // On Git 2.38, --missing disables lazy fetches and --ignore-missing skips
    // absent input tips. All tips are selected blob IDs, so the walk ends at each one.
    let available = checked_output(with_input(
        &[
            "rev-list",
            "--objects",
            "--no-object-names",
            "--missing=allow-any",
            "--ignore-missing",
            "--stdin",
        ],
        query.as_bytes(),
    )?)?;
    for id in available.lines() {
        missing.remove(id);
    }
    if !missing.is_empty() {
        let missing = missing.into_iter().collect::<Vec<_>>().join("\n");
        // If the batch is incomplete, the following cat-file read retries each
        // required blob and reports any manifest that remains unavailable.
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
    let mut command = Command::new("git");
    command.args(args);
    command_with_input(command, input)
}

fn command_with_input(mut command: Command, input: &[u8]) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped Git stdin");
    std::thread::scope(|scope| {
        let written = scope.spawn(move || stdin.write_all(input));
        let output = child.wait_with_output()?;
        let written = written.join().expect("Git input writer panicked");
        if output.status.success() {
            written?;
        }
        Ok(output)
    })
}

/// The single cat-file process reads just the configuration blobs requested.
pub struct Blobs {
    pub conflicts: BTreeSet<Vec<u8>>,
    config_files: &'static [&'static str],
    hook_files: &'static [&'static str],
    directories: BTreeMap<String, BTreeMap<Vec<u8>, TreeEntry>>,
    config_entries: BTreeMap<String, BTreeMap<Vec<u8>, TreeEntry>>,
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}
impl Blobs {
    pub fn open(
        config_files: &'static [&'static str],
        hook_files: &'static [&'static str],
    ) -> Result<Self> {
        Self::spawn(Command::new("git"), config_files, hook_files)
    }

    fn spawn(
        mut command: Command,
        config_files: &'static [&'static str],
        hook_files: &'static [&'static str],
    ) -> Result<Self> {
        let mut child = command
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = Some(child.stdin.take().expect("piped git stdin"));
        let output = BufReader::new(child.stdout.take().expect("piped git stdout"));
        Ok(Self {
            conflicts: BTreeSet::new(),
            config_files,
            hook_files,
            directories: BTreeMap::new(),
            config_entries: BTreeMap::new(),
            child,
            input,
            output,
        })
    }

    pub fn read_object(&mut self, object: &str) -> Result<Option<String>> {
        self.bytes(object)?
            .map(|bytes| String::from_utf8(bytes).map_err(|e| format!("{object}: {e}").into()))
            .transpose()
    }

    fn bytes(&mut self, object: &str) -> Result<Option<Vec<u8>>> {
        let input = self.input.as_mut().expect("cat-file stdin open");
        writeln!(input, "{object}")?;
        input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        if header.trim_end() == format!("{object} missing") {
            return Ok(None);
        }
        let mut fields = header.split_whitespace();
        let object_id = fields.next();
        let kind = fields.next();
        let length = fields.next().and_then(|s| s.parse::<usize>().ok());
        if !object_id.is_some_and(oid) || kind.is_none() {
            return Err(format!("expected a Git blob at {object}").into());
        }
        let Some(length) = length else {
            return Err("missing git blob length".into());
        };
        if kind != Some("blob") {
            // Drain the non-blob payload + trailing newline so subsequent reads stay aligned.
            let mut remaining = length + 1;
            let mut scratch = [0u8; 4096];
            while remaining > 0 {
                let chunk = remaining.min(scratch.len());
                self.output.read_exact(&mut scratch[..chunk])?;
                remaining -= chunk;
            }
            return Err(format!("expected a Git blob at {object}").into());
        }
        let mut bytes = vec![0; length];
        self.output.read_exact(&mut bytes)?;
        let mut delimiter = [0];
        self.output.read_exact(&mut delimiter)?;
        if delimiter[0] != b'\n' {
            return Err("invalid git cat-file response".into());
        }
        Ok(Some(bytes))
    }
}
impl Drop for Blobs {
    fn drop(&mut self) {
        drop(self.input.take());
        if self.child.wait().is_err() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_stays_synchronized_after_non_blob_object() {
        let dir = tempfile::tempdir().unwrap();
        let git = || {
            let mut command = Command::new("git");
            command
                .current_dir(dir.path())
                .env_clear()
                .env("PATH", env::var_os("PATH").unwrap())
                .env("HOME", dir.path())
                .env("XDG_CONFIG_HOME", dir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1");
            command
        };
        checked(git().args(["init", "-q"])).unwrap();
        let contents = "known blob contents\n";
        std::fs::write(dir.path().join("fixture.txt"), contents).unwrap();
        checked(git().args(["add", "fixture.txt"])).unwrap();
        checked(git().args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "-qm",
            "fixture",
        ]))
        .unwrap();

        let head = checked(git().args(["rev-parse", "HEAD"])).unwrap();
        let tree = checked(git().args(["rev-parse", "HEAD^{tree}"])).unwrap();
        let mut blobs = Blobs::spawn(git(), &[], &[]).unwrap();
        for object in [head.trim(), tree.trim()] {
            let error = blobs.read_object(object).unwrap_err().to_string();
            assert!(error.contains("expected a Git blob"));
        }
        assert_eq!(
            blobs
                .read_object(&format!("{}:fixture.txt", head.trim()))
                .unwrap()
                .as_deref(),
            Some(contents)
        );
    }
}
