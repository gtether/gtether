use std::sync::Arc;
use vulkano::buffer::{AllocateBufferError, Buffer, BufferContents, BufferCreateInfo, BufferUsage, IndexBuffer, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::graphics::vertex_input::Vertex;
use vulkano::{Validated, ValidationError};

use crate::render::EngineDevice;

pub mod obj;

#[derive(BufferContents, Vertex, Debug, Clone)]
#[repr(C)]
pub struct ModelVertex {
    #[format(R32G32B32_SFLOAT)]
    position: glm::TVec3<f32>,
}

#[derive(BufferContents, Vertex, Debug, Clone)]
#[repr(C)]
pub struct ModelVertexNormal {
    #[format(R32G32B32_SFLOAT)]
    position: glm::TVec3<f32>,
    #[format(R32G32B32_SFLOAT)]
    normal: glm::TVec3<f32>,
}

#[derive(BufferContents, Vertex, Debug, Clone)]
#[repr(C)]
pub struct ModelVertexNormalColor {
    #[format(R32G32B32_SFLOAT)]
    position: glm::TVec3<f32>,
    #[format(R32G32B32_SFLOAT)]
    normal: glm::TVec3<f32>,
    #[format(R32G32B32_SFLOAT)]
    color: glm::TVec3<f32>,
}

#[derive(BufferContents, Vertex, Debug, Clone)]
#[repr(C)]
pub struct ModelVertexNormalTex {
    #[format(R32G32B32_SFLOAT)]
    position: glm::TVec3<f32>,
    #[format(R32G32B32_SFLOAT)]
    normal: glm::TVec3<f32>,
    #[format(R32G32_SFLOAT)]
    tex_coord: glm::TVec2<f32>,
}

pub trait Index: BufferContents {
    #[inline]
    fn create_subbuffer<I>(
        device: &Arc<EngineDevice>,
        iter: I,
    ) -> Result<Subbuffer<[Self]>, Validated<AllocateBufferError>>
    where
        Self: Sized,
        I: IntoIterator<Item=Self>,
        I::IntoIter: ExactSizeIterator,
    {
        Buffer::from_iter(
            device.memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::INDEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            iter,
        )
    }

    fn create_index_buffer<I>(
        device: &Arc<EngineDevice>,
        iter: I,
    ) -> Result<IndexBuffer, Validated<AllocateBufferError>>
    where
        I: IntoIterator<Item=Self>,
        I::IntoIter: ExactSizeIterator;
}

impl Index for u8 {
    #[inline]
    fn create_index_buffer<I>(
        device: &Arc<EngineDevice>,
        iter: I,
    ) -> Result<IndexBuffer, Validated<AllocateBufferError>>
    where
        I: IntoIterator<Item=Self>,
        I::IntoIter: ExactSizeIterator,
    {
        Self::create_subbuffer(device, iter).map(Into::into)
    }
}

impl Index for u16 {
    #[inline]
    fn create_index_buffer<I>(
        device: &Arc<EngineDevice>,
        iter: I,
    ) -> Result<IndexBuffer, Validated<AllocateBufferError>>
    where
        I: IntoIterator<Item=Self>,
        I::IntoIter: ExactSizeIterator,
    {
        Self::create_subbuffer(device, iter).map(Into::into)
    }
}

impl Index for u32 {
    #[inline]
    fn create_index_buffer<I>(
        device: &Arc<EngineDevice>,
        iter: I,
    ) -> Result<IndexBuffer, Validated<AllocateBufferError>>
    where
        I: IntoIterator<Item=Self>,
        I::IntoIter: ExactSizeIterator,
    {
        Self::create_subbuffer(device, iter).map(Into::into)
    }
}

#[derive(Debug)]
pub struct Model<V: Vertex> {
    vertex_buffer: Subbuffer<[V]>,
    index_buffer: Option<IndexBuffer>,
}

impl<V: Vertex> Model<V> {
    #[inline]
    pub fn from_buffers(
        vertex_buffer: Subbuffer<[V]>,
        index_buffer: Option<impl Into<IndexBuffer>>,
    ) -> Self {
        Self {
            vertex_buffer,
            index_buffer: index_buffer.map(Into::into),
        }
    }

    pub fn from_raw<I, IV, II>(
        device: &Arc<EngineDevice>,
        vertices: IV,
        indices: Option<II>,
    ) -> Result<Self, Validated<AllocateBufferError>>
    where
        I: Index,
        IV: IntoIterator<Item=V>,
        IV::IntoIter: ExactSizeIterator,
        II: IntoIterator<Item=I>,
        II::IntoIter: ExactSizeIterator,
    {
        let vertex_buffer = Buffer::from_iter(
            device.memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            vertices,
        )?;
        let index_buffer = indices
            .map(|indices| I::create_index_buffer(device, indices))
            .transpose()?;

        Ok(Self::from_buffers(
            vertex_buffer,
            index_buffer,
        ))
    }

    #[inline]
    pub fn vertex_buffer(&self) -> &Subbuffer<[V]> {
        &self.vertex_buffer
    }

    #[inline]
    pub fn index_buffer(&self) -> Option<&IndexBuffer> {
        self.index_buffer.as_ref()
    }

    pub fn draw(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Box<ValidationError>> {
        builder.bind_vertex_buffers(0, self.vertex_buffer.clone())?;
        if let Some(index_buffer) = &self.index_buffer {
            builder
                .bind_index_buffer(index_buffer.clone())?
                .draw_indexed(index_buffer.len() as u32, 1, 0, 0, 0)?;
        } else {
            builder.draw(self.vertex_buffer.len() as u32, 1, 0, 0)?;
        }
        Ok(())
    }

    pub fn draw_instanced<I: Vertex>(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        instance_buffer: Subbuffer<[I]>,
    ) -> Result<(), Box<ValidationError>> {
        let instance_buffer_len = instance_buffer.len() as u32;
        builder.bind_vertex_buffers(0, (
            self.vertex_buffer.clone(),
            instance_buffer,
        ))?;
        if let Some(index_buffer) = &self.index_buffer {
            builder
                .bind_index_buffer(index_buffer.clone())?
                .draw_indexed(index_buffer.len() as u32, instance_buffer_len, 0, 0, 0)?;
        } else {
            builder.draw(self.vertex_buffer.len() as u32, instance_buffer_len, 0, 0)?;
        }
        Ok(())
    }
}