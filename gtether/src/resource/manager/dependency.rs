/// Dependency graph logic.
///
/// This module contains the logic used to track [DependencyGraphs](DependencyGraph) in the
/// [ResourceManager](super::ResourceManager). While this logic can be used to inspect active
/// dependency relations in the ResourceManager, it is primarily intended for use in e.g. tests,
/// where the DependencyGraph can be compared against expected graphs.

use ahash::{HashMap, HashSet};
use educe::Educe;
use std::iter::FusedIterator;

use crate::resource::id::ResourceId;
use crate::resource::{BoxedResKey, ResKey};

/// A graph of dependency relations between resources.
///
/// Dependency relations can be traversed via [DependencyNodes](DependencyNode). Any resource that
/// has dependencies or dependents can be accessed via [`DependencyGraph::get()`].
///
/// DependencyGraphs are comparable via equivalence relations.
#[derive(Educe, Default, Clone, PartialEq, Eq)]
#[educe(Debug)]
pub struct DependencyGraph {
    dependencies: HashMap<Box<dyn ResKey>, HashSet<Box<dyn ResKey>>>,
    //#[educe(Debug(ignore))]
    dependents: HashMap<Box<dyn ResKey>, HashSet<Box<dyn ResKey>>>,
    //#[educe(Debug(ignore))]
    key_map: HashMap<ResourceId, HashSet<Box<dyn ResKey>>>,
}

impl DependencyGraph {
    /// Create a [DependencyGraphBuilder].
    ///
    /// This can be useful to construct a populated DependencyGraph in e.g. unit tests.
    #[inline]
    pub fn builder() -> DependencyGraphBuilder {
        DependencyGraphBuilder::default()
    }

    /// Clear all dependency relations for a particular resource in this graph.
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    ///
    /// let mut graph = DependencyGraph::builder()
    ///     .add(&a, [&b, &c])
    ///     .add(&b, [&c])
    ///     .build();
    /// let expected_graph = DependencyGraph::builder()
    ///     .add(&b, [&c])
    ///     .build();
    ///
    /// graph.clear_dependencies(&a);
    /// assert_eq!(graph, expected_graph);
    /// assert_eq!(graph.get(&a), None);
    /// ```
    pub fn clear_dependencies(&mut self, key: impl AsRef<dyn ResKey>) {
        let key = key.as_ref();
        if let Some(dependencies) = self.dependencies.remove(key) {
            for dependency in dependencies {
                let mut remove = false;
                if let Some(dependents) = self.dependents.get_mut(&dependency) {
                    dependents.remove(key);
                    if dependents.is_empty() {
                        remove = true;
                    }
                }
                if remove {
                    self.dependents.remove(&dependency);
                    if !self.dependencies.contains_key(&dependency)
                        && let Some(keys) = self.key_map.get_mut(&dependency.id())
                    {
                        keys.remove(&dependency);
                        if keys.is_empty() {
                            self.key_map.remove(&dependency.id());
                        }
                    }
                }
            }
        }
        if let Some(keys) = self.key_map.get_mut(&key.id()) {
            keys.remove(key);
            if keys.is_empty() {
                self.key_map.remove(&key.id());
            }
        }
    }

    /// Add a dependency relation for a particular resource in this graph.
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    ///
    /// let mut graph = DependencyGraph::builder().add(&a, [&b]).build();
    /// let expected_graph = DependencyGraph::builder().add(&a, [&b, &c]).build();
    ///
    /// graph.add_dependency(&a, &c);
    /// assert_eq!(graph, expected_graph);
    /// ```
    pub fn add_dependency(&mut self, key: impl AsRef<dyn ResKey>, dependency: impl AsRef<dyn ResKey>) {
        let key = key.as_ref();
        let dependency = dependency.as_ref();
        self.dependencies.entry(key.to_box())
            .or_default()
            .insert(dependency.to_box());
        self.dependents.entry(dependency.to_box())
            .or_default()
            .insert(key.to_box());
        self.key_map.entry(key.id())
            .or_default()
            .insert(key.to_box());
        self.key_map.entry(dependency.id())
            .or_default()
            .insert(dependency.to_box());
    }

    /// Set the dependency relations for a particular resource in this graph.
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    /// let d = res_key("d", &IdentityLoader);
    /// let e = res_key("e", &IdentityLoader);
    ///
    /// let mut graph = DependencyGraph::builder().add(&a, [&b, &c]).build();
    /// let expected_graph = DependencyGraph::builder().add(&a, [&d, &e]).build();
    ///
    /// graph.set_dependencies(&a, [&d, &e]);
    /// assert_eq!(graph, expected_graph);
    /// ```
    pub fn set_dependencies(
        &mut self,
        key: impl AsRef<dyn ResKey>,
        dependencies: impl IntoIterator<Item=impl AsRef<dyn ResKey>>,
    ) {
        let key = key.as_ref();
        self.clear_dependencies(key);
        let dependencies = dependencies.into_iter()
            .map(|k| k.as_ref().to_box())
            .collect::<HashSet<_>>();
        if !dependencies.is_empty() {
            for dependency in &dependencies {
                self.dependents.entry(dependency.to_box())
                    .or_default()
                    .insert(key.to_box());
                self.key_map.entry(dependency.id())
                    .or_default()
                    .insert(dependency.to_box());
            }
            self.dependencies.insert(key.to_box(), dependencies);
            self.key_map.entry(key.id())
                .or_default()
                .insert(key.to_box());
        }
    }

    /// Get a [DependencyNode] for a particular resource in this graph.
    ///
    /// If the given resource does not have any dependencies or dependents, it will not be in the
    /// graph, and so this method will return `None`.
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder().add(&a, [&b]).build();
    ///
    /// assert_eq!(graph.get(&a).unwrap().key(), &*a);
    /// assert_eq!(graph.get(&b).unwrap().key(), &*b);
    /// assert_eq!(graph.get(&c), None);
    /// ```
    pub fn get(&self, key: impl AsRef<dyn ResKey>) -> Option<DependencyNode<'_>> {
        let key = key.as_ref();
        if let Some((key, _)) = self.dependencies.get_key_value(key) {
            Some(DependencyNode {
                graph: self,
                key: &**key,
            })
        } else if let Some((key, _)) = self.dependents.get_key_value(key) {
            Some(DependencyNode {
                graph: self,
                key: &**key,
            })
        } else {
            None
        }
    }

    /// Get all known resource [keys](ResKey) for a particular [ID](ResourceId).
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    /// use std::collections::HashSet;
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    ///
    /// let mut graph = DependencyGraph::builder().add(&a, [&b]).build();
    ///
    /// assert_eq!(HashSet::<_>::from_iter(graph.keys_for_id("a")), HashSet::from([&a]));
    /// graph.clear_dependencies(&a);
    /// assert_eq!(HashSet::<_>::from_iter(graph.keys_for_id("a")), HashSet::from([]));
    /// ```
    #[inline]
    pub fn keys_for_id(
        &self,
        id: impl Into<ResourceId>,
    ) -> std::collections::hash_set::Iter<'_, Box<dyn ResKey>> {
        if let Some(keys) = self.key_map.get(&id.into()) {
            keys.iter()
        } else {
            Default::default()
        }
    }

    /// Iterate over all nodes in the graph that have the given [ID](ResourceId).
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    /// use std::collections::HashSet;
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder().add(&a, [&b]).build();
    /// let expected_keys = HashSet::from([a.clone()]);
    ///
    /// let keys = graph.iter()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    /// ```
    #[inline]
    pub fn nodes_for_id(&self, id: impl Into<ResourceId>) -> NodesForId<'_> {
        NodesForId {
            graph: self,
            keys: self.keys_for_id(id),
        }
    }

    /// Iterate over all dependencies in this graph, in no particular order.
    ///
    /// This will only iterate over [nodes](DependencyNode) that have dependencies, and will not
    /// iterate over [leaf nodes](DependencyNode::is_leaf).
    ///
    /// ```
    /// use std::collections::HashSet;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder().add(&a, [&b, &c]).add(&b, [&c]).build();
    /// let expected_keys = HashSet::from([a.clone(), b.clone()]);
    ///
    /// let keys = graph.iter()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    /// ```
    #[inline]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            graph: self,
            inner: self.dependencies.keys(),
        }
    }
}

/// Iterator for all [nodes](DependencyNode) for a given [ID](ResourceId).
///
/// See [`DependencyGraph::nodes_for_id()`].
pub struct NodesForId<'a> {
    graph: &'a DependencyGraph,
    keys: std::collections::hash_set::Iter<'a, Box<dyn ResKey>>,
}

impl<'a> Iterator for NodesForId<'a> {
    type Item = DependencyNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key = self.keys.next()?;
            if let Some(node) = self.graph.get(key) {
                return Some(node)
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.keys.len()))
    }
}

impl<'a> FusedIterator for NodesForId<'a> {}

/// Iterator for all [DependencyNodes](DependencyNode) in a [DependencyGraph].
///
/// See [`DependencyGraph::iter()`].
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    graph: &'a DependencyGraph,
    inner: std::collections::hash_map::Keys<'a, Box<dyn ResKey>, HashSet<Box<dyn ResKey>>>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = DependencyNode<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|key| DependencyNode {
            graph: self.graph,
            key: &**key,
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a> ExactSizeIterator for Iter<'a> {}
impl<'a> FusedIterator for Iter<'a> {}

/// Individual node in a [DependencyGraph].
///
/// This node consists of entirely references, so it is cheap to clone/copy. It is used to get a
/// view into the DependencyGraph at a particular point.
///
/// Traversal can be achieved via [`dependencies()`](DependencyNode::dependencies) or
/// [`dependents()`](DependencyNode::dependents).
#[derive(Educe, Clone, Copy)]
#[educe(Debug)]
pub struct DependencyNode<'a> {
    #[educe(Debug(ignore))]
    graph: &'a DependencyGraph,
    key: &'a dyn ResKey,
}

impl<'a> PartialEq for DependencyNode<'a> {
    fn eq(&self, other: &Self) -> bool {
        if !std::ptr::eq(self.graph, other.graph) {
            return false
        }
        self.key == other.key
    }
}

impl<'a> Eq for DependencyNode<'a> {}

impl<'a> DependencyNode<'a> {
    /// The [key](DepKey) for this particular node.
    #[inline]
    pub fn key(&self) -> &'a dyn ResKey {
        self.key
    }

    /// Whether this node is a root node (has no dependents).
    #[inline]
    pub fn is_root(&self) -> bool {
        if let Some(dependents) = self.graph.dependents.get(self.key) {
            dependents.is_empty()
        } else {
            true
        }
    }

    /// Whether this node is a leaf node (has no dependencies).
    #[inline]
    pub fn is_leaf(&self) -> bool {
        if let Some(dependencies) = self.graph.dependencies.get(self.key) {
            dependencies.is_empty()
        } else {
            true
        }
    }

    /// Iterate over this node's dependencies.
    ///
    /// By default, this only iterates over direct dependencies, but recursive iteration can be
    /// achieved via calling [`recursive()`](NodeIter::recursive) on the iterator.
    ///
    /// ```
    /// use std::collections::HashSet;
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    /// let d = res_key("d", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder()
    ///     .add(&a, [&b, &c])
    ///     .add(&b, [&d])
    ///     .build();
    ///
    /// // Flat
    /// let expected_keys = HashSet::from([b.clone(), c.clone()]);
    /// let keys = graph.get(&a).unwrap()
    ///     .dependencies()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    ///
    /// // Recursive
    /// let expected_keys = HashSet::from([b.clone(), c.clone(), d.clone()]);
    /// let keys = graph.get(&a).unwrap()
    ///     .dependencies()
    ///     .recursive()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    /// ```
    #[inline]
    pub fn dependencies(&self) -> NodeIter<'a> {
        if let Some(dependencies) = self.graph.dependencies.get(self.key) {
            NodeIter {
                graph: self.graph,
                inner: dependencies.iter(),
                direction: NodeIterDirection::Dependencies,
            }
        } else {
            NodeIter {
                graph: self.graph,
                inner: Default::default(),
                direction: NodeIterDirection::Dependencies,
            }
        }
    }

    /// Iterate over this node's dependents.
    ///
    /// By default, this only iterates over direct dependents, but recursive iteration can be
    /// achieved via calling [`recursive()`](NodeIter::recursive) on the iterator.
    ///
    /// ```
    /// use std::collections::HashSet;
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    /// let d = res_key("d", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder()
    ///     .add(&a, [&b])
    ///     .add(&b, [&d])
    ///     .add(&c, [&d])
    ///     .build();
    ///
    /// // Flat
    /// let expected_keys = HashSet::from([b.clone(), c.clone()]);
    /// let keys = graph.get(&d).unwrap()
    ///     .dependents()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    ///
    /// // Recursive
    /// let expected_keys = HashSet::from([a.clone(), b.clone(), c.clone()]);
    /// let keys = graph.get(&d).unwrap()
    ///     .dependents()
    ///     .recursive()
    ///     .map(|node| node.key().to_box())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(keys, expected_keys);
    /// ```
    #[inline]
    pub fn dependents(&self) -> NodeIter<'a> {
        if let Some(dependents) = self.graph.dependents.get(self.key) {
            NodeIter {
                graph: self.graph,
                inner: dependents.iter(),
                direction: NodeIterDirection::Dependents,
            }
        } else {
            NodeIter {
                graph: self.graph,
                inner: Default::default(),
                direction: NodeIterDirection::Dependents,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeIterDirection {
    Dependencies,
    Dependents,
}

/// Iterator for [node](DependencyNode) dependencies or dependents.
///
/// See [`DependencyNode::dependencies()`]/[`DependencyNode::dependents()`].
#[derive(Debug, Clone)]
pub struct NodeIter<'a> {
    graph: &'a DependencyGraph,
    inner: std::collections::hash_set::Iter<'a, Box<dyn ResKey>>,
    direction: NodeIterDirection,
}

impl<'a> NodeIter<'a> {
    /// Turn this iterator into a recursive iterator.
    ///
    /// Instead of yielding only direct dependencies or dependents, it will instead yield
    /// recursively until leaf nodes (in the case of dependencies) or root nodes (in the case of
    /// dependents) are reached.
    ///
    /// See also [`DependencyNode::dependencies()`]/[`DependencyNode::dependents()`].
    pub fn recursive(self) -> NodeRecursiveIter<'a> {
        NodeRecursiveIter {
            nodes: self,
            next_layer: None,
        }
    }
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = DependencyNode<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|key| DependencyNode {
            graph: self.graph,
            key: &**key,
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<'a> ExactSizeIterator for NodeIter<'a> {}
impl<'a> FusedIterator for NodeIter<'a> {}

/// Recursive iterator for [node](DependencyNode) dependencies or dependents.
///
/// See [`DependencyNode::dependencies()`]/[`DependencyNode::dependents()`].
pub struct NodeRecursiveIter<'a> {
    nodes: NodeIter<'a>,
    next_layer: Option<Box<Self>>,
}

impl<'a> Iterator for NodeRecursiveIter<'a> {
    type Item = DependencyNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(next_layer) = &mut self.next_layer {
            if let Some(node) = next_layer.next() {
                return Some(node)
            } else {
                self.next_layer = None;
            }
        }

        if let Some(node) = self.nodes.next() {
            self.next_layer = Some(Box::new(match self.nodes.direction {
                NodeIterDirection::Dependencies => node.dependencies().recursive(),
                NodeIterDirection::Dependents => node.dependents().recursive(),
            }));
            Some(node)
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.nodes.len(), None)
    }
}

impl<'a> FusedIterator for NodeRecursiveIter<'a> {}

/// Builder pattern for [DependencyGraphs](DependencyGraph).
///
/// This is useful for creating pre-populated DependencyGraphs. While a DependencyGraphBuilder can
/// be created via `default()`, it is recommended to instead use [`DependencyGraph::builder()`].
///
/// # Examples
/// ```
/// use gtether::resource::id::ResourceId;
/// use gtether::resource::manager::dependency::DependencyGraph;
/// use gtether::resource::{IdentityLoader, res_key};
///
/// let a = res_key("a", &IdentityLoader);
/// let b = res_key("b", &IdentityLoader);
/// let c = res_key("c", &IdentityLoader);
/// let d = res_key("d", &IdentityLoader);
/// let e = res_key("e", &IdentityLoader);
///
/// let graph = DependencyGraph::builder()
///     .add(&a, [&b, &c])
///     .add(&b, [&d])
///     .add(&c, [&d, &e])
///     .build();
/// ```
#[derive(Default)]
pub struct DependencyGraphBuilder {
    graph: DependencyGraph,
}

impl DependencyGraphBuilder {
    /// Add a dependency relation.
    ///
    /// Overrides any existing relation for the given `key`.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    /// use gtether::resource::{IdentityLoader, res_key};
    ///
    /// let a = res_key("a", &IdentityLoader);
    /// let b = res_key("b", &IdentityLoader);
    /// let c = res_key("c", &IdentityLoader);
    ///
    /// let graph = DependencyGraph::builder()
    ///     // "a" depends on both "b" and "c"
    ///     .add(a, [b, c])
    ///     .build();
    /// ```
    #[inline]
    pub fn add(
        &mut self,
        key: impl AsRef<dyn ResKey>,
        dependencies: impl IntoIterator<Item=impl Into<BoxedResKey>>,
    ) -> &mut Self {
        self.graph.set_dependencies(
            key,
            dependencies.into_iter().map(Into::into),
        );
        self
    }

    /// Build a [DependencyGraph].
    #[inline]
    pub fn build(&self) -> DependencyGraph {
        self.graph.clone()
    }
}

pub(in crate::resource) struct DependencyGraphLayer {
    parents: Vec<Box<dyn ResKey>>,
    key: Box<dyn ResKey>,
    dependencies: HashSet<Box<dyn ResKey>>,
}

impl DependencyGraphLayer {
    #[inline]
    pub fn new(
        key: Box<dyn ResKey>,
        parents: impl IntoIterator<Item=impl Into<Box<dyn ResKey>>>,
    ) -> Self {
        Self {
            parents: parents.into_iter().map(Into::into).collect(),
            key,
            dependencies: Default::default(),
        }
    }

    #[inline]
    pub fn parents(&self) -> std::slice::Iter<'_, Box<dyn ResKey>> {
        self.parents.iter()
    }

    #[inline]
    pub fn key(&self) -> &dyn ResKey {
        &*self.key
    }

    #[inline]
    pub fn insert(&mut self, dependency: Box<dyn ResKey>) {
        self.dependencies.insert(dependency);
    }

    #[inline]
    pub fn apply(self, graph: &mut DependencyGraph) {
        graph.set_dependencies(&self.key, self.dependencies);
    }
}