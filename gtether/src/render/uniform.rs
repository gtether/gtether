//! Utilities for Vulkan uniforms.
//!
//! This module contains helper utilities for working with Vulkan uniforms. These come in two
//! primary flavors; one for a [single uniform](Uniform), and one for a
//! [set of uniforms](UniformSet) that share the same index. The latter is intended for iterating
//! over during multiple render calls, such as when working with light sources.
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! uniforms.

use arrayvec::ArrayVec;
use bytemuck::NoUninit;
use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::sync::Arc;
use ahash::RandomState;
use vulkano::buffer::{AllocateBufferError, Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::descriptor_set::{DescriptorBufferInfo, WriteDescriptorSet};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::{DeviceSize, Validated};

use crate::render::descriptor_set::{DescriptorOffsetIter, VKDescriptorSource};
use crate::render::frame::{FrameManager, FrameSet, FrameSetUpdateStyle};
use crate::render::EngineDevice;

/// Helper trait that all values wrapped by [Uniform] or [UniformSet] must implement.
///
/// This trait defines how to retrieve buffer contents from the uniform's value. Note that this
/// trait is automatically implemented for any type that also implements [BufferContents], Debug,
/// and Clone, with an implementation that simply clones the type and returns it.
pub trait UniformValue<T: BufferContents>: Debug + Send + Sync + 'static {
    /// Get the buffer contents for this uniforms value.
    fn buffer_contents(&self) -> T;
}

impl<T: Debug + Clone + BufferContents> UniformValue<T> for T {
    #[inline]
    fn buffer_contents(&self) -> T {
        self.clone()
    }
}

struct UniformBuffer<T: ?Sized> {
    buffer: Subbuffer<T>,
    stale: bool,
}

impl<T: ?Sized> Debug for UniformBuffer<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformBuffer")
            .field("buffer", &"...")
            .field("stale", &self.stale)
            .finish()
    }
}

pub type UniformReadGuard<'a, T> = RwLockReadGuard<'a, T>;
pub type UniformWriteGuard<'a, T> = RwLockWriteGuard<'a, T>;

/// Helper struct for maintaining a Vulkan uniform.
///
/// Internally, this struct maintains a series of uniform buffers, one per frame in the target's
/// framebuffer. These buffers are eventually consistent. Whenever the value of this Uniform is
/// written to, the buffers will be marked as stale. Stale buffers are updated on a frame-by-frame
/// basis to the latest value when any descriptor sets that reference this uniform are used.
pub struct Uniform<T: BufferContents, U: UniformValue<T> = T> {
    // NOTE: When locking both value and buffers, ALWAYS lock value first to avoid deadlocks
    value: Arc<RwLock<U>>,
    buffers: Arc<FrameSet<UniformBuffer<T>, Validated<AllocateBufferError>>>,
    random_state: RandomState,
}

impl<T: BufferContents, U: UniformValue<T>> Debug for Uniform<T, U> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uniform")
            .field("value", &self.value)
            .field("buffers", &self.buffers)
            .finish_non_exhaustive()
    }
}

impl<T: BufferContents, U: UniformValue<T>> Uniform<T, U> {
    /// Create a new uniform, with per-frame data managed via the given [FrameManager].
    pub fn new(
        device: Arc<EngineDevice>,
        frame_manager: Arc<dyn FrameManager>,
        value: U,
    ) -> Result<Self, Validated<AllocateBufferError>> {
        let value = Arc::new(RwLock::new(value));

        let buffer_device = device.clone();
        let buffer_value = value.clone();
        let buffers = FrameSet::with_update_style(
            frame_manager,
            FrameSetUpdateStyle::KeepExisting,
            move |_| {
                let buffer = Buffer::from_data(
                    buffer_device.memory_allocator().clone(),
                    BufferCreateInfo {
                        usage: BufferUsage::UNIFORM_BUFFER,
                        ..Default::default()
                    },
                    AllocationCreateInfo {
                        memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                            | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                        ..Default::default()
                    },
                    buffer_value.read().buffer_contents(),
                )?;
                Ok(UniformBuffer {
                    buffer,
                    stale: false,
                })
            },
        )?;

        Ok(Self {
            value,
            buffers,
            random_state: RandomState::new(),
        })
    }

    /// Get a read lock on this uniform's value.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::Renderer;
    /// use gtether::render::uniform::Uniform;
    ///
    /// # fn wrapper(renderer: Arc<Renderer>) -> Result<(), Validated<AllocateBufferError>> {
    /// let value: u64 = 42;
    /// let uniform = Arc::new(Uniform::new(
    ///     renderer.device().clone(),
    ///     renderer.frame_manager(),
    ///     value,
    /// )?);
    /// assert_eq!(*uniform.read(), value);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, U> {
        self.value.read()
    }

    /// Get a write lock on this uniform's value.
    ///
    /// Buffers will be marked as stale, and will be updated the next time they need to be accessed.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::Renderer;
    /// use gtether::render::uniform::Uniform;
    ///
    /// # fn wrapper(renderer: Arc<Renderer>) -> Result<(), Validated<AllocateBufferError>> {
    /// let value: u64 = 0;
    /// let uniform = Arc::new(Uniform::new(
    ///     renderer.device().clone(),
    ///     renderer.frame_manager(),
    ///     value,
    /// )?);
    /// *uniform.write() = 42;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, U> {
        let lock = self.value.write();
        // Assume any write operation is going to mutate the data
        self.buffers.for_each(|buffer| buffer.stale = true);
        lock
    }
}

impl<T: BufferContents, U: UniformValue<T>> VKDescriptorSource for Uniform<T, U> {
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let buffer = self.buffers.get(frame_idx).unwrap();
        (
            WriteDescriptorSet::buffer(binding, buffer.buffer.clone()),
            self.random_state.hash_one(&buffer.buffer),
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        let value = self.value.read();
        let mut buffer = self.buffers.get(frame_idx).unwrap();
        if buffer.stale {
            *buffer.buffer.write().unwrap() = value.buffer_contents();
            buffer.stale = false;
        }
        self.random_state.hash_one(&buffer.buffer)
    }
}

#[derive(Debug)]
struct UniformSetBuffer {
    buffer: Subbuffer<[u8]>,
    len: usize,
    stale: bool,
}

/// Helper struct for maintaining a set of Vulkan uniforms.
///
/// This struct differs from [Uniform] in that it uses dynamic uniform buffers. The buffer capacity
/// is set at creation time using constant generics, but the UniformSet can contain any number of
/// values up to that capacity. All uniforms in this set use the same descriptor index, and the
/// intended use is with [descriptor set offsets][dso].
///
/// Otherwise, this struct functions similarly to [Uniform].
///
/// [dso]: vulkano::descriptor_set::DescriptorSetWithOffsets
pub struct UniformSet<T: BufferContents, const CAP: usize, U: UniformValue<T> = T> {
    // NOTE: When locking both uniforms and buffers, ALWAYS lock uniforms first to avoid deadlocks
    uniforms: Arc<RwLock<ArrayVec<U, CAP>>>,
    buffers: Arc<FrameSet<UniformSetBuffer, Validated<AllocateBufferError>>>,
    align: usize,
    random_state: RandomState,
    _phantom: PhantomData<T>,
}

impl<T: BufferContents, const CAP: usize, U: UniformValue<T>> Debug for UniformSet<T, CAP, U> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformSet")
            .field("uniforms", &self.uniforms)
            .field("buffers", &self.buffers)
            .field("align", &self.align)
            .finish_non_exhaustive()
    }
}

impl<T: BufferContents + NoUninit, const CAP: usize, U: UniformValue<T>> UniformSet<T, CAP, U> {
    fn fill_buffer(buffer: &mut UniformSetBuffer, uniforms: &ArrayVec<U, CAP>, align: usize) {
        let mut write_guard = buffer.buffer.write().unwrap();
        for (val_idx, value) in uniforms.iter().enumerate() {
            let buffer_value = value.buffer_contents();
            let bytes = bytemuck::bytes_of(&buffer_value);
            for (idx, byte) in bytes.iter().enumerate() {
                write_guard[(val_idx * align) + idx] = *byte;
            }
            for idx in bytes.len()..align {
                write_guard[(val_idx * align) + idx] = 0;
            }
        }
        for idx in (uniforms.len() * align)..write_guard.len() {
            write_guard[idx] = 0;
        }
        buffer.len = uniforms.len();
    }

    /// Create a new uniform set, with per-frame data managed via the given [FrameManager].
    pub fn new(
        device: Arc<EngineDevice>,
        frame_manager: Arc<dyn FrameManager>,
        values: impl IntoIterator<Item=U>,
    ) -> Result<Self, Validated<AllocateBufferError>> {
        let uniforms = Arc::new(RwLock::new(
            values.into_iter().collect::<ArrayVec<_, CAP>>()
        ));

        let min_dynamic_align = device.vk_device()
            .physical_device()
            .properties().min_uniform_buffer_offset_alignment
            .as_devicesize() as usize;
        // Round size up to the next multiple of align
        let align = (size_of::<T>() + min_dynamic_align - 1) & !(min_dynamic_align - 1);

        let buffer_device = device.clone();
        let buffer_uniforms = uniforms.clone();
        let buffers = FrameSet::with_update_style(
            frame_manager,
            FrameSetUpdateStyle::KeepExisting,
            move |_| {
                let buffer = Buffer::new_slice::<u8>(
                    buffer_device.memory_allocator().clone(),
                    BufferCreateInfo {
                        usage: BufferUsage::UNIFORM_BUFFER,
                        ..Default::default()
                    },
                    AllocationCreateInfo {
                        memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                            | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                        ..Default::default()
                    },
                    (align * CAP).try_into().unwrap(),
                )?;

                let mut uniform_buffer = UniformSetBuffer {
                    buffer,
                    len: 0,
                    stale: false,
                } ;

                Self::fill_buffer(
                    &mut uniform_buffer,
                    &buffer_uniforms.read(),
                    align,
                );

                Ok(uniform_buffer)
            },
        )?;

        Ok(Self {
            uniforms,
            buffers,
            align,
            random_state: RandomState::new(),
            _phantom: PhantomData,
        })
    }
}

impl<T: BufferContents, const CAP: usize, U: UniformValue<T>> UniformSet<T, CAP, U> {
    /// Get a read lock on this uniform set's values.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::Renderer;
    /// use gtether::render::uniform::UniformSet;
    ///
    /// # fn wrapper(renderer: Arc<Renderer>) -> Result<(), Validated<AllocateBufferError>> {
    /// let values = [0, 1, 2];
    /// let uniform: Arc<UniformSet<u64, 8>> = Arc::new(UniformSet::new(
    ///     renderer.device().clone(),
    ///     renderer.frame_manager(),
    ///     values,
    /// )?);
    /// assert_eq!(uniform.read().as_slice(), &[0, 1, 2]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, ArrayVec<U, CAP>> {
        self.uniforms.read()
    }

    /// Get a write lock on this uniform set's values.
    ///
    /// Buffers will be marked as stale, and will be updated the next time they need to be accessed.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::Renderer;
    /// use gtether::render::uniform::UniformSet;
    ///
    /// # fn wrapper(renderer: Arc<Renderer>) -> Result<(), Validated<AllocateBufferError>> {
    /// let values = [0, 1, 2];
    /// let uniform: Arc<UniformSet<u64, 8>> = Arc::new(UniformSet::new(
    ///     renderer.device().clone(),
    ///     renderer.frame_manager(),
    ///     values,
    /// )?);
    /// let mut write_guard = uniform.write();
    /// write_guard[1] = 42;
    /// write_guard.push(9001);
    /// assert_eq!(write_guard.as_slice(), &[0, 42, 2, 9001]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, ArrayVec<U, CAP>> {
        let lock = self.uniforms.write();
        // Assume any write operation is going to mutate the data
        self.buffers.for_each(|buffer| buffer.stale = true);
        lock
    }
}

impl<T: BufferContents + NoUninit, const CAP: usize, U: UniformValue<T>> VKDescriptorSource for UniformSet<T, CAP, U> {
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let buffer = self.buffers.get(frame_idx).unwrap();
        (
            WriteDescriptorSet::buffer_with_range(
                binding,
                DescriptorBufferInfo {
                    buffer: buffer.buffer.clone(),
                    range: 0..size_of::<T>() as DeviceSize,
                },
            ),
            self.random_state.hash_one(&buffer.buffer),
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        let values = self.uniforms.read();
        let mut buffer = self.buffers.get(frame_idx).unwrap();
        if buffer.stale {
            Self::fill_buffer(
                &mut buffer,
                &values,
                self.align,
            );
            buffer.stale = false;
        }
        self.random_state.hash_one(&buffer.buffer)
    }

    fn descriptor_offsets(&self, frame_idx: usize) -> Option<DescriptorOffsetIter> {
        let buffer = self.buffers.get(frame_idx).unwrap();
        Some(DescriptorOffsetIter::new(self.align as u32, (buffer.len * self.align) as u32))
    }
}