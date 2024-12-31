//! Utilities for Vulkan attachments.
//!
//! This module contains helper utilities for working with Vulkan attachments. This includes things
//! like a [trait][am] for mapping between friendly names and their attachments in a render pass, as
//! well as an [AttachmentSet] for handling descriptor sets for attachments between subpasses.
//!
//! [am]: AttachmentMap
//!
//! Many of these utilities are entirely optional, and you are free to roll your own logic for
//! Vulkan attachments where applicable.

use parking_lot::Mutex;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, OnceLock};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::image::view::ImageView;
use vulkano::pipeline::Pipeline;

use crate::event::{Event, EventHandler};
use crate::render::pipeline::VKGraphicsPipelineSource;
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};

/// Represents a particular attachment for a render pass
#[derive(Debug)]
pub enum AttachmentType {
    /// A transient attachment used within a render pass, for passing data between subpasses.
    Transient(Arc<ImageView>),

    /// Marker that represents an attachment that is used as the output for a render pass.
    ///
    /// For example, the final image that gets rendered for presenting in a swapchain would use
    /// this marker.
    Output,
}

impl Clone for AttachmentType {
    fn clone(&self) -> Self {
        match self {
            AttachmentType::Transient(image)
                => AttachmentType::Transient(image.clone()),
            AttachmentType::Output => AttachmentType::Output,
        }
    }
}

/// Mapping of attachments to their friendly name.
pub trait AttachmentMap: Send + Sync {
    /// Retrieves an attachment by name and framebuffer index.
    ///
    /// If there are no attachments associated by that name, or the framebuffer index is out of
    /// bounds, returns None.
    fn get_attachment(&self, name: String, frame_index: usize) -> Option<AttachmentType>;

    /// Retrieves all attachments (in order) for a particular framebuffer index.
    ///
    /// If the framebuffer index is out of bounds, returns None.
    fn get_attachments_for_frame(&self, frame_index: usize) -> Option<Vec<AttachmentType>>;
}

#[derive(Default)]
struct AttachmentSetState {
    attachment_map: Option<Arc<dyn AttachmentMap>>,
    descriptor_sets: OnceLock<Vec<Arc<PersistentDescriptorSet>>>,
}

impl AttachmentSetState {
    fn get_or_init_descriptor_sets(
        &self,
        target: &Arc<dyn RenderTarget>,
        graphics: &Arc<dyn VKGraphicsPipelineSource>,
        input_names: &Vec<String>,
        set_index: u32,
    ) -> &Vec<Arc<PersistentDescriptorSet>> {
        self.descriptor_sets.get_or_init(|| {
            let graphics = graphics.vk_graphics();
            let attachment_map = self.attachment_map.as_ref()
                .expect("AttachmentSet not initialized");

            let input_buffers = (0..target.framebuffer_count()).into_iter().map(|frame_idx| {
                input_names.iter().map(|color| {
                    match attachment_map.get_attachment(color.clone(), frame_idx as usize).unwrap() {
                        AttachmentType::Transient(image_view) => image_view.clone(),
                        _ => panic!("'{color}' attachment was not transient?"),
                    }
                }).collect::<Vec<_>>()
            }).collect::<Vec<_>>();

            let set_layout = graphics.layout().set_layouts().get(set_index as usize).unwrap();
            (0..target.framebuffer_count()).into_iter().map(|frame_idx| {
                PersistentDescriptorSet::new(
                    target.device().descriptor_set_allocator(),
                    set_layout.clone(),
                    input_buffers[frame_idx as usize].iter().enumerate().map(|(idx, buffer)| {
                        WriteDescriptorSet::image_view(idx as u32, buffer.clone())
                    }).collect::<Vec<_>>(),
                    [],
                ).unwrap()
            }).collect::<Vec<_>>()
        })
    }
}

impl Debug for AttachmentSetState {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentSetState")
            .finish_non_exhaustive()
    }
}

/// Helper struct for maintaining subpass attachments.
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// [re]: crate::render::Renderer
pub struct AttachmentSet {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<dyn VKGraphicsPipelineSource>,
    input_names: Vec<String>,
    set_index: u32,
    inner: Mutex<AttachmentSetState>,
}

impl Debug for AttachmentSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentSet")
            .field("target", &self.target)
            .field("input_names", &self.input_names)
            .field("set_index", &self.set_index)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl AttachmentSet {
    /// Create a new subpass attachment set.
    ///
    /// The `input_names` should match to valid attachments in the render pass's [AttachmentMap].
    pub fn new(
        renderer: &RendererHandle,
        graphics: Arc<dyn VKGraphicsPipelineSource>,
        input_names: Vec<String>,
        set_index: u32,
    ) -> Arc<Self> {
        let attachment_set = Arc::new(Self {
            target: renderer.target().clone(),
            graphics,
            input_names,
            set_index,
            inner: Mutex::new(AttachmentSetState::default()),
        });
        renderer.event_bus().register(RendererEventType::Stale, attachment_set.clone());
        attachment_set
    }

    /// Initialize this [AttachmentSet] with the render pass's [AttachmentMap].
    ///
    /// Should be called exactly once.
    pub fn init(&self, attachment_map: Arc<dyn AttachmentMap>) {
        self.inner.lock().attachment_map = Some(attachment_map);
    }

    /// Get the descriptor set associated with this [AttachmentSet], creating it if necessary.
    pub fn descriptor_set(&self, frame_idx: usize) -> Option<Arc<PersistentDescriptorSet>> {
        self.inner.lock().get_or_init_descriptor_sets(
            &self.target,
            &self.graphics,
            &self.input_names,
            self.set_index
        ).get(frame_idx).map(Arc::clone)
    }
}

impl EventHandler<RendererEventType, RendererEventData> for AttachmentSet {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "AttachmentSet can only handle 'Stale' Renderer events");
        self.inner.lock().descriptor_sets.take();
    }
}