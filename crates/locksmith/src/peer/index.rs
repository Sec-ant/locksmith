//! Shared release and workspace-link indexes for the input lockfiles.
use std::{cell::OnceCell, collections::HashMap, rc::Rc};

use crate::{LinkedPeerEncoding, Value, model::graph, model::pin};

use super::{Merger, identity::link_peer_id};

pub(super) type WorkspaceLinks = HashMap<String, HashMap<String, Option<String>>>;

type ReleaseCandidates<'a> = HashMap<&'a str, Vec<(&'a str, &'a Value)>>;

pub(crate) struct PeerIndex<'a> {
    pub(super) documents: [Option<&'a Value>; 4],
    candidates: OnceCell<ReleaseCandidates<'a>>,
    links: [OnceCell<WorkspaceLinks>; 4],
}

impl<'a> PeerIndex<'a> {
    pub(crate) fn new(documents: [Option<&'a Value>; 4]) -> Rc<Self> {
        Rc::new(Self {
            documents,
            candidates: OnceCell::new(),
            links: Default::default(),
        })
    }

    pub(super) fn candidates(&self) -> &ReleaseCandidates<'a> {
        self.candidates.get_or_init(|| {
            let mut candidates = ReleaseCandidates::new();
            for document in self.documents.into_iter().flatten() {
                for (key, body) in graph::section(document, "snapshots").into_iter().flatten() {
                    candidates
                        .entry(pin::package_key(key))
                        .or_default()
                        .push((key, body));
                }
            }
            candidates
        })
    }

    pub(super) fn links(&self, side: usize) -> &WorkspaceLinks {
        self.links[side].get_or_init(|| {
            let mut links = WorkspaceLinks::new();
            for (_, body) in self.documents[side]
                .and_then(|doc| graph::section(doc, "snapshots"))
                .into_iter()
                .flatten()
            {
                for group in ["dependencies", "optionalDependencies"] {
                    for (name, value) in graph::section(body, group).into_iter().flatten() {
                        let Some(reference) = value.as_str() else {
                            continue;
                        };
                        let Some(path) = reference.strip_prefix("link:") else {
                            continue;
                        };
                        for encoding in [LinkedPeerEncoding::Filename, LinkedPeerEncoding::Path] {
                            links
                                .entry(name.clone())
                                .or_default()
                                .entry(link_peer_id(path, encoding))
                                .and_modify(|old| {
                                    if old.as_deref() != Some(reference) {
                                        *old = None;
                                    }
                                })
                                .or_insert_with(|| Some(reference.into()));
                        }
                    }
                }
            }
            links
        })
    }
}

impl<'a> Merger<'a> {
    pub(crate) fn has_release(&self, key: &str) -> bool {
        self.index.candidates().contains_key(key)
    }

    pub(crate) fn locked_instances(
        &self,
        side: usize,
        release: &str,
    ) -> impl Iterator<Item = &'a str> + '_ {
        let snapshots = self.documents[side].and_then(|d| graph::section(d, "snapshots"));
        self.index
            .candidates()
            .get(release)
            .into_iter()
            .flatten()
            .filter_map(move |&(key, record)| {
                snapshots?
                    .get(key)
                    .filter(|v| std::ptr::eq(*v, record))
                    .map(|_| key)
            })
    }

    pub(crate) fn snapshot_key(&self, name: &str, reference: &str) -> String {
        let exists = |key| {
            self.has_generated(key)
                || self
                    .documents
                    .iter()
                    .flatten()
                    .any(|doc| doc["snapshots"].get(key).is_some())
        };
        let named = format!("{name}@{reference}");
        if !exists(&named) && exists(reference) {
            reference.into()
        } else {
            named
        }
    }
}
