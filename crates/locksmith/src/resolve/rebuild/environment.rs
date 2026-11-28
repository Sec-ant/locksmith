//! Intern cache contexts while releasing temporary provider environments.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Symbol(u32);
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct Context(u32);
impl Context {
    const EMPTY: Self = Self(0);
}
type Binding = (Symbol, Symbol);
pub(super) type Environment = Arc<[Binding]>;

pub(super) struct Environments {
    strings: Vec<Arc<str>>,
    symbols: HashMap<Arc<str>, Symbol>,
    provider_names: usize,
    visited: Vec<bool>,
    reachable: Vec<Symbol>,
    bindings: Vec<Arc<[Binding]>>,
    environments: HashMap<Arc<[Binding]>, Context>,
}

impl Environments {
    pub(super) fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let empty: Arc<[Binding]> = Arc::from([]);
        let mut result = Self {
            strings: Vec::new(),
            symbols: HashMap::new(),
            provider_names: 0,
            visited: Vec::new(),
            reachable: Vec::new(),
            bindings: vec![Arc::clone(&empty)],
            environments: HashMap::from([(empty, Context::EMPTY)]),
        };
        for name in names {
            result.symbol(name);
        }
        result.provider_names = result.strings.len();
        result.visited.resize(result.provider_names, false);
        result
    }

    pub(super) fn symbol(&mut self, value: &str) -> Symbol {
        if let Some(id) = self.symbols.get(value) {
            return *id;
        }
        let id = Symbol(self.strings.len().try_into().expect("symbol index"));
        let value: Arc<str> = Arc::from(value);
        self.symbols.insert(Arc::clone(&value), id);
        self.strings.push(value);
        id
    }

    pub(super) fn text(&self, symbol: Symbol) -> &str {
        &self.strings[symbol.0 as usize]
    }

    pub(super) fn empty(&self) -> Environment {
        Arc::clone(&self.bindings[0])
    }

    pub(super) fn get(&self, environment: &Environment, name: &str) -> Option<&str> {
        let name = self.symbols.get(name)?;
        let index = environment
            .binary_search_by_key(name, |(name, _)| *name)
            .ok()?;
        Some(self.text(environment[index].1))
    }

    pub(super) fn overlay<'a>(
        &mut self,
        environment: &Environment,
        providers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Environment {
        match self.overlaid(environment, providers) {
            Cow::Borrowed(_) => Arc::clone(environment),
            Cow::Owned(bindings) => Arc::from(bindings),
        }
    }

    pub(super) fn overlay_context<'a>(
        &mut self,
        context: Context,
        providers: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Context {
        let inherited = Arc::clone(&self.bindings[context.0 as usize]);
        match self.overlaid(&inherited, providers) {
            Cow::Borrowed(_) => context,
            Cow::Owned(bindings) => self.intern(bindings),
        }
    }

    fn overlaid<'e, 'p>(
        &mut self,
        inherited: &'e [Binding],
        providers: impl IntoIterator<Item = (&'p str, &'p str)>,
    ) -> Cow<'e, [Binding]> {
        let mut bindings = Cow::Borrowed(inherited);
        for (name, reference) in providers {
            let Some(name) = self
                .symbols
                .get(name)
                .copied()
                .filter(|name| (name.0 as usize) < self.provider_names)
            else {
                continue;
            };
            let reference = self.symbol(reference);
            match bindings.binary_search_by_key(&name, |(name, _)| *name) {
                Ok(index) if bindings[index].1 == reference => {}
                Ok(index) => bindings.to_mut()[index].1 = reference,
                Err(index) => bindings.to_mut().insert(index, (name, reference)),
            }
        }
        bindings
    }

    /// Only providers present in the inherited scope contribute to the cache
    /// key. Follow dependencies of absent or release-only providers as before.
    pub(super) fn project_reachable(
        &mut self,
        scope: &Environment,
        names: &[Symbol],
        dependencies: &HashMap<Symbol, Vec<Symbol>>,
    ) -> Context {
        if scope.is_empty() || names.is_empty() {
            return Context::EMPTY;
        }
        self.reachable.extend_from_slice(names);
        for name in names {
            self.visited[name.0 as usize] = true;
        }
        let mut i = 0;
        while i < self.reachable.len() {
            let name = self.reachable[i];
            i += 1;
            if let Some(neighbors) = dependencies.get(&name) {
                if scope
                    .binary_search_by_key(&name, |(n, _)| *n)
                    .ok()
                    .is_some_and(|i| self.text(scope[i].1).contains('('))
                {
                    continue;
                }
                for &next in neighbors {
                    if !self.visited[next.0 as usize] {
                        self.visited[next.0 as usize] = true;
                        self.reachable.push(next);
                    }
                }
            }
        }
        let bindings = scope
            .iter()
            .copied()
            .filter(|(name, _)| self.visited[name.0 as usize])
            .collect();
        for name in self.reachable.drain(..) {
            self.visited[name.0 as usize] = false;
        }
        self.intern(bindings)
    }

    pub(super) fn project(&mut self, environment: &Environment, names: &[Symbol]) -> Context {
        let source = environment;
        let mut bindings = Vec::with_capacity(names.len().min(source.len()));
        let mut next = names.iter().peekable();
        for &(name, reference) in source.iter() {
            while next.peek().is_some_and(|candidate| **candidate < name) {
                next.next();
            }
            if next.peek().is_some_and(|candidate| **candidate == name) {
                bindings.push((name, reference));
                next.next();
            }
            if next.peek().is_none() {
                break;
            }
        }
        self.intern(bindings)
    }

    pub(super) fn combine(&mut self, environment: Context, providers: Context) -> Context {
        if providers == Context::EMPTY || providers == environment {
            return environment;
        }
        let mut bindings = self.bindings[environment.0 as usize].to_vec();
        for &(name, reference) in self.bindings[providers.0 as usize].iter() {
            match bindings.binary_search_by_key(&name, |(name, _)| *name) {
                Ok(index) => bindings[index].1 = reference,
                Err(index) => bindings.insert(index, (name, reference)),
            }
        }
        self.intern(bindings)
    }

    fn intern(&mut self, bindings: Vec<Binding>) -> Context {
        if let Some(id) = self.environments.get(bindings.as_slice()) {
            return *id;
        }
        let id = Context(self.bindings.len().try_into().expect("environment index"));
        let bindings: Arc<[Binding]> = Arc::from(bindings);
        self.environments.insert(Arc::clone(&bindings), id);
        self.bindings.push(bindings);
        id
    }

    pub(super) fn owned(&self, environment: &Environment) -> BTreeMap<String, String> {
        environment
            .iter()
            .map(|(name, reference)| {
                (
                    self.text(*name).to_owned(),
                    self.text(*reference).to_owned(),
                )
            })
            .collect()
    }
}
