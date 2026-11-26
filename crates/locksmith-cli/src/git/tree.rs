//! Resolve workspace paths against Git tree entries. Only logical workspace names
//! need to be UTF-8; symlink targets and merge-tree conflict paths are raw bytes.
use std::collections::{BTreeMap, BTreeSet};

use super::{Blobs, Result};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectKind {
    Blob,
    Symlink,
    Tree,
    Commit,
}

/// Split ls-tree -z records before decoding the ASCII header.
pub(crate) fn entries(listing: &[u8]) -> impl Iterator<Item = Result<(ObjectKind, &str, &[u8])>> {
    listing
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|record| {
            let tab = record
                .iter()
                .position(|&b| b == b'\t')
                .ok_or("invalid Git tree entry")?;
            let (header, name) = record.split_at(tab);
            let mut fields = std::str::from_utf8(header)?.split_whitespace();
            let kind = match (fields.next(), fields.next()) {
                (Some("120000"), Some("blob")) => ObjectKind::Symlink,
                (_, Some("blob")) => ObjectKind::Blob,
                (_, Some("tree")) => ObjectKind::Tree,
                (_, Some("commit")) => ObjectKind::Commit,
                _ => return Err("invalid Git entry mode or type".into()),
            };
            let id = fields.next().ok_or("missing Git object ID")?;
            Ok((kind, id, &name[1..]))
        })
}

pub struct LinkedObject {
    pub kind: ObjectKind,
    pub id: String,
    pub physical_path: Vec<u8>,
    pub links: usize,
}

#[derive(Clone)]
pub(super) struct TreeEntry {
    kind: ObjectKind,
    id: String,
}

impl Blobs {
    fn entry(&mut self, tree: &str, name: &[u8]) -> Result<Option<TreeEntry>> {
        if let Some(directory) = self.directories.get(tree) {
            return Ok(directory.get(name).cloned());
        }
        let config_names = || {
            crate::workspace::CONFIG_FILES
                .iter()
                .copied()
                .chain(crate::workspace::DEFAULT_HOOK_FILES)
                .chain(["pnpm-lock.yaml"])
        };
        let restricted = config_names().any(|path| path.as_bytes() == name);
        if restricted && let Some(directory) = self.config_entries.get(tree) {
            return Ok(directory.get(name).cloned());
        }
        let object = format!("{tree}^{{tree}}");
        let (listing, digest_len) = self
            .object(&object, "tree")?
            .ok_or_else(|| format!("missing Git tree at {tree}"))?;
        let directory = read_tree(&listing, digest_len, |name| {
            !restricted || config_names().any(|path| path.as_bytes() == name)
        })?;
        let entry = directory.get(name).cloned();
        if restricted {
            self.config_entries.insert(tree.to_owned(), directory);
        } else {
            self.directories.insert(tree.to_owned(), directory);
        }
        Ok(entry)
    }

    pub fn read(&mut self, tree: &str, path: &str) -> Result<Option<String>> {
        let mut location = TreePath::new(tree);
        if !location.follow(self, path.as_bytes())?.inside(path)? {
            return Ok(None);
        }
        let entry = location.current();
        if entry.kind != ObjectKind::Blob {
            return Err(format!("expected a Git blob at {tree}:{path}").into());
        }
        self.read_object(&entry.id)
    }

    pub fn linked_object(
        &mut self,
        tree: &str,
        prefix: &str,
        path: &str,
    ) -> Result<Option<LinkedObject>> {
        let mut location = TreePath::new(tree);
        if !location.follow(self, prefix.as_bytes())?.inside(prefix)? {
            return Ok(None);
        }
        let mut ancestors = BTreeSet::from([location.physical_path()]);
        for part in path.split('/') {
            if !location.follow(self, part.as_bytes())?.inside(path)?
                || (location.current().kind == ObjectKind::Tree
                    && !ancestors.insert(location.physical_path()))
            {
                return Ok(None);
            }
        }
        let target = location.current();
        Ok(Some(LinkedObject {
            kind: target.kind,
            id: target.id.clone(),
            physical_path: location.physical_path(),
            links: location.links,
        }))
    }

    pub fn check_linked_conflicts(
        &mut self,
        tree: &str,
        inputs: impl Iterator<Item = String>,
    ) -> Result<()> {
        for path in inputs {
            TreePath::new(tree).follow(self, path.as_bytes())?;
        }
        Ok(())
    }
}

fn read_tree(
    mut bytes: &[u8],
    digest_len: usize,
    contains: impl Fn(&[u8]) -> bool,
) -> Result<BTreeMap<Vec<u8>, TreeEntry>> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut entries = BTreeMap::new();
    while !bytes.is_empty() {
        // A raw entry is octal mode, space, filename, NUL, then the binary object ID.
        let nul = bytes
            .iter()
            .position(|&byte| byte == 0)
            .ok_or("invalid Git tree entry")?;
        let header = &bytes[..nul];
        let space = header
            .iter()
            .position(|&byte| byte == b' ')
            .ok_or("invalid Git tree entry")?;
        let (digest, rest) = bytes[nul + 1..]
            .split_at_checked(digest_len)
            .ok_or("truncated Git tree object ID")?;
        bytes = rest;
        let name = &header[space + 1..];
        if !contains(name) {
            continue;
        }
        let mode = u32::from_str_radix(std::str::from_utf8(&header[..space])?, 8)?;
        let kind = match mode & 0o170000 {
            0o040000 => ObjectKind::Tree,
            0o120000 => ObjectKind::Symlink,
            0o160000 => ObjectKind::Commit,
            _ => ObjectKind::Blob,
        };
        let mut id = String::with_capacity(digest_len * 2);
        for &byte in digest {
            id.push(HEX[usize::from(byte >> 4)] as char);
            id.push(HEX[usize::from(byte & 0xf)] as char);
        }
        entries.insert(name.to_vec(), TreeEntry { kind, id });
    }
    Ok(entries)
}

#[derive(PartialEq, Eq)]
enum Destination {
    Found,
    Missing,
    Outside,
}

impl Destination {
    fn inside(self, logical: &str) -> Result<bool> {
        match self {
            Self::Found => Ok(true),
            Self::Missing => Ok(false),
            Self::Outside => {
                Err(format!("linked path {logical} leaves the committed Git tree").into())
            }
        }
    }
}

struct TreePath {
    root: TreeEntry,
    parts: Vec<(Vec<u8>, TreeEntry)>,
    links: usize,
}

impl TreePath {
    fn new(tree: &str) -> Self {
        Self {
            root: TreeEntry {
                kind: ObjectKind::Tree,
                id: tree.to_owned(),
            },
            parts: Vec::new(),
            links: 0,
        }
    }

    fn current(&self) -> &TreeEntry {
        self.parts.last().map_or(&self.root, |(_, entry)| entry)
    }

    fn physical_path(&self) -> Vec<u8> {
        self.parts
            .iter()
            .map(|(name, _)| name.as_slice())
            .collect::<Vec<_>>()
            .join(&b'/')
    }

    fn follow(&mut self, blobs: &mut Blobs, path: &[u8]) -> Result<Destination> {
        if path.starts_with(b"/") {
            return Ok(Destination::Outside);
        }
        for part in path.split(|&b| b == b'/') {
            if self.current().kind != ObjectKind::Tree {
                return Ok(Destination::Missing);
            }
            match part {
                b"" | b"." => continue,
                b".." => {
                    if self.parts.pop().is_none() {
                        return Ok(Destination::Outside);
                    }
                    continue;
                }
                _ => (),
            }
            if !blobs.conflicts.is_empty() {
                let mut current = self.physical_path();
                if !current.is_empty() {
                    current.push(b'/');
                }
                current.extend_from_slice(part);
                if blobs.conflicts.contains(&current) {
                    return Err(format!(
                        "resolve dependency configuration first:\n  {}",
                        String::from_utf8_lossy(&current)
                    )
                    .into());
                }
            }
            let Some(entry) = blobs.entry(&self.current().id, part)? else {
                return Ok(Destination::Missing);
            };
            if entry.kind == ObjectKind::Symlink {
                self.links += 1;
                // Match Git's bounded symlink traversal, including for cycles in committed trees.
                if self.links > 40 {
                    return Ok(Destination::Missing);
                }
                let target = blobs.bytes(&entry.id)?.ok_or("missing Git symlink blob")?;
                if target.is_empty() {
                    return Ok(Destination::Missing);
                }
                let destination = self.follow(blobs, &target)?;
                if destination != Destination::Found {
                    return Ok(destination);
                }
            } else {
                self.parts.push((part.to_vec(), entry));
            }
        }
        Ok(Destination::Found)
    }
}
