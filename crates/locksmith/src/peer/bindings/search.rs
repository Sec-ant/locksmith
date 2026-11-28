//! Test scoped peer candidates in pnpm identity order, within a fixed search budget.
use super::{Bindings, optional_requirement};
use crate::{LinkedPeerEncoding, Value, model::graph};
use crate::{
    model::checksum::{HashStyle, digest},
    model::pin,
    peer::identity::peer_id,
};
use sha2::{Digest, Sha256};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
};

const SEARCH_LIMIT: usize = 4096;

pub(super) fn recover(
    doc: &Value,
    key: &str,
    direct: &BTreeMap<String, String>,
    found: &BTreeMap<&str, BTreeSet<String>>,
    hash: &str,
    style: Option<HashStyle>,
    parallelism: &OnceCell<usize>,
) -> Option<Bindings> {
    let inherited: Vec<_> = found.keys().copied().collect();
    let fingerprint = Fingerprint::new(hash, style);
    for allow_omissions in [false, true] {
        if allow_omissions && inherited.is_empty() {
            break;
        }
        let optional: Vec<_> = if allow_omissions {
            inherited
                .iter()
                .copied()
                .filter(|name| {
                    optional_requirement(
                        key,
                        name,
                        graph::section(doc, "snapshots").expect("snapshots"),
                        graph::section(doc, "packages").expect("packages"),
                    ) == Some(true)
                })
                .collect()
        } else {
            Vec::new()
        };
        for flat in [false, true] {
            let mut budget = SEARCH_LIMIT;
            let domains: Vec<_> = found
                .iter()
                .map(|(name, values)| {
                    let choices = if flat {
                        let mut versions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
                        for value in values {
                            versions
                                .entry(pin::package_key(value))
                                .or_default()
                                .push(value);
                        }
                        versions
                            .into_iter()
                            .map(|(version, contexts)| {
                                let ambiguous = contexts.len() > 1;
                                (if ambiguous { version } else { contexts[0] }, ambiguous)
                            })
                            .collect::<Vec<_>>()
                    } else {
                        values.iter().map(|v| (v.as_str(), false)).collect()
                    };
                    (*name, choices)
                })
                .collect();
            // Resolve small, scoped ambiguities by verifying the original digest.
            // Bound the search rather than enumerating an exponential graph-wide
            // product. Memory is proportional to the observed candidate bindings.
            let count = domains.iter().try_fold(1usize, |count, (_, choices)| {
                count
                    .checked_mul(choices.len())
                    .filter(|n| *n <= SEARCH_LIMIT)
            });
            let Some(count) = count else {
                continue;
            };
            // Each release choice has a stable spelling for this encoding. Share
            // it across the candidate product and materialize only the winner.
            let fixed_ids: Vec<_> = direct
                .iter()
                .filter(|(name, _)| name.as_str() != "patch_hash")
                .map(|(name, value)| Candidate::new(name, value, false, flat))
                .collect();
            let domains: Vec<Vec<_>> = domains
                .into_iter()
                .map(|(name, choices)| {
                    choices
                        .into_iter()
                        .map(|(value, ambiguous)| Candidate::new(name, value, ambiguous, flat))
                        .collect()
                })
                .collect();
            let mut plans = Vec::new();
            // Deduplicated children can acquire bindings absent from their
            // parent's identity, including required peers. The original digest
            // proves participation. Optional bindings are the cheapest known
            // omission domain. Broader inference uses the remaining budget and
            // tries small sets across releases before expanding them further.
            let mut groups = vec![(&optional[..], true, None, None)];
            if allow_omissions {
                // A deduplicated descendant can add a required provider along
                // with an optional one. Prioritize those mixed pairs before
                // spending the remaining budget on arbitrary inherited sets.
                for size in 0..=usize::from(!optional.is_empty()) {
                    for name in inherited.iter().filter(|name| !optional.contains(name)) {
                        groups.push((&optional[..], false, Some(*name), Some(size)));
                    }
                }
            }
            groups.push((&inherited[..], false, None, None));
            'groups: for (names, release_first, fixed, fixed_size) in groups {
                if (!allow_omissions && release_first)
                    || (allow_omissions
                        && fixed.is_none()
                        && (names.is_empty() || (!release_first && names == optional)))
                {
                    continue;
                }
                let sizes = if allow_omissions && fixed_size.is_none() {
                    names.len()
                } else {
                    1
                };
                for attempt in 0..count * sizes {
                    let (combination, offset) = if release_first {
                        (attempt / sizes, attempt % sizes)
                    } else {
                        (attempt % count, attempt / count)
                    };
                    let size = if allow_omissions {
                        fixed_size.unwrap_or(offset + 1)
                    } else {
                        0
                    };
                    let limit = combinations(names.len(), size, budget);
                    plans.push(Probe {
                        names,
                        fixed,
                        size,
                        combination,
                        start: SEARCH_LIMIT - budget,
                        count: limit,
                    });
                    budget -= limit;
                    if budget == 0 {
                        break 'groups;
                    }
                }
            }
            let search = |range: std::ops::Range<usize>| {
                let mut text = String::new();
                let mut prefix = Sha256Prefix::default();
                for plan in &plans {
                    if range.end <= plan.start {
                        break;
                    }
                    let begin = range.start.max(plan.start);
                    let end = range.end.min(plan.start + plan.count);
                    if begin >= end {
                        continue;
                    }
                    let mut combination = plan.combination;
                    let mut selected: Vec<_> = fixed_ids.iter().collect();
                    for choices in &domains {
                        let choice = &choices[combination % choices.len()];
                        combination /= choices.len();
                        if plan.fixed != Some(choice.name) {
                            selected.push(choice);
                        }
                    }
                    if let Some((omitted, linked_peer_encoding)) = match_candidates(
                        &selected,
                        plan.names,
                        plan.size,
                        |text| fingerprint.matches(text, &mut prefix),
                        &mut text,
                        begin - plan.start..end - plan.start,
                    ) {
                        let mut peers = direct.clone();
                        let mut versions_only = BTreeSet::new();
                        for choice in selected {
                            if omitted.iter().any(|i| plan.names[*i] == choice.name) {
                                continue;
                            }
                            peers.insert(choice.name.into(), choice.value.into());
                            if choice.ambiguous {
                                versions_only.insert(choice.name.into());
                            }
                        }
                        return Some((peers, linked_peer_encoding, versions_only));
                    }
                }
                None
            };
            let probes = SEARCH_LIMIT - budget;
            let matched = if probes < 1024 {
                search(0..probes)
            } else if let Some(matched) = search(0..64) {
                Some(matched)
            } else {
                let workers = *parallelism.get_or_init(|| {
                    std::thread::available_parallelism().map_or(1, |p| p.get().min(4))
                });
                let width = (probes - 64).div_ceil(workers);
                std::thread::scope(|scope| {
                    let mut tasks = Vec::new();
                    for start in (64 + width..probes).step_by(width) {
                        let search = &search;
                        let end = (start + width).min(probes);
                        tasks.push((
                            start..end,
                            std::thread::Builder::new()
                                .spawn_scoped(scope, move || search(start..end)),
                        ));
                    }
                    let mut first = search(64..(64 + width).min(probes));
                    for (range, task) in tasks {
                        let matched = match task {
                            Ok(worker) => worker.join().expect("peer search worker"),
                            Err(_) if first.is_none() => search(range),
                            Err(_) => None,
                        };
                        if first.is_none() {
                            first = matched;
                        }
                    }
                    first
                })
            };
            if let Some((peers, linked_peer_encoding, versions_only)) = matched {
                return Some(Bindings {
                    peers: peers.into(),
                    hash: style,
                    flat,
                    linked_peer_encoding,
                    versions_only,
                });
            }
        }
    }
    None
}

enum Fingerprint<'a> {
    Text(&'a str),
    Sha256(Option<[u8; 16]>),
    Md5Base32(&'a str),
}

impl<'a> Fingerprint<'a> {
    fn new(expected: &'a str, style: Option<HashStyle>) -> Self {
        match style {
            None => Self::Text(expected),
            Some(HashStyle::Md5Base32) => Self::Md5Base32(expected),
            Some(HashStyle::Sha256) => {
                // pnpm emits lowercase hex; preserve the exact spelling check.
                let bytes = (!expected.bytes().any(|b| b.is_ascii_uppercase())).then(|| {
                    let mut bytes = [0; 16];
                    let (pairs, _) = expected.as_bytes().as_chunks::<2>();
                    for (byte, pair) in bytes.iter_mut().zip(pairs) {
                        *byte = u8::from_str_radix(
                            std::str::from_utf8(pair).expect("hexadecimal fingerprint"),
                            16,
                        )
                        .expect("hexadecimal byte");
                    }
                    bytes
                });
                Self::Sha256(bytes)
            }
        }
    }
    fn matches(&self, text: &str, prefix: &mut Sha256Prefix) -> bool {
        match self {
            Self::Text(expected) => text == *expected,
            Self::Sha256(bytes) => {
                bytes.is_some_and(|expected| prefix.digest(text.as_bytes())[..16] == expected)
            }
            Self::Md5Base32(expected) => digest(text, HashStyle::Md5Base32) == *expected,
        }
    }
}

/// Adjacent candidates often share long nested peer IDs. One SHA-256 state
/// per worker lets those candidates start at an already hashed block boundary.
#[derive(Default)]
struct Sha256Prefix {
    bytes: Vec<u8>,
    state: Sha256,
}

impl Sha256Prefix {
    fn digest(&mut self, text: &[u8]) -> [u8; 32] {
        if text.len() < 512 {
            return Sha256::digest(text).into();
        }
        if self.bytes.is_empty() || !text.starts_with(&self.bytes) {
            // Keep at most 8 KiB of complete SHA-256 blocks. The varying tail
            // is typically near the end of a sorted peer list.
            let len = (text.len() / 2).min(8192) / 64 * 64;
            self.bytes.clear();
            self.bytes.extend_from_slice(&text[..len]);
            self.state = Sha256::new_with_prefix(&self.bytes);
        }
        self.state
            .clone()
            .chain_update(&text[self.bytes.len()..])
            .finalize()
            .into()
    }
}

struct Candidate<'a> {
    name: &'a str,
    value: &'a str,
    ambiguous: bool,
    filename: String,
    path: Option<String>,
}

impl<'a> Candidate<'a> {
    fn new(name: &'a str, value: &'a str, ambiguous: bool, flat: bool) -> Self {
        let filename = peer_id(name, value, flat, LinkedPeerEncoding::Filename);
        let path = value
            .starts_with("link:")
            .then(|| peer_id(name, value, flat, LinkedPeerEncoding::Path));
        Self {
            name,
            value,
            ambiguous,
            filename,
            path,
        }
    }
}

struct Probe<'a> {
    names: &'a [&'a str],
    fixed: Option<&'a str>,
    size: usize,
    combination: usize,
    start: usize,
    count: usize,
}

// Count only as far as the remaining search budget.
fn combinations(n: usize, k: usize, limit: usize) -> usize {
    let mut count = 1u128;
    for i in 0..k.min(n - k) {
        count = count * (n - i) as u128 / (i + 1) as u128;
        if count >= limit as u128 {
            return limit;
        }
    }
    count as usize
}

fn match_candidates(
    selected: &[&Candidate<'_>],
    names: &[&str],
    size: usize,
    mut matches: impl FnMut(&str) -> bool,
    text: &mut String,
    probes: std::ops::Range<usize>,
) -> Option<(Vec<usize>, LinkedPeerEncoding)> {
    let has_links = selected.iter().any(|choice| choice.path.is_some());
    let encodings: Vec<_> = [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path]
        .into_iter()
        .take(if has_links { 2 } else { 1 })
        .map(|encoding| {
            let mut ids: Vec<_> = selected
                .iter()
                .map(|choice| {
                    let id = if encoding == LinkedPeerEncoding::Path {
                        choice.path.as_deref().unwrap_or(&choice.filename)
                    } else {
                        &choice.filename
                    };
                    (names.binary_search(&choice.name).ok(), id)
                })
                .collect();
            ids.sort_unstable_by(|a, b| a.1.cmp(b.1));
            (encoding, ids)
        })
        .collect();
    let mut indices: Vec<_> = (0..size).collect();
    for ordinal in 0..probes.end {
        if ordinal >= probes.start {
            for (encoding, ids) in &encodings {
                text.clear();
                for (_, id) in ids
                    .iter()
                    .filter(|(index, _)| index.is_none_or(|i| !indices.contains(&i)))
                {
                    if !text.is_empty() {
                        text.push_str(")(");
                    }
                    text.push_str(id);
                }
                if matches(text) {
                    return Some((indices, *encoding));
                }
            }
        }
        let Some(next) = (0..size).rfind(|i| indices[*i] < names.len() - size + i) else {
            break;
        };
        indices[next] += 1;
        for i in next + 1..size {
            indices[i] = indices[i - 1] + 1;
        }
    }
    None
}
