use std::sync::Arc;
use vulkano::command_buffer::allocator::{StandardCommandBufferAllocator, StandardCommandBufferAllocatorCreateInfo};
use vulkano::device::{Device as VKDevice, DeviceCreateInfo, DeviceExtensions, Queue, QueueCreateInfo, QueueFlags};
use vulkano::device::physical::{PhysicalDevice, PhysicalDeviceType};
use vulkano::format::Format;
use vulkano::instance::Instance;
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::swapchain::{Surface, SurfaceCapabilities};
use vulkano::descriptor_set::allocator::{StandardDescriptorSetAllocator, StandardDescriptorSetAllocatorCreateInfo};

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

/// Wrapper around an array of two u32s, that is used to represent width/height dimensions.
pub struct Dimensions([u32; 2]);

impl Dimensions {
    /// Calculate the width / height aspect ratio for these dimensions.
    #[inline]
    pub fn aspect_ratio(&self) -> f32 {
        self.0[0] as f32 / self.0[1] as f32
    }
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

/// Represents a target for rendering to.
///
/// Custom render targets can be created by implementing this trait, but the gTether engine library
/// provides several of its own render targets that are intended for common use-cases.
pub trait RenderTarget: Send + Sync + 'static {
    /// The [Surface] that is being rendered to.
    fn surface(&self) -> &Arc<Surface>;
    /// The [Dimensions] of the surface that is being rendered to.
    fn dimensions(&self) -> Dimensions;
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