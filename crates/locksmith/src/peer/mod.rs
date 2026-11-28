//! Reconcile existing dependency instances, preserving their package identity.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    rc::Rc,
};

use crate::{LinkedPeerEncoding, Value};

mod bindings;
mod context;
mod exact;
mod identity;
mod index;
mod providers;
pub(crate) mod range;
mod rebind;
mod three_way;

pub(crate) use bindings::optional_requirement;
use bindings::{BindingGraph, Bindings, ConstructedBindings};
pub(crate) use exact::same_binding;
pub(crate) use identity::{
    context as explicit_bindings, matches_bindings, projected_identity, root_provider,
};
pub(crate) use index::PeerIndex;
use index::WorkspaceLinks;
pub(crate) use providers::{
    bound_reference, canonical_binding, canonical_providers, importer_providers,
};
pub(crate) use rebind::same_releases;

struct Origin {
    side: usize,
    original: String,
    bindings: Rc<BTreeMap<String, String>>,
}

struct SavedRecord {
    key: String,
    body: Option<Value>,
    origin: Option<Origin>,
}

type Triple = (String, String, String);
type BindingCandidates = BTreeMap<String, BTreeSet<String>>;

pub(crate) struct Merger<'a> {
    documents: [Option<&'a Value>; 4],
    cache: HashMap<Triple, Option<String>>,
    active: HashSet<Triple>,
    rebinding: HashSet<String>,
    generated: BTreeMap<String, Value>,
    bindings: HashMap<(usize, String), Option<Bindings>>,
    misses: HashMap<(usize, String), HashSet<Vec<Option<String>>>>,
    constructed: HashMap<(usize, String), ConstructedBindings>,
    binding_graphs: [BindingGraph<'a>; 4],
    index: Rc<PeerIndex<'a>>,
    preferred_sources: Option<[usize; 4]>,
    dedupe_peers: bool,
    linked_peer_encoding: Option<LinkedPeerEncoding>,
    origins: HashMap<String, Origin>,
    provisional: Option<Vec<SavedRecord>>,
}

impl<'a> Merger<'a> {
    pub fn new([base, ours, theirs]: [Option<&'a Value>; 3]) -> Self {
        Self::with_index(PeerIndex::new([base, ours, theirs, None]))
    }

    pub(crate) fn with_index(index: Rc<PeerIndex<'a>>) -> Self {
        let documents = index.documents;
        Self {
            preferred_sources: None,
            dedupe_peers: false,
            linked_peer_encoding: None,
            documents,
            index,
            bindings: HashMap::new(),
            misses: HashMap::new(),
            constructed: HashMap::new(),
            binding_graphs: Default::default(),
            cache: HashMap::new(),
            active: HashSet::new(),
            rebinding: HashSet::new(),
            generated: BTreeMap::new(),
            origins: HashMap::new(),
            provisional: None,
        }
    }

    pub(crate) fn origin_side(&self, key: &str) -> Option<usize> {
        self.origins.get(key).map(|origin| origin.side)
    }

    pub(crate) fn has_generated(&self, key: &str) -> bool {
        self.generated.contains_key(key)
    }

    pub(crate) fn rebound_snapshot(&self, key: &str) -> Option<(usize, &Value)> {
        let body = self.generated.get(key)?;
        Some((
            self.origin_side(key).expect("rebound instance origin"),
            body,
        ))
    }

    pub(crate) fn into_generated(self) -> BTreeMap<String, Value> {
        self.generated
    }

    pub(crate) fn origin_key<'k>(&'k self, key: &'k str) -> &'k str {
        self.origins.get(key).map_or(key, |origin| &origin.original)
    }
}
