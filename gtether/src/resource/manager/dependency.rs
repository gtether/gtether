use ahash::{HashMap, HashSet};
use std::iter::FusedIterator;

use crate::resource::id::ResourceId;

#[derive(Default, Debug, PartialEq, Eq)]
pub struct DependencyGraph {
    dependencies: HashMap<ResourceId, HashSet<ResourceId>>,
    dependents: HashMap<ResourceId, HashSet<ResourceId>>,
}

impl DependencyGraph {
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

    pub fn add_dependency(&mut self, id: ResourceId, dependency: ResourceId) {
        self.dependencies.entry(id.clone())
            .or_default()
            .insert(dependency.clone());
        self.dependents.entry(dependency)
            .or_default()
            .insert(id);
    }

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

    pub fn merge(&mut self, other: Self) {
        for (id, dependencies) in other.dependencies {
            self.set_dependencies(id, dependencies);
        }
    }

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
}

pub struct DependencyNode<'a> {
    graph: &'a DependencyGraph,
    id: &'a ResourceId,
}

impl<'a> DependencyNode<'a> {
    #[inline]
    pub fn id(&self) -> &'a ResourceId {
        self.id
    }

    #[inline]
    pub fn is_root(&self) -> bool {
        if let Some(dependents) = self.graph.dependents.get(self.id) {
            dependents.is_empty()
        } else {
            true
        }
    }

    #[inline]
    pub fn is_leaf(&self) -> bool {
        if let Some(dependencies) = self.graph.dependencies.get(self.id) {
            dependencies.is_empty()
        } else {
            true
        }
    }

    #[inline]
    pub fn dependencies(&self) -> DependencyNodeIter<'a> {
        if let Some(dependencies) = self.graph.dependencies.get(self.id) {
            DependencyNodeIter {
                graph: self.graph,
                inner: dependencies.iter(),
            }
        } else {
            DependencyNodeIter {
                graph: self.graph,
                inner: Default::default(),
            }
        }
    }

    #[inline]
    pub fn dependents(&self) -> DependencyNodeIter<'a> {
        if let Some(dependents) = self.graph.dependents.get(self.id) {
            DependencyNodeIter {
                graph: self.graph,
                inner: dependents.iter(),
            }
        } else {
            DependencyNodeIter {
                graph: self.graph,
                inner: Default::default(),
            }
        }
    }
}

pub struct DependencyNodeIter<'a> {
    graph: &'a DependencyGraph,
    inner: std::collections::hash_set::Iter<'a, ResourceId>,
}

impl<'a> Iterator for DependencyNodeIter<'a> {
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

impl<'a> ExactSizeIterator for DependencyNodeIter<'a> {}
impl<'a> FusedIterator for DependencyNodeIter<'a> {}

pub struct DependencyGraphLayer {
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