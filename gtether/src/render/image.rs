//! Utilities for Vulkan images.
//!
//! This module contains helper utilities for working with Vulkan images. This includes things like
//! an [ImageSampler], for maintaining the descriptor sets associated with sampling a Vulkano
//! [ImageView].
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! images.

use parking_lot::RwLock;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, OnceLock};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::image::sampler::Sampler;
use vulkano::image::view::ImageView;
use vulkano::pipeline::Pipeline;

use crate::event::{Event, EventHandler};
use crate::render::pipeline::VKGraphicsPipelineSource;
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};

/// Helper struct for maintaining a Vulkan image sampler.
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// [re]: crate::render::Renderer
pub struct ImageSampler {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<dyn VKGraphicsPipelineSource>,
    view: Arc<ImageView>,
    set_index: u32,
    sampler: Arc<Sampler>,
    descriptor_set: RwLock<OnceLock<Arc<PersistentDescriptorSet>>>,
}

impl Debug for ImageSampler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSampler")
            .field("target", &self.target)
            .field("view", &self.view)
            .field("set_index", &self.set_index)
            .field("sampler", &self.sampler)
            .finish_non_exhaustive()
    }
}

impl ImageSampler {
    /// Create a new image sampler.
    pub fn new(
        renderer: &RendererHandle,
        graphics: Arc<dyn VKGraphicsPipelineSource>,
        view: Arc<ImageView>,
        set_index: u32,
        sampler: Arc<Sampler>,
    ) -> Arc<Self> {
        let image_sampler = Arc::new(Self {
            target: renderer.target.clone(),
            graphics,
            view,
            set_index,
            sampler,
            descriptor_set: RwLock::new(OnceLock::new()),
        });
        renderer.event_bus().register(RendererEventType::Stale, image_sampler.clone());
        image_sampler
    }

    /// Get the descriptor set associated with this image sampler, creating it if necessary.
    pub fn descriptor_set(&self) -> Arc<PersistentDescriptorSet> {
        self.descriptor_set.read().get_or_init(|| {
            let graphics = self.graphics.vk_graphics();

            PersistentDescriptorSet::new(
                self.target.device().descriptor_set_allocator(),
                graphics.layout().set_layouts().get(self.set_index as usize).unwrap().clone(),
                [
                    WriteDescriptorSet::image_view_sampler(
                        self.set_index,
                        self.view.clone(),
                        self.sampler.clone(),
                    )
                ],
                [],
            ).unwrap()
        }).clone()
    }
}

impl EventHandler<RendererEventType, RendererEventData> for ImageSampler {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "ImageSampler can only handle 'Stale' Renderer events");
        self.descriptor_set.write().take();
    }
}