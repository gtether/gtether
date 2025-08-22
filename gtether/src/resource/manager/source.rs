use crate::resource::id::ResourceId;
use crate::resource::source::{ResourceSource, SealedResourceDataResult, SealedResourceDataSource, SourceIndex};
use crate::resource::ResourceLoadError;

pub struct Sources {
    inner: Vec<Box<dyn ResourceSource>>,
}

impl Sources {
    #[inline]
    pub fn new(sources: impl IntoIterator<Item=Box<dyn ResourceSource>>) -> Self {
        Self {
            inner: sources.into_iter().collect(),
        }
    }

    pub fn watch_n(&self, id: &ResourceId, source_idx: &SourceIndex) {
        for (idx, source) in self.inner.iter().enumerate() {
            if idx <= source_idx.idx() {
                source.watch(
                    id.clone(),
                    source_idx.sub_idx().cloned(),
                );
            } else {
                source.unwatch(id);
            }
        }
    }

    pub fn find_hash(&self, id: &ResourceId) -> Option<SealedResourceDataSource> {
        for (idx, source) in self.inner.iter().enumerate() {
            match source.hash(id) {
                Some(hash) => return Some(hash.seal(idx)),
                None => {},
            }
        }
        None
    }

    pub async fn find_data(&self, id: &ResourceId) -> Option<SealedResourceDataResult> {
        for (idx, source) in self.inner.iter().enumerate() {
            match source.load(id).await {
                Ok(data) => return Some(Ok(data.seal(idx))),
                Err(ResourceLoadError::NotFound(_)) => {},
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}