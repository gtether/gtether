//! All logic related to swapchain framebuffers and per-frame logic.
//!
//! While the swapchain itself is an internal implementation to engine
//! [Renderers](crate::render::Renderer), individual framebuffers can be referenced via
//! [EngineFramebuffer]. These EngineFramebuffers offer a few utilities, with the primary one being
//! access to that frame's attachment data, which is useful for creating descriptor sets with
//! transient attachment descriptors. As a shortcut, [FrameManager] (which can be accessed from a
//! [Renderer](crate::render::Renderer::frame_manager)) offers the method
//! [attachment_descriptor()](FrameManagerExt::attachment_descriptor) for generating such
//! descriptors.
//!
//! In addition to access to EngineFramebuffers, this module also contains [FrameSet]. This struct
//! is useful for managing per-frame data, such as uniform buffers. FrameSets wrap a generic type
//! `T`, and will automatically maintain a set of [Frames](Frame) corresponding with how many
//! framebuffers are in the relevant [FrameManager].

use std::error::Error;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use ahash::HashMap;
use educe::Educe;
use itertools::Itertools;
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
use smallvec::SmallVec;
use tracing::warn;
use vulkano::command_buffer::CommandBufferExecFuture;
use vulkano::image::view::ImageView;
use vulkano::render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass};
use vulkano::swapchain::{PresentFuture, Swapchain, SwapchainAcquireFuture, SwapchainPresentInfo};
use vulkano::sync::future::{FenceSignalFuture, JoinFuture};
use vulkano::sync::GpuFuture;
use vulkano::{sync, Validated, VulkanError};
use vulkano::device::{DeviceOwned, Queue};
use vulkano::image::{Image, ImageCreateInfo, ImageUsage};
use vulkano::memory::allocator::AllocationCreateInfo;

use crate::render::attachment::{AttachmentDescriptor, AttachmentType};
use crate::render::{EngineDevice, RenderTarget, VulkanoError};

type FrameFence = FenceSignalFuture<PresentFuture<CommandBufferExecFuture<
    JoinFuture<Box<dyn GpuFuture + Send + Sync>, SwapchainAcquireFuture>
>>>;

/// Representation of a single rendering framebuffer.
///
/// Wraps a Vulkan [Framebuffer], with additional functionality. Also contains any frame-based
/// attachment data, such as images for transient attachments.
#[derive(Educe)]
#[educe(Debug)]
pub struct EngineFramebuffer {
    index: usize,
    vk_queue: Arc<Queue>,
    vk_swapchain: Arc<Swapchain>,
    vk_framebuffer: Arc<Framebuffer>,
    attachments: Vec<AttachmentType>,
    name_map: Arc<HashMap<String, u32>>,
    #[educe(Debug(ignore))]
    fence: Mutex<Option<Arc<FrameFence>>>,
}

impl EngineFramebuffer {
    fn new(
        index: usize,
        device: &Arc<EngineDevice>,
        vk_swapchain: Arc<Swapchain>,
        vk_render_pass: Arc<RenderPass>,
        output_image: Arc<Image>,
        attachment_create_info: Vec<ImageCreateInfo>,
        name_map: Arc<HashMap<String, u32>>,
    ) -> Result<Arc<Self>, Validated<VulkanoError>> {
        let (attachments, framebuffer_images): (Vec<_>, Vec<_>) = attachment_create_info.into_iter()
            .map(move |create_info| {
                if create_info.usage.contains(ImageUsage::TRANSIENT_ATTACHMENT) {
                    let image_view = ImageView::new_default(
                        Image::new(
                            device.memory_allocator().clone(),
                            create_info,
                            AllocationCreateInfo::default(),
                        ).map_err(VulkanoError::from_validated)?
                    ).map_err(VulkanoError::from_validated)?;
                    Ok::<_, Validated<VulkanoError>>((
                        AttachmentType::Transient(image_view.clone()),
                        image_view,
                    ))
                } else {
                    let image_view = ImageView::new_default(
                        output_image.clone()
                    ).map_err(VulkanoError::from_validated)?;
                    Ok::<_, Validated<VulkanoError>>((
                        AttachmentType::Output,
                        image_view,
                    ))
                }
            })
            .process_results(|iter| iter.unzip())?;

        let vk_framebuffer = Framebuffer::new(
            vk_render_pass,
            FramebufferCreateInfo {
                attachments: framebuffer_images,
                ..Default::default()
            },
        ).map_err(VulkanoError::from_validated)?;

        Ok(Arc::new(Self {
            index,
            vk_queue: device.queue().clone(),
            vk_swapchain,
            vk_framebuffer,
            attachments,
            name_map,
            fence: Mutex::new(None),
        }))
    }

    /// Create a set of framebuffers using pre-created images.
    ///
    /// Given a series of swapchain output images, this method generates a framebuffer for each one.
    /// If the render pass has defined any attachments, any needed attachment images will be
    /// created, and mapped via the given `name_map`.
    pub fn for_images(
        target: &Arc<dyn RenderTarget>,
        device: &Arc<EngineDevice>,
        vk_swapchain: &Arc<Swapchain>,
        vk_render_pass: &Arc<RenderPass>,
        images: Vec<Arc<Image>>,
        name_map: HashMap<String, u32>,
    ) -> Result<SmallVec<[Arc<EngineFramebuffer>; 3]>, Validated<VulkanoError>> {
        let mut create_infos = vk_render_pass.attachments().iter()
            .map(|atch| ImageCreateInfo {
                extent: target.extent().fixed_resize::<3, 1>(1).into(),
                format: atch.format,
                usage: ImageUsage::empty(),
                ..Default::default()
            }).collect::<Vec<_>>();

        for subpass in vk_render_pass.subpasses().iter() {
            // TODO: Need to go through *_resolve attachments?
            for color_atch in subpass.color_attachments.iter().flatten() {
                create_infos[color_atch.attachment as usize].usage = ImageUsage::COLOR_ATTACHMENT;
            }
            for depth_stencil_atch in subpass.depth_stencil_attachment.iter() {
                create_infos[depth_stencil_atch.attachment as usize].usage
                    = ImageUsage::DEPTH_STENCIL_ATTACHMENT | ImageUsage::TRANSIENT_ATTACHMENT;
            }
            for input_atch in subpass.input_attachments.iter().flatten() {
                create_infos[input_atch.attachment as usize].usage
                    |= ImageUsage::TRANSIENT_ATTACHMENT | ImageUsage::INPUT_ATTACHMENT;
            }
        }

        let name_map = Arc::new(name_map);

        images.into_iter().enumerate()
            .map(|(idx, image)| {
                EngineFramebuffer::new(
                    idx,
                    device,
                    vk_swapchain.clone(),
                    vk_render_pass.clone(),
                    image,
                    create_infos.clone(),
                    name_map.clone(),
                )
            })
            .collect::<Result<SmallVec<_>, _>>()
    }

    /// Index of this particular framebuffer.
    #[inline]
    pub fn index(&self) -> usize { self.index }

    /// Wait for this framebuffer to be ready.
    ///
    /// If this framebuffer has been used for a rendering pass, will wait until said pass has
    /// finished rendering and has been presented via the swapchain.
    ///
    /// See also: [Self::get_future()]
    #[inline]
    pub fn wait_ready(&self) {
        if let Some(fence) = &*self.fence.lock() {
            fence.wait(None).unwrap();
        }
    }

    /// Get a future representing when this framebuffer will be ready.
    ///
    /// If this framebuffer has been used for a rendering pass, yields a future that represents when
    /// said pass has finished rendering and has been presented via the swapchain.
    ///
    /// See also: [Self::wait_ready()]
    #[inline]
    pub fn get_future(&self) -> Box<dyn GpuFuture + Send + Sync> {
        if let Some(fence) = self.fence.lock().clone() {
            fence.boxed_send_sync()
        } else {
            let mut now = sync::now(self.vk_swapchain.device().clone());
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
                self.vk_queue.clone(),
                SwapchainPresentInfo::swapchain_image_index(self.vk_swapchain.clone(), self.index as u32),
            ).then_signal_fence_and_flush()
            .map_err(Validated::unwrap);
        let future = match result {
            Ok(value) => Some(Arc::new(value)),
            Err(VulkanError::OutOfDate) => {
                suboptimal = true;
                None
            },
            Err(error) => {
                warn!(?error, "Failed to flush future");
                None
            }
        };
        *self.fence.lock() = future;

        if suboptimal {
            return Err(VulkanError::OutOfDate)
        }

        Ok(())
    }

    /// Reference to the wrapped Vulkan [Framebuffer].
    #[inline]
    pub fn buffer(&self) -> &Arc<Framebuffer> { &self.vk_framebuffer }

    /// Get an attachment for this framebuffer.
    ///
    /// If this framebuffer has an attachment keyed by the given name, yields it. Otherwise yields
    /// `None`.
    #[inline]
    pub fn get_attachment(&self, name: impl Into<String>) -> Option<AttachmentType> {
        let name = name.into();
        let atch_idx = self.name_map.get(&name)?.clone();
        Some(self.attachments.get(atch_idx as usize)?.clone())
    }
}

/// Container for frame-based data.
///
/// Holds data of type `T`. This container represents data for a single frame in a given set of
/// frames (e.g. from a swapchain).
#[derive(Debug)]
pub struct Frame<T> {
    buffer: Arc<EngineFramebuffer>,
    value: T,
}

impl<T> Frame<T> {
    /// Create a new frame container.
    pub fn new(buffer: Arc<EngineFramebuffer>, value: T) -> Self {
        Self {
            buffer,
            value,
        }
    }

    /// The framebuffer index of this frame container.
    #[inline]
    pub fn index(&self) -> usize {
        self.buffer.index()
    }
}

impl<T> Deref for Frame<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for Frame<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<R, T> AsRef<R> for Frame<T>
where
    R: ?Sized,
    <Frame<T> as Deref>::Target: AsRef<R>,
{
    #[inline]
    fn as_ref(&self) -> &R {
        self.deref().as_ref()
    }
}

impl<R, T> AsMut<R> for Frame<T>
where
    <Frame<T> as Deref>::Target: AsMut<R>,
{
    #[inline]
    fn as_mut(&mut self) -> &mut R {
        self.deref_mut().as_mut()
    }
}

/// Configuration for how [FrameSets](FrameSet) update their [Frames](Frame).
///
/// When FrameSets need to be recreated due to e.g. swapchain updates, this configuration controls
/// the method in which new Frames are generated.
///
/// The default is [RecreateAll](FrameSetUpdateStyle::RecreateAll).
#[derive(Debug, Clone, Copy)]
pub enum FrameSetUpdateStyle {
    /// When generating new [Frames](Frame), re-use existing frame data.
    ///
    /// For example, if a [FrameSet] has data for frames 0 and 1, and the FrameSet is recreated
    /// with 3 frames, the new frames 0 and 1 will use the same data from the previous frames 0 and
    /// 1, while the new frame 2 will generate new data.
    KeepExisting,

    /// When generating new [Frames](Frame), always generate new frame data.
    ///
    /// Discard any existing frame data when a [FrameSet] is recreated, and always generate new
    /// data.
    ///
    /// This is the default.
    RecreateAll,
}

impl Default for FrameSetUpdateStyle {
    #[inline]
    fn default() -> Self {
        Self::RecreateAll
    }
}

type CreateFrameDataFn<T, E> = dyn (
    Fn(&EngineFramebuffer) -> Result<T, E>
) + Send + Sync + 'static;

/// Set of [Frames](Frame) that are kept in sync with framebuffers, e.g. from a swapchain.
///
/// FrameSets hold per-frame data, and will automatically generate more data per-frame when their
/// source [framebuffers](EngineFramebuffer) are changed. They use a [FrameManager] to accomplish
/// this, and register themselves to said FrameManager.
#[derive(Educe)]
#[educe(Debug)]
pub struct FrameSet<T, E>
where
    T: Send + Sync + 'static,
    E: Error + 'static,
{
    manager: Arc<dyn FrameManager>,
    update_style: FrameSetUpdateStyle,
    #[educe(Debug(ignore))]
    create_frame_data: Box<CreateFrameDataFn<T, E>>,
    frames: Mutex<Option<SmallVec<[Frame<T>; 3]>>>,
}

impl<T, E> FrameSet<T, E>
where
    T: Send + Sync + 'static,
    E: Error + 'static,
{
    fn create_frames(
        manager: &Arc<dyn FrameManager>,
        existing_frames: Option<SmallVec<[Frame<T>; 3]>>,
        create_frame_data: &Box<CreateFrameDataFn<T, E>>,
    ) -> Result<SmallVec<[Frame<T>; 3]>, E> {
        if let Some(existing_frames) = existing_frames {
            existing_frames.into_iter()
                .map(|v| Some(v))
                .chain(std::iter::repeat_with(|| None::<Frame<T>>))
                .zip(manager.framebuffers())
                .map(|(existing_frame, framebuffer)| {
                    if let Some(frame) = existing_frame {
                        Ok(frame)
                    } else {
                        create_frame_data(&*framebuffer)
                            .map(|value| Frame::new(framebuffer, value))
                    }
                })
                .collect::<Result<SmallVec<_>, _>>()
        } else {
            manager.framebuffers().into_iter()
                .map(|framebuffer| {
                    create_frame_data(&*framebuffer)
                        .map(|value| Frame::new(framebuffer.clone(), value))
                })
                .collect::<Result<SmallVec<_>, _>>()
        }
    }

    /// Create a new FrameSet.
    ///
    /// Uses the default [FrameSetUpdateStyle].
    ///
    /// Registers the new FrameSet with the given [FrameManager], which will trigger frame updates
    /// when the source framebuffers change.
    ///
    /// # Errors
    ///
    /// Errors if the `create_frame_data` function errors during initial frame creation.
    #[inline]
    pub fn new(
        manager: Arc<dyn FrameManager>,
        create_frame_data: impl (Fn(&EngineFramebuffer) -> Result<T, E>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, E> {
        Self::with_update_style(
            manager,
            FrameSetUpdateStyle::default(),
            create_frame_data,
        )
    }

    /// Create a new FrameSet with a given `update_style`.
    ///
    /// Registers the new FrameSet with the given [FrameManager], which will trigger frame updates
    /// when the source framebuffers change.
    ///
    /// # Errors
    ///
    /// Errors if the `create_frame_data` function errors during initial frame creation.
    pub fn with_update_style(
        manager: Arc<dyn FrameManager>,
        update_style: FrameSetUpdateStyle,
        create_frame_data: impl (Fn(&EngineFramebuffer) -> Result<T, E>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, E> {
        let frameset = Self::with_config_unregistered(
            manager.clone(),
            update_style,
            create_frame_data,
        )?;
        manager.register_frameset(frameset.clone().into());
        Ok(frameset)
    }

    /// Create a new FrameSet without registering it.
    ///
    /// Uses the default [FrameSetUpdateStyle].
    ///
    /// Does _not_ register the new FrameSet with the given [FrameManager]. The new FrameSet will
    /// need to be manually [recreated](Self::recreate) when necessary.
    ///
    /// # Errors
    ///
    /// Errors if the `create_frame_data` function errors during initial frame creation.
    #[inline]
    pub fn new_unregistered(
        manager: Arc<dyn FrameManager>,
        create_frame_data: impl (Fn(&EngineFramebuffer) -> Result<T, E>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, E> {
        Self::with_config_unregistered(
            manager,
            FrameSetUpdateStyle::default(),
            create_frame_data,
        )
    }

    /// Create a new FrameSet with a given `update_style` and without registering it.
    ///
    /// Does _not_ register the new FrameSet with the given [FrameManager]. The new FrameSet will
    /// need to be manually [recreated](Self::recreate) when necessary.
    ///
    /// # Errors
    ///
    /// Errors if the `create_frame_data` function errors during initial frame creation.
    pub fn with_config_unregistered(
        manager: Arc<dyn FrameManager>,
        update_style: FrameSetUpdateStyle,
        create_frame_data: impl (Fn(&EngineFramebuffer) -> Result<T, E>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, E> {
        let create_frame_data: Box<CreateFrameDataFn<T, E>> = Box::new(create_frame_data);

        let frames = Mutex::new(Some(Self::create_frames(
            &manager,
            None,
            &create_frame_data,
        )?));

        Ok(Arc::new(Self {
            manager: manager.clone(),
            update_style,
            create_frame_data,
            frames,
        }))
    }

    /// Recreate the frame data for this FrameSet.
    ///
    /// This method only needs to be called if the FrameSet is created without registering it with
    /// a [FrameManager]. The standard creation methods automatically register new FrameSets.
    ///
    /// # Errors
    ///
    /// Errors if the `create_frame_data` function errors during frame recreation.
    #[inline]
    pub fn recreate(&self) -> Result<(), E> {
        let mut frames = self.frames.lock();
        match self.update_style {
            FrameSetUpdateStyle::KeepExisting => {
                let existing_frames = frames.take();
                *frames = Some(Self::create_frames(
                    &self.manager,
                    existing_frames,
                    &self.create_frame_data,
                )?)
            },
            FrameSetUpdateStyle::RecreateAll => {
                *frames = Some(Self::create_frames(
                    &self.manager,
                    None,
                    &self.create_frame_data,
                )?)
            }
        }
        Ok(())
    }

    /// Acquire a lock on the current [Frame].
    #[inline]
    pub fn current(&self) -> MappedMutexGuard<Frame<T>> {
        let current_frame_idx = self.manager.current_idx();
        MutexGuard::map(self.frames.lock(), |frames| {
            &mut frames.as_mut().unwrap()[current_frame_idx]
        })
    }

    /// Get the index of the current framebuffer.
    #[inline]
    pub fn current_idx(&self) -> usize {
        self.manager.current_idx()
    }

    /// Acquire a lock on a specific [Frame].
    ///
    /// Yields `None` if the given `frame_idx` is out of bounds.
    #[inline]
    pub fn get(&self, frame_idx: usize) -> Option<MappedMutexGuard<Frame<T>>> {
        let guard = self.frames.lock();
        if frame_idx < guard.as_ref().unwrap().len() {
            Some(MutexGuard::map(guard, |frames| {
                &mut frames.as_mut().unwrap()[frame_idx]
            }))
        } else {
            None
        }
    }

    /// Execute a given function once for each [Frame].
    ///
    /// FrameSets do not include a way to iterate over each [Frame], due to the nature of how they
    /// are locked. Instead, if an operation needs to happen once for each [Frame], use this method.
    #[inline]
    pub fn for_each<F>(&self, f: F)
    where
        F: FnMut(&mut Frame<T>)
    {
        self.frames.lock().as_mut().unwrap().iter_mut().for_each(f)
    }
}

/// A non-generic reference to a [FrameSet].
///
/// This struct is used to refer to a [FrameSet] while erasing generic types. This is useful for
/// e.g. a [FrameManager], which may need to refer to a collection of FrameSets with different
/// generic types.
///
/// FrameSetRefs are weak references, and do not prevent FrameSets from being dropped.
///
/// The only real operation that a FrameSetRef can do is [recreate()](FrameSetRef::recreate).
#[derive(Educe)]
#[educe(Debug)]
pub struct FrameSetRef {
    frame_type: &'static str,
    #[educe(Debug(ignore))]
    recreate_fn: Box<dyn (Fn() -> bool) + Send + Sync + 'static>,
}

impl FrameSetRef {
    /// Create a new FrameSetRef from a [FrameSet].
    ///
    /// Note that `From<FrameSet<T, E>>` is also implemented for FrameSetRef.
    pub fn new<T, E>(frameset: Arc<FrameSet<T, E>>) -> Self
    where
        T: Send + Sync + 'static,
        E: Error + 'static,
    {
        let weak = Arc::downgrade(&frameset);
        Self {
            frame_type: std::any::type_name::<T>(),
            recreate_fn: Box::new(move || {
                if let Some(frameset) = weak.upgrade() {
                    match frameset.recreate() {
                        Ok(_) => {},
                        Err(error) => {
                            warn!(?error, "Failed to recreate frameset");
                        }
                    }
                    true
                } else {
                    false
                }
            }),
        }
    }

    /// Recreate the [FrameSet] that this FrameSetRef refers to.
    ///
    /// If the referred [FrameSet] has been dropped, this method returns `false`; otherwise it
    /// always returns `true`.
    pub fn recreate(&self) -> bool {
        (self.recreate_fn)()
    }
}

impl<T, E> From<Arc<FrameSet<T, E>>> for FrameSetRef
where
    T: Send + Sync + 'static,
    E: Error + 'static,
{
    #[inline]
    fn from(value: Arc<FrameSet<T, E>>) -> Self {
        FrameSetRef::new(value)
    }
}

/// Framebuffer source and manager.
///
/// A FrameManager is where framebuffers are maintained, and where [FrameSets](FrameSet) are
/// registered, so that they may be updated when framebuffers change.
///
/// Generally, a FrameManager is something like a swapchain, but this trait allows it to be
/// anything, as long as it implements the right behavior.
pub trait FrameManager: Debug + Send + Sync + 'static {

    /// Get the current framebuffer index.
    fn current_idx(&self) -> usize;

    /// Get a framebuffer reference.
    ///
    /// Yields `None` if `frame_idx` is out of bounds.
    fn framebuffer(&self, frame_idx: usize) -> Option<Arc<EngineFramebuffer>>;

    /// Get references to all framebuffers.
    fn framebuffers(&self) -> SmallVec<[Arc<EngineFramebuffer>; 3]>;

    /// Register a [FrameSet].
    ///
    /// The FrameSet will be automatically updated whenever this FrameManager's framebuffers are
    /// changed.
    fn register_frameset(&self, frameset: FrameSetRef);
}

/// Extension trait for [FrameManagers](FrameManager).
///
/// This extension trait is automatically implemented on `Arc<FrameManager>`, and adds additional
/// functionality, such as generating [AttachmentDescriptors](AttachmentDescriptor).
pub trait FrameManagerExt {
    /// Create an [AttachmentDescriptor] using this [FrameManager].
    fn attachment_descriptor(&self, input_name: impl Into<String>) -> Arc<AttachmentDescriptor>;
}

impl FrameManagerExt for Arc<dyn FrameManager> {
    #[inline]
    fn attachment_descriptor(&self, input_name: impl Into<String>) -> Arc<AttachmentDescriptor> {
        Arc::new(AttachmentDescriptor::new(
            self.clone(),
            input_name,
        ))
    }
}

impl<M: FrameManager> FrameManagerExt for Arc<M> {
    #[inline]
    fn attachment_descriptor(&self, input_name: impl Into<String>) -> Arc<AttachmentDescriptor> {
        Arc::new(AttachmentDescriptor::new(
            self.clone(),
            input_name,
        ))
    }
}