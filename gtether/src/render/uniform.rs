//! Utilities for Vulkan uniforms.
//!
//! This module contains helper utilities for working with Vulkan uniforms. These come in two
//! primary flavors; one for a [single uniform](Uniform), and one for a
//! [set of uniforms](UniformSet) that share the same index. The latter are intended for iterating
//! over during multiple render calls, such as when working with light sources.
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! uniforms.

use parking_lot::{MappedRwLockReadGuard, MappedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, OnceLock};
use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::Pipeline;

use crate::event::{Event, EventHandler};
use crate::render::pipeline::VKGraphicsPipelineSource;
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};

struct UniformData<T: Clone + BufferContents> {
    value: T,
    descriptor_set: OnceLock<Arc<PersistentDescriptorSet>>,
}

impl<T: Clone + BufferContents> Debug for UniformData<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformData")
            .finish_non_exhaustive()
    }
}

impl<T: Clone + BufferContents> UniformData<T> {
    fn new(value: T) -> Self {
        Self {
            value,
            descriptor_set: OnceLock::new(),
        }
    }

    fn get_or_init_descriptor_set(
        &self,
        target: &Arc<dyn RenderTarget>,
        graphics: &Arc<dyn VKGraphicsPipelineSource>,
        set_index: u32,
    ) -> &Arc<PersistentDescriptorSet> {
        self.descriptor_set.get_or_init(|| {
            let graphics = graphics.vk_graphics();

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
                self.value.clone(),
            ).unwrap();

            PersistentDescriptorSet::new(
                target.device().descriptor_set_allocator(),
                graphics.layout().set_layouts().get(set_index as usize).unwrap().clone(),
                [
                    WriteDescriptorSet::buffer(0, buffer),
                ],
                [],
            ).unwrap()
        })
    }
}

pub type UniformReadGuard<'a, T> = MappedRwLockReadGuard<'a, T>;
pub type UniformWriteGuard<'a, T> = MappedRwLockWriteGuard<'a, T>;

/// Helper struct for maintaining a Vulkan uniform.
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// [re]: crate::render::Renderer
pub struct Uniform<T: Clone + BufferContents> {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<dyn VKGraphicsPipelineSource>,
    set_index: u32,
    inner: RwLock<UniformData<T>>,
}

impl<T: Clone + BufferContents> Debug for Uniform<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Uniform")
            .field("target", &self.target)
            .field("set_index", &self.set_index)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T: Clone + BufferContents> Uniform<T> {
    /// Create a new uniform.
    #[inline]
    pub fn new(
        value: T,
        renderer: &RendererHandle,
        graphics: Arc<dyn VKGraphicsPipelineSource>,
        set_index: u32,
    ) -> Arc<Self> {
        let uniform = Arc::new(Self {
            target: renderer.target().clone(),
            graphics,
            set_index,
            inner: RwLock::new(UniformData::new(value)),
        });
        renderer.event_bus().register(RendererEventType::Stale, uniform.clone());
        uniform
    }

    /// Get a read lock on this uniform's value.
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, T> {
        RwLockReadGuard::map(self.inner.read(), |inner| &inner.value)
    }

    /// Get a write lock on this uniform's value.
    ///
    /// The descriptor set associated with this uniform will be automatically invalidated, forcing
    /// recreation next time it is needed.
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, T> {
        let mut lock = self.inner.write();
        // Assume any write operation is going to mutate the data, requiring a new descriptor set
        lock.descriptor_set.take();
        RwLockWriteGuard::map(lock, |inner| &mut inner.value)
    }

    /// Get the descriptor set associated with this uniform, creating it if necessary.
    #[inline]
    pub fn descriptor_set(&self) -> Arc<PersistentDescriptorSet> {
        self.inner.read().get_or_init_descriptor_set(
            &self.target,
            &self.graphics,
            self.set_index,
        ).clone()
    }
}

impl<T: Clone + BufferContents> EventHandler<RendererEventType, RendererEventData> for Uniform<T> {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "Uniform can only handle 'Stale' Renderer events");
        self.inner.write().descriptor_set.take();
    }
}

struct UniformSetData<T: Clone + BufferContents> {
    values: Vec<T>,
    descriptor_sets: OnceLock<Vec<Arc<PersistentDescriptorSet>>>,
}

impl<T: Clone + BufferContents> Debug for UniformSetData<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformSetData")
            .finish_non_exhaustive()
    }
}

impl<T: Clone + BufferContents> UniformSetData<T> {
    fn new(values: Vec<T>) -> Self {
        Self {
            values,
            descriptor_sets: OnceLock::new(),
        }
    }

    fn get_or_init_descriptor_sets(
        &self,
        target: &Arc<dyn RenderTarget>,
        graphics: &Arc<dyn VKGraphicsPipelineSource>,
        set_index: u32,
    ) -> &Vec<Arc<PersistentDescriptorSet>> {
        self.descriptor_sets.get_or_init(|| {
            let graphics = graphics.vk_graphics();

            self.values.iter().map(|value| {
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
                ).unwrap();

                PersistentDescriptorSet::new(
                    target.device().descriptor_set_allocator(),
                    graphics.layout().set_layouts().get(set_index as usize).unwrap().clone(),
                    [
                        WriteDescriptorSet::buffer(0, buffer),
                    ],
                    [],
                ).unwrap()
            }).collect::<Vec<_>>()
        })
    }
}

/// Helper struct for maintaining a set of Vulkan uniforms.
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// This struct differs from [Uniform] in that it contains a dynamic list of values, where one
/// uniform will be created for each value in this set, but all uniforms will use the same
/// descriptor set index. This is useful for a collection of values that you want to execute
/// separate render calls for, such as different positional lights.
///
/// [re]: crate::render::Renderer
pub struct UniformSet<T: Clone + BufferContents> {
    target: Arc<dyn RenderTarget>,
    graphics: Arc<dyn VKGraphicsPipelineSource>,
    set_index: u32,
    inner: RwLock<UniformSetData<T>>,
}

impl<T: Clone + BufferContents> Debug for UniformSet<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformSet")
            .field("target", &self.target)
            .field("set_index", &self.set_index)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T: Clone + BufferContents> UniformSet<T> {
    /// Create a new uniform set.
    #[inline]
    pub fn new(
        values: Vec<T>,
        renderer: &RendererHandle,
        graphics: Arc<dyn VKGraphicsPipelineSource>,
        set_index: u32,
    ) -> Arc<Self> {
        let uniform_set = Arc::new(Self {
            target: renderer.target().clone(),
            graphics,
            set_index,
            inner: RwLock::new(UniformSetData::new(values)),
        });
        renderer.event_bus().register(RendererEventType::Stale, uniform_set.clone());
        uniform_set
    }

    /// Get a read lock on this uniform's values.
    #[inline]
    pub fn read(&self) -> UniformReadGuard<'_, Vec<T>> {
        RwLockReadGuard::map(self.inner.read(), |inner| &inner.values)
    }

    /// Get a write lock on this uniform's values.
    ///
    /// The descriptor sets associated with this uniform set will be automatically invalidated,
    /// forcing recreation next time they are needed.
    ///
    /// Values may be added or removed; new descriptor sets will be created as needed.
    #[inline]
    pub fn write(&self) -> UniformWriteGuard<'_, Vec<T>> {
        let mut lock = self.inner.write();
        // Assume any write operation is going to mutate the data, requiring new descriptor sets
        lock.descriptor_sets.take();
        RwLockWriteGuard::map(lock, |inner| &mut inner.values)
    }

    /// Get the descriptor sets associated with this uniform set, creating them if necessary.
    ///
    /// One descriptor set will be returned for each value in this uniform set, and they will all
    /// use the same set index.
    #[inline]
    pub fn descriptor_sets(&self) -> Vec<Arc<PersistentDescriptorSet>> {
        self.inner.read().get_or_init_descriptor_sets(
            &self.target,
            &self.graphics,
            self.set_index,
        ).clone()
    }
}

impl<T: Clone + BufferContents> EventHandler<RendererEventType, RendererEventData> for UniformSet<T> {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "UniformSet can only handle 'Stale' Renderer events");
        self.inner.write().descriptor_sets.take();
    }
}