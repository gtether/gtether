//! Utilities for Vulkan attachments.
//!
//! This module contains helper utilities for working with Vulkan attachments. This includes things
//! like an [AttachmentDescriptor] for generating descriptors for attachments between subpasses.
//!
//! Many of these utilities are entirely optional, and you are free to roll your own logic for
//! Vulkan attachments where applicable.

use ahash::RandomState;
use educe::Educe;
use std::fmt::Debug;
use std::sync::Arc;
use vulkano::descriptor_set::layout::DescriptorSetLayoutBinding;
use vulkano::descriptor_set::WriteDescriptorSet;
use vulkano::image::view::ImageView;

use crate::render::descriptor_set::VKDescriptorSource;
use crate::render::frame::FrameManager;

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
    /// # use vulkano::image::view::ImageView;
    /// use gtether::render::attachment::AttachmentType;
    /// #
    /// # let image_view: Arc<ImageView> = return;
    ///
    /// let attachment = AttachmentType::Transient(image_view.clone());
    /// assert_eq!(image_view, attachment.unwrap_transient());
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

/// Represents a [WriteDescriptor][wd] for a single attachment between subpasses.
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// use vulkano::descriptor_set::layout::DescriptorSetLayout;
///
/// use gtether::render::attachment::AttachmentDescriptor;
/// use gtether::render::descriptor_set::EngineDescriptorSet;
/// use gtether::render::frame::{FrameManager, FrameManagerExt};
/// use gtether::render::Renderer;
/// #
/// # fn get_descriptor_layout() -> Arc<DescriptorSetLayout> { unreachable!() }
///
/// fn create_render_handler(
///     renderer: Arc<Renderer>,
///     frame_manager: Arc<dyn FrameManager>,
///     // other relevant parameters
/// ) {
///     // Other setup, such as creating pipelines/etc
///
///     // Something in previous setup creates a descriptor set layout
///     let descriptor_set_layout: Arc<DescriptorSetLayout> = get_descriptor_layout();
///
///     let descriptor_set = EngineDescriptorSet::builder(renderer)
///         .layout(descriptor_set_layout)
///         .descriptor_source(0, Arc::new(AttachmentDescriptor::new(frame_manager.clone(), "foo")))
///         // Or, using a shortcut
///         .descriptor_source(1, frame_manager.attachment_descriptor("bar"))
///         .build();
/// }
/// ```
///
/// [wd]: WriteDescriptorSet
#[derive(Educe)]
#[educe(Debug)]
pub struct AttachmentDescriptor {
    frames: Arc<dyn FrameManager>,
    input_name: String,
    #[educe(Debug(ignore))]
    random_state: RandomState,
}

impl AttachmentDescriptor {
    /// Create a new AttachmentDescriptor for a particular attachment name.
    pub fn new(
        frames: Arc<dyn FrameManager>,
        input_name: impl Into<String>,
    ) -> Self {
        Self {
            frames,
            input_name: input_name.into(),
            random_state: RandomState::new(),
        }
    }
}

impl VKDescriptorSource for AttachmentDescriptor {
    fn write_descriptor(
        &self,
        frame_idx: usize,
        binding: u32,
        _layout: &DescriptorSetLayoutBinding,
    ) -> (WriteDescriptorSet, u64) {
        let image_view = self.frames
            .framebuffer(frame_idx).unwrap()
            .get_attachment(self.input_name.clone())
                .expect(&format!("No attachment '{}'", &self.input_name))
            .unwrap_transient();
        (
            WriteDescriptorSet::image_view(binding, image_view.clone()),
            self.random_state.hash_one(image_view),
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        // This COULD be expensive; need to keep an eye on it
        let image_view = self.frames
            .framebuffer(frame_idx).unwrap()
            .get_attachment(self.input_name.clone()).unwrap()
            .unwrap_transient();
        self.random_state.hash_one(image_view)
    }
}