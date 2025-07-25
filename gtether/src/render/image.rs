//! Utilities for Vulkan images.
//!
//! This module contains helper utilities for working with Vulkan images. This includes things like
//! an [ImageSampler] that implements [VKDescriptorSource], for integrating with an
//! [EngineDescriptorSet][eds].
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! images.
//!
//! [eds]: crate::render::descriptor_set::EngineDescriptorSet

use parking_lot::RwLock;
use std::fmt::Debug;
use std::sync::Arc;
use ahash::RandomState;
use vulkano::descriptor_set::WriteDescriptorSet;
use vulkano::image::sampler::Sampler;
use vulkano::image::view::ImageView;

use crate::render::descriptor_set::VKDescriptorSource;

/// Helper struct for maintaining a Vulkan image sampler.
///
/// This struct can generate write descriptors for an [EngineDescriptorSet][eds].
///
/// [eds]: crate::render::descriptor_set::EngineDescriptorSet
#[derive(Debug)]
pub struct ImageSampler {
    sampler: Arc<Sampler>,
    view: RwLock<Arc<ImageView>>,
    random_state: RandomState,
}

impl ImageSampler {
    /// Create a new image sampler.
    pub fn new(
        view: Arc<ImageView>,
        sampler: Arc<Sampler>,
    ) -> Self {
        Self {
            sampler,
            view: RwLock::new(view),
            random_state: RandomState::new(),
        }
    }

    /// Set this sampler's Vulkan ImageView.
    #[inline]
    pub fn set_image_view(&self, view: Arc<ImageView>) {
        *self.view.write() = view;
    }
}

impl VKDescriptorSource for ImageSampler {
    fn write_descriptor(&self, _frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let view = self.view.read();
        (
            WriteDescriptorSet::image_view_sampler(
                binding,
                view.clone(),
                self.sampler.clone(),
            ),
            self.random_state.hash_one(view.clone()),
        )
    }

    fn update_descriptor_source(&self, _frame_idx: usize) -> u64 {
        let view = self.view.read();
        self.random_state.hash_one(view.clone())
    }
}

/// Helper trait for retrieving dynamic [ImageViews](ImageView).
///
/// This is mostly useful for use with [Resources](crate::resource::Resource), where a generic API
/// interface can request input via `Arc<Resource<dyn VKImageViewSource>>`.
pub trait VKImageViewSource: Debug + Send + Sync {
    fn image_view(&self) -> &Arc<ImageView>;
}