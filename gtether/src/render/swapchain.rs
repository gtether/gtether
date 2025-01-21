use std::cell::{Cell, RefCell};
use std::ops::Deref;
use std::sync::Arc;
use tracing::{event, Level};

use vulkano::command_buffer::CommandBufferExecFuture;
use vulkano::image::view::ImageView;
use vulkano::image::{Image, ImageUsage};
use vulkano::render_pass::{Framebuffer as VKFramebuffer, FramebufferCreateInfo, RenderPass};
use vulkano::swapchain::{PresentFuture, Swapchain as VKSwapchain, SwapchainAcquireFuture, SwapchainCreateInfo, SwapchainPresentInfo};
use vulkano::sync::future::{FenceSignalFuture, JoinFuture};
use vulkano::sync::GpuFuture;
use vulkano::{swapchain, sync, Validated, VulkanError};

use crate::render::attachment::AttachmentType;
use crate::render::render_pass::EngineRenderPass;
use crate::render::RenderTarget;

type FrameFence = FenceSignalFuture<PresentFuture<CommandBufferExecFuture<JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>>>>;

/// Representation of a single rendering framebuffer
///
/// Wraps a [vulkano::render_pass::Framebuffer], with additional functionality. Note that only the
/// framebuffer index + a reference to the [vulkano::render_pass::Framebuffer] is accessible outside
/// the engine.
pub struct Framebuffer {
    index: usize,
    target: Arc<dyn RenderTarget>,
    vk_swapchain: Arc<VKSwapchain>,
    vk_framebuffer: Arc<VKFramebuffer>,
    fence: RefCell<Option<Arc<FrameFence>>>,
}

impl Framebuffer {
    fn new(
        index: usize,
        target: Arc<dyn RenderTarget>,
        vk_swapchain: Arc<VKSwapchain>,
        render_pass: Arc<RenderPass>,
        attachments: Vec<Arc<ImageView>>,
    ) -> Self {
        let vk_framebuffer = VKFramebuffer::new(
            render_pass,
            FramebufferCreateInfo {
                attachments,
                ..Default::default()
            },
        ).unwrap();

        Self {
            index,
            target,
            vk_swapchain,
            vk_framebuffer,
            fence: RefCell::new(None),
        }
    }

    fn for_images(
        target: &Arc<dyn RenderTarget>,
        vk_swapchain: &Arc<VKSwapchain>,
        render_pass: &dyn EngineRenderPass,
        images: &Vec<Arc<Image>>,
    ) -> Vec<Framebuffer> {
        let attachment_map = render_pass.attachment_map();
        images.iter().enumerate().map(|(idx, image)| {
            let attachments = attachment_map.get_attachments_for_frame(idx).unwrap().iter()
                .map(|atch_buffer| match atch_buffer {
                    AttachmentType::Transient(image_view) => image_view.clone(),
                    AttachmentType::Output => ImageView::new_default(image.clone()).unwrap(),
                }).collect::<Vec<_>>();
            Framebuffer::new(
                idx,
                target.clone(),
                vk_swapchain.clone(),
                render_pass.render_pass().clone(),
                attachments,
            )
        }).collect::<Vec<_>>()
    }

    /// Index of this particular framebuffer.
    #[inline]
    pub fn index(&self) -> usize { self.index }

    #[inline]
    fn wait_ready(&self) {
        if let Some(fence) = &self.fence.borrow().deref() {
            fence.wait(None).unwrap();
        }
    }

    #[inline]
    pub fn get_future(&self) -> Box<dyn GpuFuture + Send + Sync> {
        if let Some(fence) = self.fence.borrow().deref().clone() {
            fence.boxed_send_sync()
        } else {
            let mut now = sync::now(self.target.device().vk_device().clone());
            now.cleanup_finished();
            now.boxed_send_sync()
        }
    }

    #[inline]
    pub(in crate::render) fn flush_command(
        &self,
        command_future: CommandBufferExecFuture<JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>>,
    ) -> Result<(), VulkanError> {
        let mut suboptimal = false;

        let result = command_future
            .then_swapchain_present(
                self.target.device().queue().clone(),
                SwapchainPresentInfo::swapchain_image_index(self.vk_swapchain.clone(), self.index as u32),
            ).then_signal_fence_and_flush()
            .map_err(Validated::unwrap);
        let future = match result {
            Ok(value) => Some(Arc::new(value)),
            Err(VulkanError::OutOfDate) => {
                suboptimal = true;
                None
            },
            Err(e) => {
                event!(Level::WARN, "Failed to flush future: {e}");
                None
            }
        };
        self.fence.replace(future);

        if suboptimal {
            return Err(VulkanError::OutOfDate)
        }

        Ok(())
    }

    /// Reference to the wrapped [vulkano::render_pass::Framebuffer].
    #[inline]
    pub fn buffer(&self) -> &Arc<VKFramebuffer> { &self.vk_framebuffer }
}

pub(in crate::render) struct Swapchain {
    target: Arc<dyn RenderTarget>,
    vk_swapchain: Arc<VKSwapchain>,
    framebuffers: Vec<Framebuffer>,
    prev_idx: Cell<usize>,
}

impl Swapchain {
    pub(in crate::render) fn new(target: &Arc<dyn RenderTarget>, render_pass: &dyn EngineRenderPass) -> Self {
        let device = target.device();
        let physical_device = device.physical_device();

        let caps = physical_device
            .surface_capabilities(&target.surface(), Default::default())
            .expect("Failed to get surface capabilities");
        let composite_alpha = caps.supported_composite_alpha.into_iter().next().unwrap();

        // TODO: Allow configurable present_mode;
        //  Fifo (default) == VSync; look into possibly using Mailbox for VSync as well; works best with at least triple buffering
        //  Immediate == uncapped / no VSync
        let (vk_swapchain, images) = VKSwapchain::new(
            device.vk_device().clone(),
            target.surface().clone(),
            SwapchainCreateInfo {
                min_image_count: caps.min_image_count + 1, // Buffer count
                image_format: target.format(),
                image_extent: target.extent().into(),
                image_usage: ImageUsage::COLOR_ATTACHMENT,
                composite_alpha,
                ..Default::default()
            },
        ).unwrap();

        let framebuffers = Framebuffer::for_images(
            target,
            &vk_swapchain,
            render_pass,
            &images,
        );

        Self {
            target: target.clone(),
            vk_swapchain,
            framebuffers,
            prev_idx: Cell::new(0),
        }
    }

    pub(in crate::render) fn recreate(&mut self, render_pass: &dyn EngineRenderPass) -> Result<(), VulkanError> {
        let (vk_swapchain, images) = self.vk_swapchain.recreate(
            SwapchainCreateInfo {
                image_extent: self.target.extent().into(),
                ..self.vk_swapchain.create_info()
            }
        ).map_err(Validated::unwrap)?;

        let framebuffers = Framebuffer::for_images(
            &self.target,
            &vk_swapchain,
            render_pass,
            &images,
        );

        self.vk_swapchain = vk_swapchain;
        self.framebuffers = framebuffers;
        self.prev_idx.set(0);

        Ok(())
    }

    pub(in crate::render) fn acquire_next_frame(&self)
        -> Result<(&Framebuffer, bool, JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>),
            VulkanError> {
        let (image_idx, suboptimal, acquire_future) =
            swapchain::acquire_next_image(self.vk_swapchain.clone(), None)
                .map_err(Validated::unwrap)?;

        let prev_idx = self.prev_idx.replace(image_idx as usize);

        self.framebuffers[image_idx as usize].wait_ready();

        let join_future = self.framebuffers[prev_idx]
            .get_future().join(acquire_future);

        let frame = &self.framebuffers[image_idx as usize];

        Ok((frame, suboptimal, join_future))
    }
}
