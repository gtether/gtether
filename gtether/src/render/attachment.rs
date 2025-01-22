//! Utilities for Vulkan attachments.
//!
//! This module contains helper utilities for working with Vulkan attachments. This includes things
//! like a [trait][am] for mapping between friendly names and their attachments in a render pass, as
//! well as an [AttachmentDescriptor] for generating descriptors for attachments between subpasses.
//!
//! [am]: AttachmentMap
//!
//! Many of these utilities are entirely optional, and you are free to roll your own logic for
//! Vulkan attachments where applicable.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use ahash::RandomState;
use vulkano::descriptor_set::WriteDescriptorSet;
use vulkano::image::view::ImageView;

use crate::render::descriptor_set::VKDescriptorSource;

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

impl AttachmentType {
    /// Assumes this AttachmentType is [Transient](Self::Transient), and returns the transient
    /// [ImageView], consuming `self`.
    ///
    /// # Panics
    ///
    /// Panics if this AttachmentType is not [Transient](Self::Transient).
    ///
    /// # Examples
    /// ```
    /// # use std::sync::Arc;
    /// # use gtether::render::attachment::AttachmentMap;
    /// # let attachment_map: Arc<dyn AttachmentMap> = return;
    /// # let frame_index: usize = return;
    /// #
    /// let image_view = attachment_map
    ///     .get_attachment("my_transient_attachment".to_owned(), frame_index).unwrap()
    ///     .unwrap_transient();
    /// ```
    pub fn unwrap_transient(self) -> Arc<ImageView> {
        match self {
            Self::Transient(image_view) => image_view,
            _ => panic!("attachment was not transient"),
        }
    }
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

/// Represents a [WriteDescriptor][wd] for a single attachment between subpasses.
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// use vulkano::descriptor_set::layout::DescriptorSetLayout;
///
/// use gtether::render::attachment::{AttachmentDescriptor, AttachmentMap};
/// use gtether::render::descriptor_set::EngineDescriptorSet;
/// use gtether::render::RenderTarget;
/// #
/// # fn get_descriptor_layout() -> Arc<DescriptorSetLayout> { unreachable!() }
///
/// fn create_renderer(
///     target: &Arc<dyn RenderTarget>,
///     attachments: &Arc<dyn AttachmentMap>,
///     // other relevant parameters
/// ) {
///     // Other setup, such as creating pipelines/etc
///
///     // Something in previous setup creates a descriptor set layout
///     let descriptor_set_layout: Arc<DescriptorSetLayout> = get_descriptor_layout();
///
///     let descriptor_set = EngineDescriptorSet::builder(target)
///         .layout(descriptor_set_layout)
///         .descriptor_source(0, Arc::new(AttachmentDescriptor::new(attachments.clone(), "my_attachment")))
///         .build();
/// }
/// ```
///
/// [wd]: WriteDescriptorSet
pub struct AttachmentDescriptor {
    attachment_map: Arc<dyn AttachmentMap>,
    input_name: String,
    random_state: RandomState,
}

impl Debug for AttachmentDescriptor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentDescriptor")
            .field("input_name", &self.input_name)
            .finish_non_exhaustive()
    }
}

impl AttachmentDescriptor {
    /// Create a new AttachmentDescriptor for a particular attachment name.
    pub fn new(
        attachment_map: Arc<dyn AttachmentMap>,
        input_name: impl Into<String>,
    ) -> Self {
        Self {
            attachment_map,
            input_name: input_name.into(),
            random_state: RandomState::new(),
        }
    }
}

impl VKDescriptorSource for AttachmentDescriptor {
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let image_view = self.attachment_map
            .get_attachment(self.input_name.clone(), frame_idx).unwrap()
            .unwrap_transient();
        (
            WriteDescriptorSet::image_view(binding, image_view.clone()),
            self.random_state.hash_one(image_view),
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        // This COULD be expensive; need to keep an eye on it
        let image_view = self.attachment_map
            .get_attachment(self.input_name.clone(), frame_idx).unwrap()
            .unwrap_transient();
        self.random_state.hash_one(image_view)
    }
}