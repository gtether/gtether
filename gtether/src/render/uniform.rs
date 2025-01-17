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
use parking_lot::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use smallvec::SmallVec;
use std::fmt::Debug;
use std::sync::Arc;
use vulkano::buffer::{AllocateBufferError, Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::descriptor_set::{DescriptorBufferInfo, WriteDescriptorSet};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::{DeviceSize, Validated};

use crate::render::descriptor_set::{DescriptorOffsetIter, VKDescriptorSource};
use crate::render::RenderTarget;

#[derive(Debug)]
struct UniformBuffer<T: ?Sized> {
    buffer: Subbuffer<T>,
    stale: bool,
}

pub type UniformReadGuard<'a, T> = RwLockReadGuard<'a, T>;
pub type UniformWriteGuard<'a, T> = RwLockWriteGuard<'a, T>;

/// Helper struct for maintaining a Vulkan uniform.
///
/// Internally, this struct maintains a series of uniform buffers, one per frame in the target's
/// framebuffer. These buffers are eventually consistent. Whenever the value of this Uniform is
/// written to, the buffers will be marked as stale. Stale buffers are updated on a frame-by-frame
/// basis to the latest value when any descriptor sets that reference this uniform are used.
#[derive(Debug)]
pub struct Uniform<T: Clone + BufferContents> {
    // NOTE: When locking both value and buffers, ALWAYS lock buffers first to avoid deadlocks
    value: RwLock<T>,
    buffers: Mutex<SmallVec<[UniformBuffer<T>; 3]>>,
}

impl<T: Clone + BufferContents> Uniform<T> {
    /// Create a new uniform, using the target's framebuffer count.
    pub fn new(
        target: &Arc<dyn RenderTarget>,
        value: T,
    ) -> Result<Self, Validated<AllocateBufferError>> {
        let buffers = (0..target.framebuffer_count()).into_iter()
            .map(|_| {
                let buffer = Buffer::from_data(
                    target.device().memory_allocator().clone(),
                    BufferCreateInfo {
                        usage: BufferUsage::UNIFORM_BUFFER,
                        ..Default::default()
                    },
                    AllocationCreateInfo {
                        memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                        ..Default::default()
                    },
                    value.clone(),
                )?;
                Ok(UniformBuffer {
                    buffer,
                    stale: false,
                })
            }).collect::<Result<SmallVec<_>, Validated<AllocateBufferError>>>()?;

        Ok(Self {
            value: RwLock::new(value),
            buffers: Mutex::new(buffers),
        })
    }

    /// Get a read lock on this uniform's value.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::RenderTarget;
    /// use gtether::render::uniform::Uniform;
    ///
    /// # fn wrapper(target: &Arc<dyn RenderTarget>) -> Result<(), Validated<AllocateBufferError>> {
    /// let value: u64 = 42;
    /// let uniform = Arc::new(Uniform::new(target, value)?);
    /// assert_eq!(*uniform.read(), value);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, T> {
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
    /// # use gtether::render::RenderTarget;
    /// use gtether::render::uniform::Uniform;
    ///
    /// # fn wrapper(target: &Arc<dyn RenderTarget>) -> Result<(), Validated<AllocateBufferError>> {
    /// let value: u64 = 0;
    /// let uniform = Arc::new(Uniform::new(target, value)?);
    /// *uniform.write() = 42;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, T> {
        let mut buffers = self.buffers.lock();
        let lock = self.value.write();
        // Assume any write operation is going to mutate the data
        for buffer in &mut *buffers {
            buffer.stale = true;
        }
        lock
    }
}

impl<T: BufferContents + Debug + Clone> VKDescriptorSource for Uniform<T> {
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let buffers = self.buffers.lock();
        let buffer = buffers.get(frame_idx)
            .expect("Frame count should match target's frame count");
        (
            WriteDescriptorSet::buffer(binding, buffer.buffer.clone()),
            // These buffers are never replaced, and so never need a descriptor recreated
            0,
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        let mut buffers = self.buffers.lock();
        let value = self.value.read();
        let buffer = buffers.get_mut(frame_idx)
            .expect("Frame count should match target's frame count");
        if buffer.stale {
            let mut write_guard = buffer.buffer.write().unwrap();
            *write_guard = (*value).clone();
            buffer.stale = false;
        }
        // These buffers are never replaced, and so never need a descriptor recreated
        0
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
#[derive(Debug)]
pub struct UniformSet<T: Clone + BufferContents, const CAP: usize> {
    // NOTE: When locking both uniforms and buffers, ALWAYS lock buffers first to avoid deadlocks
    uniforms: RwLock<ArrayVec<T, CAP>>,
    buffers: Mutex<SmallVec<[UniformSetBuffer; 3]>>,
    align: usize,
}

impl<T: Clone + BufferContents + NoUninit, const CAP: usize> UniformSet<T, CAP> {
    fn fill_buffer(buffer: &mut UniformSetBuffer, uniforms: &ArrayVec<T, CAP>, align: usize) {
        let mut write_guard = buffer.buffer.write().unwrap();
        for (val_idx, value) in uniforms.iter().enumerate() {
            let bytes = bytemuck::bytes_of(value);
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

    /// Create a new uniform set, using the target's framebuffer count.
    pub fn new(
        target: &Arc<dyn RenderTarget>,
        values: impl IntoIterator<Item=T>,
    ) -> Result<Self, Validated<AllocateBufferError>> {
        let uniforms = values.into_iter().collect::<ArrayVec<_, CAP>>();

        let min_dynamic_align = target.device().vk_device()
            .physical_device()
            .properties().min_uniform_buffer_offset_alignment
            .as_devicesize() as usize;
        // Round size up to the next multiple of align
        let align = (size_of::<T>() + min_dynamic_align - 1) & !(min_dynamic_align - 1);

        let buffers = (0..target.framebuffer_count()).into_iter()
            .map(|_| {
                let buffer = Buffer::new_slice::<u8>(
                    target.device().memory_allocator().clone(),
                    BufferCreateInfo {
                        usage: BufferUsage::UNIFORM_BUFFER,
                        ..Default::default()
                    },
                    AllocationCreateInfo {
                        memory_type_filter: MemoryTypeFilter::PREFER_DEVICE | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
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
                    &uniforms,
                    align,
                );

                Ok(uniform_buffer)
            }).collect::<Result<SmallVec<_>, Validated<AllocateBufferError>>>()?;

        Ok(Self {
            uniforms: RwLock::new(uniforms),
            buffers: Mutex::new(buffers),
            align,
        })
    }
}

impl<T: Clone + BufferContents, const CAP: usize> UniformSet<T, CAP> {
    /// Get a read lock on this uniform set's values.
    ///
    /// ```
    /// use std::sync::Arc;
    /// # use vulkano::buffer::AllocateBufferError;
    /// # use vulkano::Validated;
    /// # use gtether::render::RenderTarget;
    /// use gtether::render::uniform::UniformSet;
    ///
    /// # fn wrapper(target: &Arc<dyn RenderTarget>) -> Result<(), Validated<AllocateBufferError>> {
    /// let values = [0, 1, 2];
    /// let uniform: Arc<UniformSet<u64, 8>> = Arc::new(UniformSet::new(target, values)?);
    /// assert_eq!(uniform.read().as_slice(), &[0, 1, 2]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, ArrayVec<T, CAP>> {
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
    /// # use gtether::render::RenderTarget;
    /// use gtether::render::uniform::UniformSet;
    ///
    /// # fn wrapper(target: &Arc<dyn RenderTarget>) -> Result<(), Validated<AllocateBufferError>> {
    /// let values = [0, 1, 2];
    /// let uniform: Arc<UniformSet<u64, 8>> = Arc::new(UniformSet::new(target, values)?);
    /// let mut write_guard = uniform.write();
    /// write_guard[1] = 42;
    /// write_guard.push(9001);
    /// assert_eq!(write_guard.as_slice(), &[0, 42, 2, 9001]);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, ArrayVec<T, CAP>> {
        let mut buffers = self.buffers.lock();
        let lock = self.uniforms.write();
        // Assume any write operation is going to mutate the data
        for buffer in &mut *buffers {
            buffer.stale = true;
        }
        lock
    }
}

impl<T: BufferContents + Debug + Clone + NoUninit, const CAP: usize> VKDescriptorSource for UniformSet<T, CAP> {
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
        let buffers = self.buffers.lock();
        let buffer = buffers.get(frame_idx)
            .expect("Frame count should match target's frame count");
        (
            WriteDescriptorSet::buffer_with_range(
                binding,
                DescriptorBufferInfo {
                    buffer: buffer.buffer.clone(),
                    range: 0..size_of::<T>() as DeviceSize,
                },
            ),
            // These buffers are never replaced, and so never need a descriptor recreated
            0,
        )
    }

    fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
        let mut buffers = self.buffers.lock();
        let values = self.uniforms.read();
        let buffer = buffers.get_mut(frame_idx)
            .expect("Frame count should match target's frame count");
        if buffer.stale {
            Self::fill_buffer(
                buffer,
                &values,
                self.align,
            );
            buffer.stale = false;
        }
        // These buffers are never replaced, and so never need a descriptor recreated
        0
    }

    fn descriptor_offsets(&self, frame_idx: usize) -> Option<DescriptorOffsetIter> {
        let buffers = self.buffers.lock();
        let buffer = buffers.get(frame_idx)
            .expect("Frame count should match target's frame count");
        Some(DescriptorOffsetIter::new(self.align as u32, (buffer.len * self.align) as u32))
    }
}