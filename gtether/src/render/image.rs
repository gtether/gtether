//! Utilities for Vulkan images.
//!
//! This module contains helper utilities for working with Vulkan images. This includes things like
//! an [EngineImageViewSampler] that implements [VKDescriptorSource], for integrating with an
//! [EngineDescriptorSet][eds].
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! images.
//!
//! [eds]: crate::render::descriptor_set::EngineDescriptorSet

use ahash::RandomState;
use parking_lot::RwLock;
use std::fmt::Debug;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use vulkano::descriptor_set::layout::DescriptorSetLayoutBinding;
use vulkano::descriptor_set::WriteDescriptorSet;
use vulkano::image::sampler::Sampler;
use vulkano::image::view::ImageView;

use crate::render::descriptor_set::VKDescriptorSource;
use crate::resource::Resource;

/// Helper struct for maintaining a Vulkan image sampler.
///
/// This struct can generate write descriptors for an [EngineDescriptorSet][eds].
///
/// [eds]: crate::render::descriptor_set::EngineDescriptorSet
#[derive(Debug)]
pub struct EngineImageViewSampler {
    inner: RwLock<(Arc<Sampler>, Arc<ImageView>)>,
    hash: AtomicU64,
    random_state: RandomState,
}

impl EngineImageViewSampler {
    fn calc_hash(random_state: &RandomState, sampler: &Arc<Sampler>, image_view: &Arc<ImageView>) -> u64 {
        let mut hash_builder = random_state.build_hasher();
        sampler.hash(&mut hash_builder);
        image_view.hash(&mut hash_builder);
        hash_builder.finish()
    }

    /// Create a new image view + sampler.
    pub fn new(
        image_view: Arc<ImageView>,
        sampler: Arc<Sampler>,
    ) -> Self {
        let random_state = RandomState::new();
        let hash = Self::calc_hash(&random_state, &sampler, &image_view);
        Self {
            inner: RwLock::new((sampler, image_view)),
            hash: AtomicU64::new(hash),
            random_state,
        }
    }

    /// Set the Vulkan [Sampler].
    #[inline]
    pub fn set_sampler(&self, sampler: Arc<Sampler>) {
        let mut inner = self.inner.write();
        inner.0 = sampler;
        self.hash.store(Self::calc_hash(&self.random_state, &inner.0, &inner.1), Ordering::Relaxed);
    }

    /// Set the Vulkan [ImageView].
    #[inline]
    pub fn set_image_view(&self, image_view: Arc<ImageView>) {
        let mut inner = self.inner.write();
        inner.1 = image_view;
        self.hash.store(Self::calc_hash(&self.random_state, &inner.0, &inner.1), Ordering::Relaxed);
    }
}

impl VKDescriptorSource for EngineImageViewSampler {
    #[inline]
    fn write_descriptor(
        &self,
        _frame_idx: usize,
        binding: u32,
        _layout: &DescriptorSetLayoutBinding,
    ) -> (WriteDescriptorSet, u64) {
        let inner = self.inner.read();
        (
            WriteDescriptorSet::image_view_sampler(
                binding,
                inner.1.clone(),
                inner.0.clone(),
            ),
            self.hash.load(Ordering::Relaxed),
        )
    }

    #[inline]
    fn update_descriptor_source(&self, _frame_idx: usize) -> u64 {
        self.hash.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct EngineSampler {
    sampler: RwLock<Arc<Sampler>>,
    hash: AtomicU64,
    random_state: RandomState,
}

impl EngineSampler {
    /// Create a new sampler.
    #[inline]
    pub fn new(sampler: Arc<Sampler>) -> Self {
        let random_state = RandomState::new();
        let hash = random_state.hash_one(sampler.clone());
        Self {
            sampler: RwLock::new(sampler),
            hash: AtomicU64::new(hash),
            random_state,
        }
    }

    /// Set the Vulkan [Sampler].
    #[inline]
    pub fn set_sampler(&self, sampler: Arc<Sampler>) {
        let hash = self.random_state.hash_one(sampler.clone());
        let mut sampler_lock = self.sampler.write();
        *sampler_lock = sampler;
        self.hash.store(hash, Ordering::Relaxed);
    }
}

impl VKDescriptorSource for EngineSampler {
    #[inline]
    fn write_descriptor(
        &self,
        _frame_idx: usize,
        binding: u32,
        _layout: &DescriptorSetLayoutBinding,
    ) -> (WriteDescriptorSet, u64) {
        let sampler = self.sampler.read();
        let hash = self.hash.load(Ordering::Relaxed);
        (WriteDescriptorSet::sampler(binding, sampler.clone()), hash)
    }

    #[inline]
    fn update_descriptor_source(&self, _frame_idx: usize) -> u64 {
        self.hash.load(Ordering::Relaxed)
    }
}

/// Helper trait for retrieving dynamic [ImageViews](ImageView).
///
/// This is mostly useful for use with [Resources](crate::resource::Resource), where a generic API
/// interface can request input via `Arc<Resource<dyn VKImageViewSource>>`.
pub trait VKImageViewSource: Debug + Send + Sync {
    fn image_view(&self) -> &Arc<ImageView>;
}

#[derive(Debug)]
pub struct EngineResourceImages<I: VKImageViewSource + 'static> {
    resources: Vec<Arc<Resource<I>>>,
    image_views: RwLock<Vec<Arc<ImageView>>>,
    hash: AtomicU64,
    random_state: RandomState,
}

impl<I: VKImageViewSource + 'static> EngineResourceImages<I> {
    fn calc_hash(&self) {
        let image_views = self.resources.iter()
            .map(|res| res.read().image_view().clone())
            .collect::<Vec<_>>();
        let mut hasher = self.random_state.build_hasher();
        image_views.len().hash(&mut hasher);
        for image_view in &image_views {
            image_view.hash(&mut hasher);
        }
        let hash = hasher.finish();
        *self.image_views.write() = image_views;
        self.hash.store(hash, Ordering::Relaxed);
    }

    #[inline]
    pub fn new(resources: impl IntoIterator<Item=Arc<Resource<I>>>) -> Arc<Self> {
        let resources: Vec<_> = resources.into_iter().collect();
        assert!(resources.len() > 0);
        let val = Arc::new(Self {
            resources,
            image_views: RwLock::new(Vec::new()),
            hash: AtomicU64::new(0),
            random_state: RandomState::new(),
        });

        for res in &val.resources {
            let cb_val = val.clone();
            res.attach_update_callback_blocking(move |_| {
                let async_val = cb_val.clone();
                async move {
                    async_val.calc_hash();
                }
            })
        }

        val.calc_hash();
        val
    }
}

impl<I: VKImageViewSource + 'static> VKDescriptorSource for EngineResourceImages<I> {
    #[inline]
    fn write_descriptor(
        &self,
        _frame_idx: usize,
        binding: u32,
        _layout: &DescriptorSetLayoutBinding,
    ) -> (WriteDescriptorSet, u64) {
        let images = self.image_views.read();
        let write_descriptor_set = WriteDescriptorSet::image_view_array(
            binding,
            0,
            images.iter().cloned(),
        );

        let hash = self.hash.load(Ordering::Relaxed);

        (write_descriptor_set, hash)
    }

    #[inline]
    fn update_descriptor_source(&self, _frame_idx: usize) -> u64 {
        self.hash.load(Ordering::Relaxed)
    }
}