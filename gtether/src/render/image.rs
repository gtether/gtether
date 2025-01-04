//! Utilities for Vulkan images.
//!
//! This module contains helper utilities for working with Vulkan images. This includes things like
//! an [ImageSampler], for maintaining the descriptor sets associated with sampling a Vulkano
//! [ImageView].
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! images.

use parking_lot::Mutex;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, OnceLock};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::image::sampler::Sampler;
use vulkano::image::view::ImageView;
use vulkano::pipeline::Pipeline;

use crate::event::{Event, EventHandler};
use crate::render::pipeline::VKGraphicsPipelineSource;
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};

struct ImageSamplerState {
    view: Arc<ImageView>,
    descriptor_set: OnceLock<Arc<PersistentDescriptorSet>>,
}

impl ImageSamplerState {
    fn new(view: Arc<ImageView>) -> Self {
        Self {
            view,
            descriptor_set: OnceLock::new(),
        }
    }

    fn get_or_init_descriptor_set(
        &self,
        target: &Arc<dyn RenderTarget>,
        graphics: &Arc<dyn VKGraphicsPipelineSource>,
        sampler: &Arc<Sampler>,
        set_index: u32,
    ) -> &Arc<PersistentDescriptorSet> {
        self.descriptor_set.get_or_init(|| {
            let graphics = graphics.vk_graphics();

            PersistentDescriptorSet::new(
                target.device().descriptor_set_allocator(),
                graphics.layout().set_layouts().get(set_index as usize).unwrap().clone(),
                [
                    WriteDescriptorSet::image_view_sampler(
                        set_index,
                        self.view.clone(),
                        sampler.clone(),
                    )
                ],
                [],
            ).unwrap()
        })
    }
}

impl Debug for ImageSamplerState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSamplerState")
            .field("view", &self.view)
            .finish_non_exhaustive()
    }
}

/// Helper struct for maintaining a Vulkan image sampler.
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// [re]: crate::render::Renderer
pub struct ImageSampler {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<dyn VKGraphicsPipelineSource>,
    set_index: u32,
    sampler: Arc<Sampler>,
    inner: Mutex<ImageSamplerState>,
}

impl Debug for ImageSampler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSampler")
            .field("target", &self.target)
            .field("set_index", &self.set_index)
            .field("sampler", &self.sampler)
            .field("inner", &self.inner)
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
            set_index,
            sampler,
            inner: Mutex::new(ImageSamplerState::new(view)),
        });
        renderer.event_bus().register(RendererEventType::Stale, image_sampler.clone());
        image_sampler
    }

    /// Set this sampler's Vulkan ImageView.
    ///
    /// This will clear the associated descriptor set, causing it to be recreated when next used.
    pub fn set_image_view(&self, view: Arc<ImageView>) {
        let mut inner = self.inner.lock();
        inner.view = view;
        inner.descriptor_set.take();
    }

    /// Get the descriptor set associated with this image sampler, creating it if necessary.
    pub fn descriptor_set(&self) -> Arc<PersistentDescriptorSet> {
        self.inner.lock().get_or_init_descriptor_set(
            &self.target,
            &self.graphics,
            &self.sampler,
            self.set_index,
        ).clone()
    }
}

impl EventHandler<RendererEventType, RendererEventData> for ImageSampler {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "ImageSampler can only handle 'Stale' Renderer events");
        self.inner.lock().descriptor_set.take();
    }
}