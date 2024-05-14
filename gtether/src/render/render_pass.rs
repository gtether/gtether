use std::sync::Arc;

use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::image::view::ImageView;
use vulkano::render_pass::{RenderPass, Subpass};

use crate::render::RenderTarget;
use crate::render::swapchain::Framebuffer;

/// Represents a particular attachment for a render pass
pub enum AttachmentBuffer {
    /// Represents a transient attachment used within a render pass, for passing data between
    /// subpasses.
    Transient(Arc<ImageView>),

    /// Marker that represents an attachment that is used as the output for a render pass.
    ///
    /// For example, the final image that gets rendered for presenting in a swapchain would use
    /// this marker.
    Output(),
}

/// Mapping of attachments to their friendly name.
pub trait AttachmentMap {
    /// Retrieves an attachment by name and framebuffer index.
    ///
    /// If there are no attachments associated by that name, or the framebuffer index is out of
    /// bounds, returns None.
    fn get_attachment(&self, name: String, frame_index: usize) -> Option<&AttachmentBuffer>;
    /// Retrieves all attachments (in order) for a particular framebuffer index.
    ///
    /// If the framebuffer index is out of bounds, returns None.
    fn get_attachments_for_frame(&self, frame_index: usize) -> Option<&Vec<AttachmentBuffer>>;
}

/// Represents a Vulkan render pass, and all the logic associated with it.
///
/// The gTether engine provides some standard implementations of [EngineRenderPass], but custom
/// implementations can also be used if desired.
///
/// Implementations must be [Send]able so that they can be passed off to separate render threads.
pub trait EngineRenderPass: Send {
    /// The [vulkano::render_pass::RenderPass] that is created and maintained by this [EngineRenderPass].
    fn render_pass(&self) -> &Arc<RenderPass>;
    /// A mapping of attachment names to [AttachmentBuffer]s.
    fn attachment_map(&self) -> Arc<dyn AttachmentMap>;
    /// Build the render commands used to render a particular frame.
    ///
    /// In general, this delegates to [EngineRenderHandler]s to build specific commands.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
    /// Recreate this [EngineRenderPass] for a given [RenderTarget].
    ///
    /// This is generally called when a render target invalidates it's swapchain for one reason or
    /// another, such as when the surface resizes. It is expected that all resources that need to be
    /// recreated when a swapchain invalidates are done so here.
    fn recreate(&mut self, target: &Arc<dyn RenderTarget>);
}

/// An individual render handler, used for rendering specific details of subpasses.
///
/// [EngineRenderHandler] is usually implemented by engine users for custom rendering logic, but the
/// gTether engine does provide several built-in handlers for certain use-cases.
///
/// Implementations must be [Send]able so that they can be passed off to separate render threads.
pub trait EngineRenderHandler: Send {
    /// Build the specific render commands used to render the part of the frame this handler is
    /// responsible for.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
    /// Recreate this [EngineRenderHandler] for a given [RenderTarget] and [Subpass].
    ///
    /// This is generally called when a render target invalidates it's swapchain for one reason or
    /// another, such as when the surface resizes. It is expected that all resources that need to be
    /// recreated when a swapchain invalidates are done so here.
    fn recreate(&mut self, target: &Arc<dyn RenderTarget>, subpass: &Subpass, attachments: &Arc<dyn AttachmentMap>);
}