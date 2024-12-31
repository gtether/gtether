use std::fmt::Debug;
use std::sync::Arc;

use glm::{identity, TMat4, TVec3};
use gtether::render::RenderTarget;
use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::graphics::vertex_input::Vertex;

pub mod cube;
pub mod ambient;
pub mod directional;

#[derive(BufferContents, Vertex)]
#[repr(C)]
pub struct FlatVertex {
    #[format(R32G32_SFLOAT)]
    position: [f32; 2],
}

impl FlatVertex {
    #[inline]
    fn square() -> [FlatVertex; 6] {
        [
            FlatVertex { position: [ -1.0, -1.0] },
            FlatVertex { position: [ -1.0,  1.0] },
            FlatVertex { position: [  1.0,  1.0] },
            FlatVertex { position: [ -1.0, -1.0] },
            FlatVertex { position: [  1.0,  1.0] },
            FlatVertex { position: [  1.0, -1.0] },
        ]
    }

    #[inline]
    fn screen_buffer(target: &Arc<dyn RenderTarget>) -> Subbuffer<[Self]> {
        Buffer::from_iter(
            target.device().memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            Self::square(),
        ).unwrap()
    }
}

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct MN {
    pub model: TMat4<f32>,
    pub normals: TMat4<f32>,
}

impl MN {
    #[inline]
    pub fn new(model: TMat4<f32>) -> Self {
        Self {
            model,
            normals: glm::inverse_transpose(model),
        }
    }
}

impl Default for MN {
    #[inline]
    fn default() -> Self { Self::new(identity()) }
}

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct VP {
    pub view: TMat4<f32>,
    pub projection: TMat4<f32>,
}

impl VP {
    #[allow(unused)]
    #[inline]
    pub fn look_at(eye: &TVec3<f32>, center: &TVec3<f32>, up: &TVec3<f32>) -> Self {
        VP {
            view: glm::look_at(eye, center, up),
            projection: identity(),
        }
    }
}

impl Default for VP {
    #[inline]
    fn default() -> Self {
        Self {
            view: identity(),
            projection: identity(),
        }
    }
}