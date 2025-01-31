use std::sync::Arc;
use parking_lot::{Mutex, RwLock};
use smallvec::SmallVec;
use vulkano::image::ImageUsage;
use vulkano::swapchain::{Swapchain as VKSwapchain, SwapchainAcquireFuture, SwapchainCreateInfo};
use vulkano::sync::future::JoinFuture;
use vulkano::sync::GpuFuture;
use vulkano::{swapchain, Validated, VulkanError};

use crate::render::frame::{EngineFramebuffer, FrameManager, FrameSetRef};
use crate::render::render_pass::EngineRenderPass;
use crate::render::{EngineDevice, RenderTarget, RenderTargetExt, VulkanoError};

/// Swapchain configuration.
///
/// This configuration controls how framebuffers are created, managed, and presented. See individual
/// members for more.
#[derive(Debug, Clone)]
pub struct EngineSwapchainConfig {
    /// Swapchain buffer count.
    ///
    /// Common values include `2` for double-buffered and `3` for triple-buffered.
    ///
    /// Default is `3`.
    pub framebuffer_count: u32,

    #[doc(hidden)]
    pub _ne: crate::NonExhaustive,
}

impl Default for EngineSwapchainConfig {
    #[inline]
    fn default() -> Self {
        Self {
            framebuffer_count: 3,
            _ne: crate::NonExhaustive(()),
        }
    }
}

#[derive(Debug)]
struct SwapchainFrameState {
    framebuffers: SmallVec<[Arc<EngineFramebuffer>; 3]>,
    current_idx: usize,
}

// TODO: Merge attachment data into this
#[derive(Debug)]
struct SwapchainFrameManager {
    state: RwLock<SwapchainFrameState>,
    // Keep this separate from the state lock,
    // as recreating framesets will require acquiring the state lock
    framesets: Mutex<Vec<FrameSetRef>>,
}

impl SwapchainFrameManager {
    fn new(framebuffers: SmallVec<[Arc<EngineFramebuffer>; 3]>) -> Arc<Self> {
        Arc::new(Self {
            state: RwLock::new(SwapchainFrameState {
                framebuffers,
                current_idx: 0,
            }),
            framesets: Mutex::new(Vec::new()),
        })
    }

    fn recreate(&self, framebuffers: SmallVec<[Arc<EngineFramebuffer>; 3]>) {
        {
            let mut state = self.state.write();
            state.framebuffers = framebuffers;
            state.current_idx = 0;
        }
        {
            let mut framesets = self.framesets.lock();
            framesets.retain(|frameset| {
                frameset.recreate()
            });
        }
    }

    fn acquire_frame(
        &self,
        frame_idx: usize,
        acquire_future: SwapchainAcquireFuture,
    ) -> (Arc<EngineFramebuffer>, JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>) {
        let mut state = self.state.write();
        let join_future = state.framebuffers[state.current_idx]
            .get_future().join(acquire_future);
        state.current_idx = frame_idx;
        let framebuffer = state.framebuffers[state.current_idx].clone();
        framebuffer.wait_ready();
        (framebuffer, join_future)
    }

    fn framebuffer_count(&self) -> u32 {
        self.state.read().framebuffers.len() as u32
    }
}

impl FrameManager for SwapchainFrameManager {
    #[inline]
    fn current_idx(&self) -> usize {
        self.state.read().current_idx
    }

    #[inline]
    fn framebuffer(&self, frame_idx: usize) -> Option<Arc<EngineFramebuffer>> {
        self.state.read().framebuffers.get(frame_idx).cloned()
    }

    #[inline]
    fn framebuffers(&self) -> SmallVec<[Arc<EngineFramebuffer>; 3]> {
        self.state.read().framebuffers.iter()
            .cloned()
            .collect::<SmallVec<_>>()
    }

    #[inline]
    fn register_frameset(&self, frameset: FrameSetRef) {
        self.framesets.lock().push(frameset)
    }
}

pub(in crate::render) struct EngineSwapchain {
    target: Arc<dyn RenderTarget>,
    device: Arc<EngineDevice>,
    vk_swapchain: Arc<VKSwapchain>,
    frame_manager: Arc<SwapchainFrameManager>,
}

impl EngineSwapchain {
    pub(in crate::render) fn new(
        target: Arc<dyn RenderTarget>,
        device: Arc<EngineDevice>,
        render_pass: &dyn EngineRenderPass,
        config: EngineSwapchainConfig,
    ) -> Result<Self, Validated<VulkanoError>> {
        let composite_alpha = target
            .capabilities(&device).map_err(VulkanoError::from_validated)?
            .supported_composite_alpha.into_iter().next().unwrap();

        // TODO: Allow configurable present_mode;
        //  Fifo (default) == VSync; look into possibly using Mailbox for VSync as well; works best with at least triple buffering
        //  Immediate == uncapped / no VSync
        let (vk_swapchain, images) = VKSwapchain::new(
            device.vk_device().clone(),
            target.surface().clone(),
            SwapchainCreateInfo {
                min_image_count: config.framebuffer_count,
                image_format: target.format(&device).map_err(VulkanoError::from_validated)?,
                image_extent: target.extent().into(),
                image_usage: ImageUsage::COLOR_ATTACHMENT,
                composite_alpha,
                ..Default::default()
            },
        ).map_err(VulkanoError::from_validated)?;

        let framebuffers = EngineFramebuffer::for_images(
            &target,
            &device,
            &vk_swapchain,
            render_pass.vk_render_pass(),
            images,
            render_pass.attachment_name_map(),
        )?;
        let frame_manager = SwapchainFrameManager::new(framebuffers);

        Ok(Self {
            target,
            device,
            vk_swapchain,
            frame_manager,
        })
    }

    pub(in crate::render) fn recreate(
        &mut self,
        render_pass: &dyn EngineRenderPass,
    ) -> Result<(), Validated<VulkanoError>> {
        let (vk_swapchain, images) = self.vk_swapchain.recreate(
            SwapchainCreateInfo {
                image_extent: self.target.extent().into(),
                ..self.vk_swapchain.create_info()
            }
        ).map_err(VulkanoError::from_validated)?;
        self.vk_swapchain = vk_swapchain;

        let framebuffers = EngineFramebuffer::for_images(
            &self.target,
            &self.device,
            &self.vk_swapchain,
            render_pass.vk_render_pass(),
            images,
            render_pass.attachment_name_map(),
        )?;
        self.frame_manager.recreate(framebuffers);

        Ok(())
    }

    pub(in crate::render) fn acquire_next_frame(&self)
        -> Result<(Arc<EngineFramebuffer>, bool, JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>),
            VulkanError> {
        let (frame_idx, suboptimal, acquire_future) =
            swapchain::acquire_next_image(self.vk_swapchain.clone(), None)
                .map_err(Validated::unwrap)?;

        let (framebuffer, join_future)
            = self.frame_manager.acquire_frame(frame_idx as usize, acquire_future);

        Ok((framebuffer, suboptimal, join_future))
    }

    #[inline]
    pub fn frame_manager(&self) -> Arc<dyn FrameManager> {
        self.frame_manager.clone()
    }

    #[inline]
    pub fn config(&self) -> EngineSwapchainConfig {
        EngineSwapchainConfig {
            framebuffer_count: self.frame_manager.framebuffer_count(),
            _ne: crate::NonExhaustive(())
        }
    }
}