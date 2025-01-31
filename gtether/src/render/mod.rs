use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::ops::Deref;
use std::sync::Arc;
use parking_lot::RwLock;
use tracing::trace;

use vulkano::buffer::{AllocateBufferError, Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::allocator::{StandardCommandBufferAllocator, StandardCommandBufferAllocatorCreateInfo};
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage};
use vulkano::descriptor_set::allocator::{StandardDescriptorSetAllocator, StandardDescriptorSetAllocatorCreateInfo};
use vulkano::device::physical::{PhysicalDevice, PhysicalDeviceType};
use vulkano::device::{Device, DeviceCreateInfo, DeviceExtensions, Queue, QueueCreateInfo, QueueFlags};
use vulkano::format::Format;
use vulkano::instance::{Instance as VKInstance, InstanceCreateInfo, InstanceExtensions};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::pipeline::graphics::vertex_input::Vertex;
use vulkano::swapchain::{Surface, SurfaceCapabilities};
use vulkano::sync::GpuFuture;
use vulkano::{Validated, VulkanError, VulkanLibrary};
use vulkano::image::AllocateImageError;

use crate::event::{EventBus, EventBusRegistry, EventType};
use crate::render::render_pass::{EngineRenderPass, NoOpEngineRenderPass};
use crate::render::swapchain::EngineSwapchain;
use crate::EngineMetadata;
use crate::render::frame::FrameManager;

pub mod attachment;
pub mod descriptor_set;
pub mod font;
pub mod frame;
pub mod image;
pub mod model;
pub mod pipeline;
pub mod render_pass;
mod swapchain;
pub mod uniform;

pub use swapchain::EngineSwapchainConfig;

/// Generic wrapper around any common error type from
/// [Vulkano](https://docs.rs/vulkano/latest/vulkano/).
///
/// This is a helper type designed to allow functions to yield multiple different types of Vulkano
/// errors. It also has support for [Validated] wrappers; see
/// [from_validated()](VulkanoError::from_validated).
///
/// # Examples
/// ```
/// use vulkano::VulkanError;
/// use vulkano::buffer::AllocateBufferError;
///
/// use gtether::render::VulkanoError;
///
/// trait FooBar {
///     fn foo(&self) -> Result<(), VulkanError>;
///     fn bar(&self) -> Result<(), AllocateBufferError>;
/// }
///
/// fn run(foobar: impl FooBar) -> Result<(), VulkanoError> {
///     foobar.foo()?;
///     foobar.bar()?;
///     Ok(())
/// }
/// ```
#[derive(Debug)]
pub enum VulkanoError {
    VulkanError(VulkanError),
    AllocateBufferError(AllocateBufferError),
    AllocateImageError(AllocateImageError),
}

impl Display for VulkanoError {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VulkanError(err) => Display::fmt(err, f),
            Self::AllocateBufferError(err) => Display::fmt(err, f),
            Self::AllocateImageError(err) => Display::fmt(err, f),
        }
    }
}

impl Error for VulkanoError {}

impl From<VulkanError> for VulkanoError {
    #[inline]
    fn from(value: VulkanError) -> Self {
        Self::VulkanError(value)
    }
}

impl From<AllocateBufferError> for VulkanoError {
    #[inline]
    fn from(value: AllocateBufferError) -> Self {
        Self::AllocateBufferError(value)
    }
}

impl From<AllocateImageError> for VulkanoError {
    #[inline]
    fn from(value: AllocateImageError) -> Self {
        Self::AllocateImageError(value)
    }
}

impl VulkanoError {
    /// Convert a [Validated] error into a Validated [VulkanoError].
    ///
    /// ```
    /// use vulkano::{Validated, VulkanError};
    /// use vulkano::buffer::AllocateBufferError;
    ///
    /// use gtether::render::VulkanoError;
    ///
    /// trait FooBar {
    ///     fn foo(&self) -> Result<(), Validated<VulkanError>>;
    ///     fn bar(&self) -> Result<(), Validated<AllocateBufferError>>;
    /// }
    ///
    /// fn run(foobar: impl FooBar) -> Result<(), Validated<VulkanoError>> {
    ///     foobar.foo().map_err(VulkanoError::from_validated)?;
    ///     foobar.bar().map_err(VulkanoError::from_validated)?;
    ///     Ok(())
    /// }
    /// ```
    #[inline]
    pub fn from_validated(value: Validated<impl Into<VulkanoError>>) -> Validated<VulkanoError> {
        value.map(Into::into)
    }
}

pub struct Instance {
    inner: Arc<VKInstance>,
}

impl Instance {
    pub(crate) fn new(engine_metadata: &EngineMetadata, extensions: InstanceExtensions) -> Self {
        let library = VulkanLibrary::new()
            .expect("Failed to load Vulkan library/DLL");

        let inner = VKInstance::new(library, InstanceCreateInfo {
            application_name: engine_metadata.application_name.clone(),
            engine_name: Some("gTether".to_owned()),
            // TODO: Engine version
            // engine_version: ,
            enabled_extensions: extensions,
            ..Default::default()
        }).expect("Failed to create instance");

        Self {
            inner,
        }
    }

    #[inline]
    pub fn vk_instance(&self) -> &Arc<VKInstance> { &self.inner }
}

impl Deref for Instance {
    type Target = Arc<VKInstance>;

    fn deref(&self) -> &Self::Target {
        self.vk_instance()
    }
}

/// Collection of Vulkano structs that together represent a rendering device.
///
/// Wraps both a Vulkano [Device] and the [PhysicalDevice] associated with it, in addition to
/// several standard allocators to be used with operations involving the device.
///
/// Generally, an EngineDevice is 1:1 with a given render target, and not used across multiple
/// render targets. For example, [windows](crate::gui::window) have an EngineDevice associated with
/// them.
#[derive(Debug)]
pub struct EngineDevice {
    physical_device: Arc<PhysicalDevice>,
    vk_device: Arc<Device>,
    memory_allocator: Arc<StandardMemoryAllocator>,
    command_buffer_allocator: Arc<StandardCommandBufferAllocator>,
    descriptor_set_allocator: Arc<StandardDescriptorSetAllocator>,
    queue: Arc<Queue>,
}

impl EngineDevice {
    /// Constructs an EngineDevice for the given Vulkano Instance and Surface.
    ///
    /// This generally only needs to be used if you are implementing a custom RenderTarget, as
    /// otherwise device creation is handled by the engine.
    pub fn for_surface(instance: Arc<Instance>, surface: Arc<Surface>) -> Self {
        // TODO: does this need to be configurable?
        let device_extensions = DeviceExtensions {
            khr_swapchain: true,
            ..DeviceExtensions::empty()
        };

        let (physical_device, queue_family_index) = instance
            .enumerate_physical_devices().expect("Could not enumerate physical devices")
            .filter(|p| p.supported_extensions().contains(&device_extensions))
            .filter_map(|p| {
                p.queue_family_properties().iter().enumerate()
                    .position(|(i, q)| {
                        q.queue_flags.contains(QueueFlags::GRAPHICS)
                            && p.surface_support(i as u32, &surface).unwrap_or(false)
                    })
                    .map(|q| (p, q as u32))
            })
            .min_by_key(|(p, _)| match p.properties().device_type {
                PhysicalDeviceType::DiscreteGpu => 0,
                PhysicalDeviceType::IntegratedGpu => 1,
                PhysicalDeviceType::VirtualGpu => 2,
                PhysicalDeviceType::Cpu => 3,
                _ => 4,
            }).expect("No device available");

        let (vk_device, mut queues) = Device::new(
            physical_device.clone(),
            DeviceCreateInfo {
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index,
                    ..Default::default()
                }],
                enabled_extensions: device_extensions,
                ..Default::default()
            },
        ).expect("Failed to create device");
        let queue = queues.next().unwrap();

        let memory_allocator = Arc::new(StandardMemoryAllocator::new_default(vk_device.clone()));
        let command_buffer_allocator = Arc::new(StandardCommandBufferAllocator::new(
            vk_device.clone(),
            StandardCommandBufferAllocatorCreateInfo::default(),
        ));
        let descriptor_set_allocator = Arc::new(StandardDescriptorSetAllocator::new(
            vk_device.clone(),
            StandardDescriptorSetAllocatorCreateInfo::default(),
        ));

        EngineDevice {
            physical_device,
            vk_device,
            memory_allocator,
            command_buffer_allocator,
            descriptor_set_allocator,
            queue,
        }
    }

    /// The Vulkano [PhysicalDevice] associated with this device.
    #[inline]
    pub fn physical_device(&self) -> &Arc<PhysicalDevice> { &self.physical_device }

    /// The Vulkano [Device] associated with this device.
    #[inline]
    pub fn vk_device(&self) -> &Arc<Device> { &self.vk_device }

    /// A [StandardMemoryAllocator] for allocating Vulkan structs.
    #[inline]
    pub fn memory_allocator(&self) -> &Arc<StandardMemoryAllocator> { &self.memory_allocator }

    /// A [StandardCommandBufferAllocator] for allocating Vulkan command buffers.
    #[inline]
    pub fn command_buffer_allocator(&self) -> &Arc<StandardCommandBufferAllocator> { &self.command_buffer_allocator }

    /// A [StandardDescriptorSetAllocator] for allocating Vulkan descriptor sets.
    #[inline]
    pub fn descriptor_set_allocator(&self) -> &Arc<StandardDescriptorSetAllocator> { &self.descriptor_set_allocator }

    /// The Vulkano [Queue] used for this device.
    #[inline]
    pub fn queue(&self) -> &Arc<Queue> { &self.queue }
}

impl Deref for EngineDevice {
    type Target = Arc<Device>;

    fn deref(&self) -> &Self::Target {
        self.vk_device()
    }
}

/// Represents a target for rendering to.
///
/// Custom render targets can be created by implementing this trait, but the gTether engine library
/// provides several of its own render targets that are intended for common use-cases.
pub trait RenderTarget: Debug + Send + Sync + 'static {
    /// The [Surface] that is being rendered to.
    fn surface(&self) -> &Arc<Surface>;

    /// The [Dimensions] of the surface that is being rendered to.
    fn extent(&self) -> glm::TVec2<u32>;

    /// The scale factor that should be applied to the surface for e.g. text rendering.
    fn scale_factor(&self) -> f64;
}

pub trait RenderTargetExt: RenderTarget {
    /// The pixel [Format] used to render to the [Surface].
    ///
    /// Returns the first valid [Format] for this target's [PhysicalDevice].
    #[inline]
    fn format(&self, device: &Arc<EngineDevice>) -> Result<Format, Validated<VulkanError>> {
        Ok(device.physical_device()
            .surface_formats(&self.surface(), Default::default())?[0].0)
    }

    /// The [SurfaceCapabilities] for this target's [Surface].
    #[inline]
    fn capabilities(&self, device: &Arc<EngineDevice>)
            -> Result<SurfaceCapabilities, Validated<VulkanError>> {
        device.physical_device()
            .surface_capabilities(self.surface(), Default::default())
    }
}

impl<T: RenderTarget + ?Sized> RenderTargetExt for T {}

/// [EventType] for [Renderer][r] [Events][evt].
///
/// See also: [RendererEventData].
///
/// [r]: Renderer
/// [evt]: crate::event::Event
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RendererEventType {
    /// The [Renderer] is stale.
    ///
    /// This event is fired when e.g. the [RenderTarget]'s size has changed, or anything else that
    /// would invalidate the framebuffers. This is a good place to hook into for recreating Vulkan
    /// pipelines / etc.
    Stale,

    /// The [Renderer] is about to render a frame.
    ///
    /// This event is cancellable, and cancelling it will stop the current frame from rendering.
    PreRender,

    /// The [Renderer] has just finished rendering a frame.
    PostRender,
}

impl EventType for RendererEventType {
    fn is_cancellable(&self) -> bool {
        match self {
            RendererEventType::PreRender => true,
            _ => false,
        }
    }
}

/// Event data for [Renderer][r] [Events][evt].
///
/// See also: [RendererEventType].
///
/// [r]: Renderer
/// [evt]: crate::event::Event
pub struct RendererEventData {
    target: Arc<dyn RenderTarget>,
    device: Arc<EngineDevice>,
}

impl RendererEventData {
    /// The [RenderTarget] the [Renderer] is using.
    #[inline]
    pub fn target(&self) -> &Arc<dyn RenderTarget> { &self.target }

    /// The [EngineDevice] the [Renderer] is using.
    #[inline]
    pub fn device(&self) -> &Arc<EngineDevice> { &self.device }
}

/// [Renderer] configuration.
///
/// This configuration controls how a [Renderer] is created and managed. See individual members
/// for more.
#[derive(Debug, Clone)]
pub struct RendererConfig {
    /// Swapchain configuration.
    ///
    /// Sub-configuration that controls the swapchain of the [Renderer]. Includes things like
    /// framebuffer count.
    pub swapchain: EngineSwapchainConfig,

    #[doc(hidden)]
    pub _ne: crate::NonExhaustive,
}

impl Default for RendererConfig {
    #[inline]
    fn default() -> Self {
        Self {
            swapchain: EngineSwapchainConfig::default(),
            _ne: crate::NonExhaustive(()),
        }
    }
}

struct RendererState {
    render_pass: Arc<dyn EngineRenderPass>,
    swapchain: EngineSwapchain,
    stale: bool,
}

/// Overall structure that handles rendering for a particular render target.
///
/// Given a [RenderTarget], [EngineDevice], and an [EngineRenderPass], the Renderer will handle
/// the actual logic around rendering to the target using an internally handled swapchain.
///
/// The Renderer also manages its own lifecycle events for e.g. pre-/post-rendering and stale
/// recreation, which can be [registered to](Renderer::event_bus).
pub struct Renderer {
    target: Arc<dyn RenderTarget>,
    device: Arc<EngineDevice>,
    event_bus: Arc<EventBus<RendererEventType, RendererEventData>>,
    state: RwLock<RendererState>,
}

impl Renderer {
    /// Create a new renderer for a particular [RenderTarget] and [EngineDevice].
    ///
    /// Initializes with a [NoOpEngineRenderPass], which effectively does nothing. A proper render
    /// pass must be later configured using [set_render_pass()](Self::set_render_pass).
    ///
    /// Configured with a default [RendererConfig].
    ///
    /// Note that the engine's [windowing](crate::gui::window) system will create its own Renderers,
    /// so it is not needed to create your own for standard window rendering.
    #[inline]
    pub fn new(
        target: Arc<dyn RenderTarget>,
        device: Arc<EngineDevice>,
    ) -> Result<Self, Validated<VulkanoError>> {
        let render_pass = NoOpEngineRenderPass::new(
            &target,
            &device,
        ).map_err(VulkanoError::from_validated)?;
        Self::with_render_pass_and_config(
            target,
            device,
            render_pass,
            RendererConfig::default(),
        )
    }

    /// Create a new renderer with a given `config`.
    ///
    /// Initializes with a [NoOpEngineRenderPass], which effectively does nothing. A proper render
    /// pass must be later configured using [Self::set_render_pass()].
    ///
    /// See also: [Self::new()]
    #[inline]
    pub fn with_config(
        target: Arc<dyn RenderTarget>,
        device: Arc<EngineDevice>,
        config: RendererConfig,
    ) -> Result<Self, Validated<VulkanoError>> {
        let render_pass = NoOpEngineRenderPass::new(
            &target,
            &device,
        ).map_err(VulkanoError::from_validated)?;
        Self::with_render_pass_and_config(
            target,
            device,
            render_pass,
            config,
        )
    }

    /// Create a new renderer with a given `render_pass`.
    ///
    /// Configured with a default [RendererConfig].
    ///
    /// See also: [Self::new()]
    #[inline]
    pub fn with_render_pass(
        target: Arc<dyn RenderTarget>,
        device: Arc<EngineDevice>,
        render_pass: Arc<dyn EngineRenderPass>,
    ) -> Result<Self, Validated<VulkanoError>> {
        Self::with_render_pass_and_config(
            target,
            device,
            render_pass,
            RendererConfig::default(),
        )
    }

    /// Create a new renderer with a given `render_pass` and `config`.
    ///
    /// See also: [Self::new()]
    pub fn with_render_pass_and_config(
        target: Arc<dyn RenderTarget>,
        device: Arc<EngineDevice>,
        render_pass: Arc<dyn EngineRenderPass>,
        config: RendererConfig,
    ) -> Result<Self, Validated<VulkanoError>> {
        let swapchain = EngineSwapchain::new(
            target.clone(),
            device.clone(),
            &*render_pass,
            config.swapchain,
        )?;

        let event_bus = Arc::new(EventBus::default());

        Ok(Self {
            target,
            device,
            event_bus: event_bus.clone(),
            state: RwLock::new(RendererState {
                render_pass,
                swapchain,
                stale: false,
            }),
        })
    }

    /// The [RenderTarget] that this [Renderer] uses.
    #[inline]
    pub fn target(&self) -> &Arc<dyn RenderTarget> {
        &self.target
    }

    /// The [EngineDevice] that this [Renderer] uses.
    #[inline]
    pub fn device(&self) -> &Arc<EngineDevice> {
        &self.device
    }

    /// The [EventBusRegistry] for this Renderer's [events][re].
    ///
    /// [re]: RendererEventType
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry<RendererEventType, RendererEventData> {
        self.event_bus.registry()
    }

    /// Generate a configuration representing this [Renderer].
    #[inline]
    pub fn config(&self) -> RendererConfig {
        let state = self.state.read();
        RendererConfig {
            swapchain: state.swapchain.config(),
            _ne: crate::NonExhaustive(())
        }
    }

    /// The [FrameManager] that this [Renderer] uses.
    ///
    /// Swapchain logic is internal to the Renderer, but this can be used to get a public-facing API
    /// for general frame management.
    #[inline]
    pub fn frame_manager(&self) -> Arc<dyn FrameManager> {
        self.state.read().swapchain.frame_manager()
    }

    /// Replace the current [EngineRenderPass] with a new one.
    ///
    /// Marks the [Renderer] as stale, and will cause a [stale event][se] to be fired during the
    /// next call to [Renderer::render()].
    ///
    /// [se]: RendererEventType::Stale
    pub fn set_render_pass(&self, render_pass: Arc<dyn EngineRenderPass>) {
        let mut state = self.state.write();
        state.render_pass = render_pass;
        state.stale = true;
    }

    fn event_data(&self) -> RendererEventData {
        RendererEventData {
            target: self.target.clone(),
            device: self.device.clone(),
        }
    }

    /// Render a frame.
    ///
    /// Handles the logic required for recreating stale resources, rendering to a free framebuffer,
    /// and swapping framebuffers in the swapchain.
    ///
    /// Note that for engine created Renderers, this is managed internally, and should not be called
    /// by a user. Instead, access to this API is intended for user-built Renderers.
    pub fn render(&self) {
        let mut state = self.state.upgradable_read();

        if state.stale {
            trace!(target = ?self.target, "Renderer is stale, recreating");
            state.with_upgraded(|state| {
                state.swapchain.recreate(&*state.render_pass)
                    .expect("Failed to recreate swapchain");
                state.stale = false;
            });
            self.event_bus.fire(RendererEventType::Stale, self.event_data());
        }

        let pre_render_event
            = self.event_bus.fire(RendererEventType::PreRender, self.event_data());
        if pre_render_event.is_cancelled() {
            return
        }

        let (frame, suboptimal, join_future) = match state.swapchain.acquire_next_frame() {
            Ok(r) => r,
            Err(VulkanError::OutOfDate) => {
                state.with_upgraded(|state| {
                    state.stale = true;
                });
                return
            },
            Err(e) => panic!("Failed to acquire next frame: {e}"),
        };
        if suboptimal {
            state.with_upgraded(|state| {
                state.stale = true;
            });
        }

        let mut command_builder = AutoCommandBufferBuilder::primary(
            self.device.command_buffer_allocator().as_ref(),
            self.device.queue().queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        ).map_err(Validated::unwrap)
            .expect("Failed to allocate command builder");
        state.render_pass.build_commands(&mut command_builder, &frame).unwrap();
        let command = command_builder.build().map_err(Validated::unwrap)
            .expect("Failed to build command");

        let command_future = join_future
            .then_execute(self.device.queue().clone(), command).unwrap();

        match frame.flush_command(command_future) {
            Ok(_) => {},
            Err(VulkanError::OutOfDate) => {
                state.with_upgraded(|state| {
                    state.stale = true;
                });
            },
            Err(e) => panic!("Failed to flush command: {e}"),
        };

        self.event_bus.fire(RendererEventType::PostRender, self.event_data());
    }

    /// Mark this renderer as stale, requiring a recreation of some resources.
    ///
    /// Used for example when the render target's surface changes size. For engine created
    /// Renderers, this is generally managed by internal lifecycles, so the user does not need to
    /// call this, unless they are doing something custom beyond normal use-cases.
    ///
    /// Will fire a [stale event][se] during the next call to [Renderer::render()].
    ///
    /// [se]: RendererEventType::Stale
    #[inline]
    pub fn mark_stale(&self) {
        self.state.write().stale = true;
    }
}

#[derive(BufferContents, Vertex)]
#[repr(C)]
pub struct FlatVertex {
    #[format(R32G32_SFLOAT)]
    position: [f32; 2],
}

impl FlatVertex {
    pub fn rect(min: glm::TVec2<f32>, max: glm::TVec2<f32>) -> [FlatVertex; 6] {
        [
            FlatVertex { position: [ min.x, min.y ]},
            FlatVertex { position: [ min.x, max.y ]},
            FlatVertex { position: [ max.x, max.y ]},
            FlatVertex { position: [ min.x, min.y ]},
            FlatVertex { position: [ max.x, max.y ]},
            FlatVertex { position: [ max.x, min.y ]},
        ]
    }

    pub fn buffer(
        alloc: Arc<dyn MemoryAllocator>,
        min: glm::TVec2<f32>,
        max: glm::TVec2<f32>,
    ) -> Subbuffer<[Self]> {
        Buffer::from_iter(
            alloc,
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            Self::rect(min, max),
        ).unwrap()
    }

    #[inline]
    pub fn screen_buffer(alloc: Arc<dyn MemoryAllocator>) -> Subbuffer<[Self]> {
        Self::buffer(alloc, glm::vec2(-1.0, -1.0), glm::vec2(1.0, 1.0))
    }
}