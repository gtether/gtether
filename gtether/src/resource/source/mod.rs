use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use std::io::Read;

use crate::resource::manager::ResourceWatcher;
use crate::resource::path::ResourcePath;
use crate::resource::ResourceLoadError;

pub mod list;

#[derive(Debug, Clone, Eq)]
pub struct SourceIndex {
    idx: usize,
    sub_idx: Option<Box<SourceIndex>>,
}

impl SourceIndex {
    #[inline]
    pub fn new(idx: usize) -> Self {
        Self {
            idx,
            sub_idx: None,
        }
    }

    #[inline]
    pub fn with_sub_idx(mut self, sub_idx: Option<SourceIndex>) -> Self {
        self.sub_idx = sub_idx.map(|inner| Box::new(inner));
        self
    }

    #[inline]
    pub fn idx(&self) -> usize { self.idx }

    #[inline]
    pub fn sub_idx(&self) -> Option<&SourceIndex> {
        self.sub_idx.as_ref().map(|inner| Box::as_ref(inner))
    }
}

impl Display for SourceIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(sub_idx) = &self.sub_idx {
            write!(f, "{}:", self.idx)?;
            Display::fmt(&sub_idx, f)
        } else {
            write!(f, "{}", self.idx)
        }
    }
}

impl From<usize> for SourceIndex {
    fn from(value: usize) -> Self {
        Self::new(value)
    }
}

impl PartialEq for SourceIndex {
    fn eq(&self, other: &Self) -> bool {
        if self.idx == other.idx {
            match (&self.sub_idx, &other.sub_idx) {
                (Some(sub_a), Some(sub_b)) => {
                    sub_a == sub_b
                },
                (None, None) => true,
                _ => false,
            }
        } else {
            false
        }
    }
}

impl PartialOrd for SourceIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match self.idx.cmp(&other.idx) {
            Ordering::Equal => {
                match (&self.sub_idx, &other.sub_idx) {
                    (Some(sub_a), Some(sub_b)) => {
                        sub_a.partial_cmp(sub_b)
                    },
                    (None, None) => Some(Ordering::Equal),
                    _ => None,
                }
            },
            order => Some(order),
        }
    }
}

pub struct ResourceData {
    pub data: Box<dyn Read>,
    pub source_idx: SourceIndex,
}

pub enum ResourceSubData {
    SubIndex(ResourceData),
    NoIndex(Box<dyn Read>),
}

impl ResourceSubData {
    pub fn seal(self, idx: usize) -> ResourceData {
        match self {
            ResourceSubData::SubIndex(r_data) => ResourceData {
                data: r_data.data,
                source_idx: SourceIndex::new(idx).with_sub_idx(Some(r_data.source_idx)),
            },
            ResourceSubData::NoIndex(r_data) => ResourceData {
                data: r_data,
                source_idx: SourceIndex::new(idx),
            },
        }
    }

    #[inline]
    pub fn wrap(self, idx: usize) -> Self {
        ResourceSubData::SubIndex(self.seal(idx))
    }
}

impl<R: Read + 'static> From<Box<R>> for ResourceSubData {
    #[inline]
    fn from(value: Box<R>) -> Self {
        Self::NoIndex(value)
    }
}

pub type ResourceSubDataResult = Result<ResourceSubData, ResourceLoadError>;
pub type ResourceDataResult = Result<ResourceData, ResourceLoadError>;

pub trait ResourceSource: Send + Sync + 'static {
    fn load(&self, id: &ResourcePath) -> ResourceSubDataResult;
    fn sub_load(&self, id: &ResourcePath, _sub_idx: &SourceIndex) -> ResourceSubDataResult {
        self.load(id)
    }
    fn watch(&self, id: ResourcePath, watcher: ResourceWatcher, sub_idx: Option<SourceIndex>);
    fn unwatch(&self, id: &ResourcePath);
}

impl<S: ResourceSource> From<S> for Box<dyn ResourceSource> {
    #[inline]
    fn from(value: S) -> Self {
        Box::new(value)
    }
}