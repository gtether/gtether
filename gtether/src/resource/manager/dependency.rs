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

/// A graph of dependency relations between resources.
///
/// Dependency relations can be traversed via [DependencyNodes](DependencyNode). Any resource that
/// has dependencies or dependents can be accessed via [`DependencyGraph::get()`].
///
/// DependencyGraphs are comparable via equivalence relations.
#[derive(Educe, Default, Clone, PartialEq, Eq)]
#[educe(Debug)]
pub struct DependencyGraph {
    dependencies: HashMap<ResourceId, HashSet<ResourceId>>,
    #[educe(Debug(ignore))]
    dependents: HashMap<ResourceId, HashSet<ResourceId>>,
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
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let mut graph = DependencyGraph::builder().add("a", ["b", "c"]).add("b", ["c"]).build();
    /// let expected_graph = DependencyGraph::builder().add("b", ["c"]).build();
    ///
    /// let id = ResourceId::from("a");
    /// graph.clear_dependencies(&id);
    /// assert_eq!(graph, expected_graph);
    /// assert_eq!(graph.get(&id), None);
    /// ```
    pub fn clear_dependencies(&mut self, id: &ResourceId) {
        if let Some(dependencies) = self.dependencies.remove(id) {
            for dependency in dependencies {
                let mut remove = false;
                if let Some(dependents) = self.dependents.get_mut(&dependency) {
                    dependents.remove(id);
                    if dependents.is_empty() {
                        remove = true;
                    }
                }
                if remove {
                    self.dependents.remove(&dependency);
                }
            }
        }
    }

    /// Add a dependency relation for a particular resource in this graph.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let mut graph = DependencyGraph::builder().add("a", ["b"]).build();
    /// let expected_graph = DependencyGraph::builder().add("a", ["b", "c"]).build();
    ///
    /// graph.add_dependency(ResourceId::from("a"), ResourceId::from("c"));
    /// assert_eq!(graph, expected_graph);
    /// ```
    pub fn add_dependency(&mut self, id: ResourceId, dependency: ResourceId) {
        self.dependencies.entry(id.clone())
            .or_default()
            .insert(dependency.clone());
        self.dependents.entry(dependency)
            .or_default()
            .insert(id);
    }

    /// Set the dependency relations for a particular resource in this graph.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let mut graph = DependencyGraph::builder().add("a", ["b", "c"]).build();
    /// let expected_graph = DependencyGraph::builder().add("a", ["d", "e"]).build();
    ///
    /// graph.set_dependencies(ResourceId::from("a"), [ResourceId::from("d"), ResourceId::from("e")]);
    /// assert_eq!(graph, expected_graph);
    /// ```
    pub fn set_dependencies(&mut self, id: ResourceId, dependencies: impl IntoIterator<Item=ResourceId>) {
        self.clear_dependencies(&id);
        let dependencies = dependencies.into_iter().collect::<HashSet<ResourceId>>();
        if !dependencies.is_empty() {
            for dependency in &dependencies {
                self.dependents.entry(dependency.clone())
                    .or_default()
                    .insert(id.clone());
            }
            self.dependencies.insert(id, dependencies);
        }
    }

    /// Get a [DependencyNode] for a particular resource in this graph.
    ///
    /// If the given resource does not have any dependencies or dependents, it will not be in the
    /// graph, and so this method will return `None`.
    ///
    /// ```
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let graph = DependencyGraph::builder().add("a", ["b"]).build();
    ///
    /// let id_a = ResourceId::from("a");
    /// let id_b = ResourceId::from("b");
    /// let id_c = ResourceId::from("c");
    ///
    /// assert_eq!(graph.get(&id_a).unwrap().id(), &id_a);
    /// assert_eq!(graph.get(&id_b).unwrap().id(), &id_b);
    /// assert_eq!(graph.get(&id_c), None);
    /// ```
    pub fn get(&self, id: &ResourceId) -> Option<DependencyNode<'_>> {
        if let Some((id, _)) = self.dependencies.get_key_value(id) {
            Some(DependencyNode {
                graph: self,
                id,
            })
        } else if let Some((id, _)) = self.dependents.get_key_value(id) {
            Some(DependencyNode {
                graph: self,
                id,
            })
        } else {
            None
        }
    }

    /// Iterate over all dependencies in this graph, in no particular order.
    ///
    /// This will only iterate over [nodes](DependencyNode) that have dependencies, and will not
    /// iterate over [leaf nodes](DependencyNode::is_leaf).
    ///
    /// ```
    /// use std::collections::HashSet;
    /// use gtether::resource::id::ResourceId;
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let graph = DependencyGraph::builder().add("a", ["b", "c"]).add("b", ["c"]).build();
    /// let expected_ids = ["a", "b"].into_iter()
    ///     .map(ResourceId::from)
    ///     .collect::<HashSet<_>>();
    ///
    /// let ids = graph.iter()
    ///     .map(|node| node.id().clone())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(ids, expected_ids);
    /// ```
    #[inline]
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            graph: self,
            inner: self.dependencies.keys(),
        }
    }
}

/// Iterator for all [DependencyNodes](DependencyNode) in a [DependencyGraph].
///
/// See [`DependencyGraph::iter()`].
#[derive(Debug, Clone)]
pub struct Iter<'a> {
    graph: &'a DependencyGraph,
    inner: std::collections::hash_map::Keys<'a, ResourceId, HashSet<ResourceId>>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = DependencyNode<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|id| DependencyNode {
            graph: self.graph,
            id,
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
    id: &'a ResourceId,
}

impl<'a> PartialEq for DependencyNode<'a> {
    fn eq(&self, other: &Self) -> bool {
        if !std::ptr::eq(self.graph, other.graph) {
            return false
        }
        self.id == other.id
    }
}

impl<'a> Eq for DependencyNode<'a> {}

impl<'a> DependencyNode<'a> {
    /// The [ResourceId] for this particular node.
    #[inline]
    pub fn id(&self) -> &'a ResourceId {
        self.id
    }

    /// Whether this node is a root node (has no dependents).
    #[inline]
    pub fn is_root(&self) -> bool {
        if let Some(dependents) = self.graph.dependents.get(self.id) {
            dependents.is_empty()
        } else {
            true
        }
    }

    /// Whether this node is a leaf node (has no dependencies).
    #[inline]
    pub fn is_leaf(&self) -> bool {
        if let Some(dependencies) = self.graph.dependencies.get(self.id) {
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
    ///
    /// let graph = DependencyGraph::builder().add("a", ["b", "c"]).add("b", ["d"]).build();
    ///
    /// // Flat
    /// let expected_ids = ["b", "c"].into_iter()
    ///     .map(ResourceId::from)
    ///     .collect::<HashSet<_>>();
    /// let ids = graph.get(&ResourceId::from("a")).unwrap()
    ///     .dependencies()
    ///     .map(|node| node.id().clone())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(ids, expected_ids);
    ///
    /// // Recursive
    /// let expected_ids = ["b", "c", "d"].into_iter()
    ///     .map(ResourceId::from)
    ///     .collect::<HashSet<_>>();
    /// let ids = graph.get(&ResourceId::from("a")).unwrap()
    ///     .dependencies()
    ///     .recursive()
    ///     .map(|node| node.id().clone())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(ids, expected_ids);
    /// ```
    #[inline]
    pub fn dependencies(&self) -> NodeIter<'a> {
        if let Some(dependencies) = self.graph.dependencies.get(self.id) {
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
    ///
    /// let graph = DependencyGraph::builder()
    ///     .add("a", ["b"])
    ///     .add("b", ["d"])
    ///     .add("c", ["d"])
    ///     .build();
    ///
    /// // Flat
    /// let expected_ids = ["b", "c"].into_iter()
    ///     .map(ResourceId::from)
    ///     .collect::<HashSet<_>>();
    /// let ids = graph.get(&ResourceId::from("d")).unwrap()
    ///     .dependents()
    ///     .map(|node| node.id().clone())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(ids, expected_ids);
    ///
    /// // Recursive
    /// let expected_ids = ["a", "b", "c"].into_iter()
    ///     .map(ResourceId::from)
    ///     .collect::<HashSet<_>>();
    /// let ids = graph.get(&ResourceId::from("d")).unwrap()
    ///     .dependents()
    ///     .recursive()
    ///     .map(|node| node.id().clone())
    ///     .collect::<HashSet<_>>();
    /// assert_eq!(ids, expected_ids);
    /// ```
    #[inline]
    pub fn dependents(&self) -> NodeIter<'a> {
        if let Some(dependents) = self.graph.dependents.get(self.id) {
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
    inner: std::collections::hash_set::Iter<'a, ResourceId>,
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
        self.inner.next().map(|id| DependencyNode {
            graph: self.graph,
            id,
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
/// use gtether::resource::manager::dependency::DependencyGraph;
///
/// let graph = DependencyGraph::builder()
///     .add("a", ["b", "c"])
///     .add("b", ["d"])
///     .add("c", ["d", "e"])
///     .build();
/// ```
#[derive(Default)]
pub struct DependencyGraphBuilder {
    graph: DependencyGraph,
}

impl DependencyGraphBuilder {
    /// Add a dependency relation.
    ///
    /// Overrides any existing relation for the given `id`.
    ///
    /// ```
    /// use gtether::resource::manager::dependency::DependencyGraph;
    ///
    /// let graph = DependencyGraph::builder()
    ///     // "a" depends on both "b" and "c"
    ///     .add("a", ["b", "c"])
    ///     .build();
    /// ```
    #[inline]
    pub fn add(
        &mut self,
        id: impl Into<ResourceId>,
        dependencies: impl IntoIterator<Item=impl Into<ResourceId>>,
    ) -> &mut Self {
        self.graph.set_dependencies(
            id.into(),
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
    parents: Vec<ResourceId>,
    id: ResourceId,
    dependencies: HashSet<ResourceId>,
}

impl DependencyGraphLayer {
    #[inline]
    pub fn new(id: ResourceId, parents: impl IntoIterator<Item=ResourceId>) -> Self {
        Self {
            parents: parents.into_iter().collect(),
            id,
            dependencies: Default::default(),
        }
    }

    #[inline]
    pub fn parents(&self) -> std::slice::Iter<'_, ResourceId> {
        self.parents.iter()
    }

    #[inline]
    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    #[inline]
    pub fn insert(&mut self, dependency: ResourceId) {
        self.dependencies.insert(dependency);
    }

    #[inline]
    pub fn apply(self, graph: &mut DependencyGraph) {
        graph.set_dependencies(self.id, self.dependencies);
    }
}