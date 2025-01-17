use std::fmt;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::mpsc;
use tracing::{event, Level};

use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::allocator::{StandardCommandBufferAllocator, StandardCommandBufferAllocatorCreateInfo};
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage};
use vulkano::descriptor_set::allocator::{StandardDescriptorSetAllocator, StandardDescriptorSetAllocatorCreateInfo};
use vulkano::device::physical::{PhysicalDevice, PhysicalDeviceType};
use vulkano::device::{Device as VKDevice, DeviceCreateInfo, DeviceExtensions, Queue, QueueCreateInfo, QueueFlags};
use vulkano::format::Format;
use vulkano::instance::{Instance as VKInstance, InstanceCreateInfo, InstanceExtensions};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::pipeline::graphics::vertex_input::Vertex;
use vulkano::swapchain::{Surface, SurfaceCapabilities};
use vulkano::sync::GpuFuture;
use vulkano::{Validated, VulkanError, VulkanLibrary};

use crate::event::{EventBus, EventBusRegistry, EventType};
use crate::render::render_pass::EngineRenderPass;
use crate::render::swapchain::Swapchain;
use crate::EngineMetadata;

pub mod attachment;
pub mod descriptor_set;
pub mod font;
pub mod image;
pub mod pipeline;
pub mod render_pass;
pub mod swapchain;
pub mod uniform;

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
/// Wraps both a [vulkano::device::Device] and the [vulkano::device::physical::PhysicalDevice]
/// associated with it, in addition to several standard allocators to be used with operations
/// involving the device.
///
/// Generally, a Device is 1:1 with a given render target, and not used across multiple render
/// targets.
pub struct Device {
    physical_device: Arc<PhysicalDevice>,
    vk_device: Arc<VKDevice>,
    memory_allocator: Arc<StandardMemoryAllocator>,
    command_buffer_allocator: Arc<StandardCommandBufferAllocator>,
    descriptor_set_allocator: Arc<StandardDescriptorSetAllocator>,
    queue: Arc<Queue>,
}

impl Device {
    /// Constructs a Device for the given Vulkano Instance and Surface.
    ///
    /// This generally only needs to be used if you are implementing a custom RenderTarget, as
    /// otherwise Device creation is handled by the engine.
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

        let (vk_device, mut queues) = VKDevice::new(
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

        Device {
            physical_device,
            vk_device,
            memory_allocator,
            command_buffer_allocator,
            descriptor_set_allocator,
            queue,
        }
    }

    /// The [vulkano::device::physical::PhysicalDevice] associated with this device.
    #[inline]
    pub fn physical_device(&self) -> &Arc<PhysicalDevice> { &self.physical_device }

    /// The [vulkano::device::Device] associated with this device.
    #[inline]
    pub fn vk_device(&self) -> &Arc<VKDevice> { &self.vk_device }

    /// A [StandardMemoryAllocator] for allocating Vulkan structs.
    #[inline]
    pub fn memory_allocator(&self) -> &Arc<StandardMemoryAllocator> { &self.memory_allocator }

    /// A [StandardCommandBufferAllocator] for allocating Vulkan command buffers.
    #[inline]
    pub fn command_buffer_allocator(&self) -> &Arc<StandardCommandBufferAllocator> { &self.command_buffer_allocator }

    /// A [StandardDescriptorSetAllocator] for allocating Vulkan descriptor sets.
    #[inline]
    pub fn descriptor_set_allocator(&self) -> &Arc<StandardDescriptorSetAllocator> { &self.descriptor_set_allocator }

    /// The [vulkano::device::Queue] used for this device.
    #[inline]
    pub fn queue(&self) -> &Arc<Queue> { &self.queue }
}

impl Deref for Device {
    type Target = Arc<VKDevice>;

    fn deref(&self) -> &Self::Target {
        self.vk_device()
    }
}

// TODO: Replace with glm
/// Wrapper around an array of two u32s, that is used to represent width/height dimensions.
pub struct Dimensions(pub [u32; 2]);

impl Dimensions {
    /// Calculate the width / height aspect ratio for these dimensions.
    #[inline]
    pub fn aspect_ratio(&self) -> f32 {
        self.0[0] as f32 / self.0[1] as f32
    }

    #[inline]
    pub fn width(&self) -> u32 { self.0[0] }

    #[inline]
    pub fn height(&self) -> u32 { self.0[1] }
}

impl From<Dimensions> for [u32; 2] {
    #[inline]
    fn from(dimensions: Dimensions) -> [u32; 2] { dimensions.0 }
}

impl From<Dimensions> for [u32; 3] {
    #[inline]
    fn from(dimensions: Dimensions) -> [u32; 3] { [dimensions.0[0], dimensions.0[1], 1] }
}

impl From<Dimensions> for [f32; 2] {
    #[inline]
    fn from(dimensions: Dimensions) -> [f32; 2] { dimensions.0.map(|v| v as f32) }
}

impl From<Dimensions> for glm::TVec2<u32> {
    #[inline]
    fn from(dimensions: Dimensions) -> Self { glm::vec2(dimensions.0[0], dimensions.0[1]) }
}

impl From<Dimensions> for glm::TVec2<f32> {
    #[inline]
    fn from(dimensions: Dimensions) -> Self { glm::vec2(dimensions.0[0] as f32, dimensions.0[1] as f32) }
}

/// Represents a target for rendering to.
///
/// Custom render targets can be created by implementing this trait, but the gTether engine library
/// provides several of its own render targets that are intended for common use-cases.
pub trait RenderTarget: Debug + Send + Sync + 'static {
    /// The [Surface] that is being rendered to.
    fn surface(&self) -> &Arc<Surface>;
    /// The [Dimensions] of the surface that is being rendered to.
    fn dimensions(&self) -> Dimensions;
    /// The scale factor that should be applied to the surface for e.g. text rendering.
    fn scale_factor(&self) -> f64;
    /// The [Device] for accessing device-specific structures.
    fn device(&self) -> &Arc<Device>;

    /// The pixel [Format] used to render to the [Surface].
    ///
    /// The default implementation will return the first valid [Format] for this target's
    /// [PhysicalDevice].
    #[inline]
    fn format(&self) -> Format {
        self.device().physical_device()
            .surface_formats(&self.surface(), Default::default())
            .unwrap()[0].0
    }

    /// The [SurfaceCapabilities] for this target's [Surface].
    #[inline]
    fn capabilities(&self) -> SurfaceCapabilities {
        self.device().physical_device()
            .surface_capabilities(self.surface(), Default::default())
            .unwrap()
    }

    /// How many Framebuffers this target is using to render
    #[inline]
    fn framebuffer_count(&self) -> u32 {
        self.capabilities().min_image_count + 1
    }
}

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
}

impl RendererEventData {
    /// The [RenderTarget] the [Renderer] is using.
    #[inline]
    pub fn target(&self) -> &Arc<dyn RenderTarget> { &self.target }
}

/// Overall structure that handles rendering for a particular render target.
///
/// Given a [RenderTarget] and an [EngineRenderPass], the [Renderer] will handle the actual logic
/// around rendering to the target using an internally handled swapchain.
pub struct Renderer {
    target: Arc<dyn RenderTarget>,
    render_pass: Arc<dyn EngineRenderPass>,
    swapchain: Swapchain,
    stale: bool,
    event_bus: Arc<EventBus<RendererEventType, RendererEventData>>,
    recv_render_pass: mpsc::Receiver<Arc<dyn EngineRenderPass>>,
}

impl Renderer {
    /// Create a new renderer for a particular [RenderTarget] and [EngineRenderPass].
    pub fn new(target: &Arc<dyn RenderTarget>, render_pass: Arc<dyn EngineRenderPass>) -> (Self, RendererHandle) {
        let (send_render_pass, recv_render_pass) = mpsc::channel();

        let swapchain = Swapchain::new(target, &*render_pass);

        let event_bus = Arc::new(EventBus::default());

        let renderer = Self {
            target: target.clone(),
            render_pass,
            swapchain,
            stale: false,
            event_bus: event_bus.clone(),
            recv_render_pass,
        };

        let handle = RendererHandle {
            target: target.clone(),
            event_bus,
            send_render_pass,
        };

        (renderer, handle)
    }

    /// The [EventBusRegistry] for this Renderer's [events][re].
    ///
    /// [re]: RendererEventType
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry<RendererEventType, RendererEventData> {
        self.event_bus.registry()
    }

    /// Replace the current [EngineRenderPass] with a new one.
    ///
    /// Marks the renderer as stale, and will cause a [stale event][se] to be fired during the next
    /// call to [Renderer::render()].
    ///
    /// [se]: RendererEventType::Stale
    pub fn set_render_pass(&mut self, render_pass: Arc<dyn EngineRenderPass>) {
        self.render_pass = render_pass;
        self.stale = true;
    }

    /// Render a frame.
    ///
    /// Handles the logic required for recreating stale resources, rendering to a free framebuffer,
    /// and swapping framebuffers in the swapchain.
    pub fn render(&mut self) {
        // Check for any outside updates
        while let Ok(render_pass) = self.recv_render_pass.try_recv() {
            self.set_render_pass(render_pass);
        }

        let target = &self.target;
        let device = target.device();

        if self.stale {
            event!(Level::TRACE, "Renderer for target {target:?} is stale, recreating");
            self.event_bus.fire(RendererEventType::Stale, RendererEventData {
                target: target.clone(),
            });
            self.swapchain.recreate(&*self.render_pass)
                .expect("Failed to recreate swapchain");

            self.stale = false;
        }

        let pre_render_event = self.event_bus.fire(RendererEventType::PreRender, RendererEventData {
            target: target.clone(),
        });
        if pre_render_event.is_cancelled() {
            return
        }

        let (frame, suboptimal, join_future) = match self.swapchain.acquire_next_frame() {
            Ok(r) => r,
            Err(VulkanError::OutOfDate) => {
                self.stale = true;
                return
            },
            Err(e) => panic!("Failed to acquire next frame: {e}"),
        };
        if suboptimal { self.stale = true; }

        let mut command_builder = AutoCommandBufferBuilder::primary(
            device.command_buffer_allocator().as_ref(),
            device.queue().queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        ).map_err(Validated::unwrap)
            .expect("Failed to allocate command builder");
        self.render_pass.build_commands(&mut command_builder, frame);
        let command = command_builder.build().map_err(Validated::unwrap)
            .expect("Failed to build command");

        let command_future = join_future
            .then_execute(device.queue().clone(), command).unwrap();

        match frame.flush_command(command_future) {
            Ok(_) => {},
            Err(VulkanError::OutOfDate) => { self.stale = true; },
            Err(e) => panic!("Failed to flush command: {e}"),
        };

        self.event_bus.fire(RendererEventType::PostRender, RendererEventData {
            target: target.clone(),
        });
    }

    /// Mark this renderer as stale, requiring a recreation of some resources.
    ///
    /// Used for example when the render target's surface changes size.
    ///
    /// Will fire a [stale event][se] during the next call to [Renderer::render()].
    ///
    /// [se]: RendererEventType::Stale
    #[inline]
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }
}

/// Threaded handle to a [Renderer].
///
/// This handle is cheaply cloneable, and can be used to interact with a [Renderer] from other
/// threads.
#[derive(Clone)]
pub struct RendererHandle {
    target: Arc<dyn RenderTarget>,
    event_bus: Arc<EventBus<RendererEventType, RendererEventData>>,
    send_render_pass: mpsc::Sender<Arc<dyn EngineRenderPass>>,
}

impl Debug for RendererHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("RendererHandle")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl RendererHandle {
    /// The [RenderTarget] for this [Renderer].
    #[inline]
    pub fn target(&self) -> &Arc<dyn RenderTarget> { &self.target }

    /// The [EventBusRegistry] for this [Renderer].
    #[inline]
    pub fn event_bus(&self) -> &EventBusRegistry<RendererEventType, RendererEventData> {
        self.event_bus.registry()
    }

    /// Replace this [Renderer]'s [EngineRenderPass] with a new one.
    ///
    /// Sends the new [EngineRenderPass] to the renderer's owning thread. This call is non-blocking,
    /// so it is not guaranteed that the renderer's render pass has been replaced before this call
    /// returns.
    ///
    /// # Errors
    ///
    /// - Errors if the renderer has been destroyed.
    pub fn set_render_pass(&self, render_pass: Arc<dyn EngineRenderPass>)
            -> Result<(), mpsc::SendError<Arc<dyn EngineRenderPass>>> {
        self.send_render_pass.send(render_pass)
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