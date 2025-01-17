//! Utilities for Vulkan descriptor sets.
//!
//! This module contains helper utilities for working with Vulkan descriptor sets. Notably, this
//! includes [EngineDescriptorSet], which can maintain and recreate a particular descriptor set
//! whenever its descriptors change enough that recreation is needed. [EngineDescriptorSet] uses
//! the [VKDescriptorSource] trait to define its descriptors, which allow user-defined descriptor
//! sources to be provided, in addition to the other utilities that the engine provides.
//!
//! Examples of other engine utilities that implement [VKDescriptorSource]:
//!  * [attachments](crate::render::attachment)
//!  * [images](crate::render::image)
//!  * [uniforms](crate::render::uniform)
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkan
//! descriptor sets.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use parking_lot::Mutex;
use smallvec::SmallVec;
use vulkano::descriptor_set::{DescriptorSet, DescriptorSetWithOffsets, PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::descriptor_set::layout::DescriptorSetLayout;
use vulkano::{Validated, VulkanError};

use crate::render::{Device, RenderTarget};

/// Iterator for descriptor memory offset values
///
/// # Examples
/// ```
/// use gtether::render::descriptor_set::DescriptorOffsetIter;
///
/// let offsets = DescriptorOffsetIter::new(16, 64).collect::<Vec<_>>();
/// assert_eq!(offsets, vec![0, 16, 32, 48]);
/// ```
pub struct DescriptorOffsetIter {
    align: u32,
    size: u32,
    curr_idx: u32,
}

impl DescriptorOffsetIter {
    /// Create a new offset iterator for a given memory alignment and total size.
    #[inline]
    pub fn new(align: u32, size: u32) -> Self {
        Self {
            align,
            size,
            curr_idx: 0,
        }
    }
}

impl Iterator for DescriptorOffsetIter {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.curr_idx * self.align;
        if self.size == 0 {
            None
        } else if offset <= (self.size - self.align) {
            self.curr_idx += 1;
            Some(offset)
        } else {
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let count = self.size / self.align;
        let rem = count - self.curr_idx;
        (rem as usize, Some(rem as usize))
    }
}

impl ExactSizeIterator for DescriptorOffsetIter {}

/// Helper trait for generating Vulkano [WriteDescriptorSets][wds].
///
/// [wds]: WriteDescriptorSet
///
/// # Implementing
///
/// Implementors need to implement both [write_descriptor()][dswd] and
/// [update_descriptor_source()][dsuds].
///
/// [write_descriptor()][dswd] is responsible for creating [WriteDescriptorSet][wds], and will be called
/// once per framebuffer whenever an [EngineDescriptorSet] needs to recreate its descriptor set.
/// This method also yields a [hash](#hashing) that identifies the write descriptor, which the
/// [EngineDescriptorSet] can use to determine when its descriptor set is invalid.
///
/// [update_descriptor_source()][dsuds] is called whenever an [EngineDescriptorSet] is asked for
/// its descriptor set. This method is where implementors should put any logic that needs to update
/// values for a given descriptor, such as writing to underlying buffers. If the update
/// significantly changes data in such a way that a new write descriptor needs to be generated, then
/// this method should yield a different [hash](#hashing) to signify that.
///
/// If the generated write descriptor refers to a dynamic buffer, [descriptor_offsets()][dsdo]
/// should also be implemented to generate the proper offsets for said dynamic buffer. By default,
/// this method yields `None`.
///
/// Basic implementation for e.g. an image view write descriptor:
/// ```
/// use std::sync::Arc;
/// use vulkano::descriptor_set::WriteDescriptorSet;
/// use vulkano::image::sampler::Sampler;
/// use vulkano::image::view::ImageView;
///
/// use gtether::render::descriptor_set::VKDescriptorSource;
/// #
/// # fn hash_image_view(image_view: Arc<ImageView>) -> u64 { /* not an actual hashing function */ 0 }
///
/// #[derive(Debug)]
/// struct ImageViewDescriptor {
///     image_view: Arc<ImageView>,
///     sampler: Arc<Sampler>,
/// }
///
/// impl VKDescriptorSource for ImageViewDescriptor {
///     fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
///         (
///             WriteDescriptorSet::image_view_sampler(
///                 binding,
///                 self.image_view.clone(),
///                 self.sampler.clone(),
///             ),
///             // This is some hashing method that produces a high quality hash
///             hash_image_view(self.image_view.clone()),
///         )
///     }
///
///     fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
///         // Nothing to actually update in this case, but if the underlying image view changed
///         // somehow, then we need an updated hash
///         hash_image_view(self.image_view.clone())
///     }
/// }
/// ```
///
/// Sample implementation for a dynamic buffer:
/// ```
/// use std::sync::Mutex;
/// use vulkano::buffer::Subbuffer;
/// use vulkano::descriptor_set::{DescriptorBufferInfo, WriteDescriptorSet};
/// use vulkano::DeviceSize;
///
/// use gtether::render::descriptor_set::{DescriptorOffsetIter, VKDescriptorSource};
///
/// #[derive(Debug)]
/// struct DynamicBufferDescriptor<const N: usize> {
///     // Our data for this example is u32's
///     data: [u32; N],
///     // One buffer (and hence write descriptor) per frame
///     dynamic_buffers: Mutex<Vec<Subbuffer<[u8]>>>,
/// }
///
/// impl<const N: usize> VKDescriptorSource for DynamicBufferDescriptor<N> {
///     fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64) {
///         let buffers = self.dynamic_buffers.lock().unwrap();
///         let buffer = buffers.get(frame_idx).unwrap();
///         (
///             WriteDescriptorSet::buffer_with_range(
///                 binding,
///                 DescriptorBufferInfo {
///                     buffer: buffer.clone(),
///                     range: 0..size_of::<u32>() as DeviceSize,
///                 },
///             ),
///             // In this case, these buffers are never replaced, so a new write descriptor is
///             // never needed. If the write descriptor ever changed, we would need to generate
///             // actual hashes.
///             0,
///         )
///     }
///
///     fn update_descriptor_source(&self, frame_idx: usize) -> u64 {
///         let mut buffers = self.dynamic_buffers.lock().unwrap();
///         let buffer = buffers.get_mut(frame_idx).unwrap();
///
///         let mut write_guard = buffer.write().unwrap();
///         for (idx, value) in self.data.iter().enumerate() {
///             write_guard[(size_of::<u32>()*idx)..(size_of::<u32>()*(idx+1))]
///                 .copy_from_slice(&value.to_ne_bytes());
///         }
///
///         // In this case, these buffers are never replaced, so a new write descriptor is
///         // never needed. If the write descriptor ever changed, we would need to generate
///         // actual hashes.
///         0
///     }
///
///     // Because we're using a dynamic buffer, we need to implement this to generate the right
///     // offsets
///     fn descriptor_offsets(&self, frame_idx: usize) -> Option<DescriptorOffsetIter> {
///         let buffers = self.dynamic_buffers.lock().unwrap();
///         let buffer = buffers.get(frame_idx).unwrap();
///         Some(DescriptorOffsetIter::new(size_of::<u32>() as u32, (N * size_of::<u32>()) as u32))
///     }
/// }
/// ```
///
/// [dswd]: VKDescriptorSource::write_descriptor
/// [dsuds]: VKDescriptorSource::update_descriptor_source
/// [dsdo]: VKDescriptorSource::descriptor_offsets
///
/// # Hashing
///
/// Descriptor sources are also responsible for hashing write descriptors, which lets an
/// [EngineDescriptorSet] know when it needs to be rebuilt. Hashes are `u64`, and are specific to
/// each descriptor source; one descriptor source does not need to use the same hashing scheme as
/// another. The only requirement is that the hashes are unique enough to guarantee a different hash
/// when the write descriptor changes.
///
/// For descriptor sources where the write descriptor never changes (but the underlying data might,
/// such as in a buffer), it is okay to use a constant hash value, or even just `0`.
pub trait VKDescriptorSource: Debug + Send + Sync {
    /// Generate a write descriptor for a specific `frame_idx` and `binding`.
    ///
    /// Also yield a [hash](VKDescriptorSource#hashing) for the generated write descriptor.
    fn write_descriptor(&self, frame_idx: usize, binding: u32) -> (WriteDescriptorSet, u64);

    /// Update this descriptor source for a specific `frame_idx`.
    ///
    /// Should yield a [hash](VKDescriptorSource#hashing) for this source's write descriptor.
    fn update_descriptor_source(&self, frame_idx: usize) -> u64;

    /// Get an iterator over any possible offsets for this write descriptor.
    ///
    /// Used with dynamic buffers, where a single write descriptor can represent many different
    /// values in said buffer. If there are no relevant offsets (the write descriptor only
    /// represents one value), this should yield `None`.
    #[allow(unused_variables)]
    fn descriptor_offsets(&self, frame_idx: usize) -> Option<DescriptorOffsetIter> { None }
}

/// Iterator for descriptor sets with offsets applied.
///
/// If a descriptor set has multiple descriptors with dynamic offsets, they will be zipped together,
/// and the iterator will stop when it finds the end of one set of offsets. For example, if a
/// descriptor set has descriptor A with 2 dynamic offsets, and descriptor B with 3 dynamic offsets,
/// this iterator will yield two modified descriptor sets with offsets, ignoring the third offset
/// from descriptor B.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use vulkano::descriptor_set::PersistentDescriptorSet;
/// use gtether::render::descriptor_set::{DescriptorOffsetIter, DescriptorSetWithOffsetsIter};
/// #
/// # let base_descriptor_set: Arc<PersistentDescriptorSet> = return;
///
/// let offsets_a = DescriptorOffsetIter::new(16, 32);
/// let offsets_b = DescriptorOffsetIter::new(16, 48);
///
/// let descriptor_sets = DescriptorSetWithOffsetsIter::new(
///     base_descriptor_set,
///     [offsets_a, offsets_b],
/// ).collect::<Vec<_>>();
/// assert_eq!(descriptor_sets.len(), 2);
/// ```
pub struct DescriptorSetWithOffsetsIter {
    base_descriptor_set: Arc<PersistentDescriptorSet>,
    // 4 is the size that Vulkano uses for its descriptor offset counts
    offset_iters: SmallVec<[DescriptorOffsetIter; 4]>,
}

impl DescriptorSetWithOffsetsIter {
    /// Create a new iterator based off of a given descriptor set and offsets.
    #[inline]
    pub fn new(
        base_descriptor_set: Arc<PersistentDescriptorSet>,
        offset_iters: impl IntoIterator<Item=DescriptorOffsetIter>,
    ) -> Self {
        Self {
            base_descriptor_set,
            offset_iters: offset_iters.into_iter().collect::<SmallVec<[_; 4]>>(),
        }
    }
}

impl Iterator for DescriptorSetWithOffsetsIter {
    type Item = DescriptorSetWithOffsets;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.offset_iters.is_empty() {
            let offsets = self.offset_iters.iter_mut()
                .map(|iter| iter.next())
                .collect::<Option<SmallVec<[u32; 4]>>>()?;
            Some(self.base_descriptor_set.clone().offsets(offsets))
        } else {
            // If there are no offsets at all, then this should never yield anything
            None
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = self.offset_iters.iter()
            .map(|iter| iter.len())
            .min().unwrap();
        (size, Some(size))
    }
}

impl ExactSizeIterator for DescriptorSetWithOffsetsIter {}

#[derive(Debug, Clone)]
struct Descriptor {
    binding: u32,
    source: Arc<dyn VKDescriptorSource>,
}

struct EngineDescriptorSetFrame {
    descriptor_set: Arc<PersistentDescriptorSet>,
    descriptor_hashes: SmallVec<[u64; 8]>,
}

impl Debug for EngineDescriptorSetFrame {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineDescriptorSetFrame")
            .field("descriptor_hashes", &self.descriptor_hashes)
            .finish_non_exhaustive()
    }
}

/// Helper struct for maintaining a Vulkan descriptor set.
///
/// This struct maintains a descriptor set per framebuffer. Descriptor sets are created from
/// [descriptor sources](VKDescriptorSource) given to this struct when it is created. Every time
/// this struct is [queried](EngineDescriptorSet::descriptor_set) for a descriptor set, it
/// automatically checks [hashes](VKDescriptorSource#hashing) for descriptors, and will first
/// recreate the descriptor set from its sources as needed.
///
/// See [EngineDescriptorSetBuilder] for examples of how to create an EngineDescriptorSet.
pub struct EngineDescriptorSet {
    device: Arc<Device>,
    layout: Arc<DescriptorSetLayout>,
    descriptors: SmallVec<[Descriptor; 8]>,
    frames: Mutex<SmallVec<[EngineDescriptorSetFrame; 3]>>,
}

impl Debug for EngineDescriptorSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineDescriptorSet")
            .field("layout", &self.layout)
            .field("descriptors", &self.descriptors)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl EngineDescriptorSet {
    /// Create an [EngineDescriptorSetBuilder].
    ///
    /// This is the recommended way to create EngineDescriptorSets.
    #[inline]
    pub fn builder(target: &Arc<dyn RenderTarget>) -> EngineDescriptorSetBuilder {
        EngineDescriptorSetBuilder::new(target.clone())
    }

    fn create_frame(
        device: &Arc<Device>,
        layout: Arc<DescriptorSetLayout>,
        frame_idx: usize,
        descriptors: &SmallVec<[Descriptor; 8]>,
    ) -> Result<EngineDescriptorSetFrame, Validated<VulkanError>> {
        let (write_descriptors, descriptor_hashes): (SmallVec<[_; 8]>, SmallVec<[_; 8]>) = descriptors.iter()
            .map(|descriptor| descriptor.source.write_descriptor(
                frame_idx,
                descriptor.binding,
            ))
            .unzip();
        let descriptor_set = PersistentDescriptorSet::new(
            device.descriptor_set_allocator(),
            layout,
            write_descriptors,
            [],
        )?;
        Ok(EngineDescriptorSetFrame {
            descriptor_set,
            descriptor_hashes,
        })
    }

    /// The descriptor set layout used to create descriptor sets.
    #[inline]
    pub fn layout(&self) -> &Arc<DescriptorSetLayout> { &self.layout }

    fn descriptor_set_impl(&self, frame_idx: usize)
            -> Result<Arc<PersistentDescriptorSet>, Validated<VulkanError>> {
        let mut frames = self.frames.lock();
        let frame = frames.get(frame_idx)
            .expect("Frame count should match target's frame count");

        let mut stale = false;
        for (idx, descriptor) in self.descriptors.iter().enumerate() {
            let old_hash = frame.descriptor_hashes[idx];
            let new_hash = descriptor.source.update_descriptor_source(frame_idx);
            if new_hash != old_hash {
                stale = true;
            }
        }

        if stale {
            frames[frame_idx] = Self::create_frame(
                &self.device,
                self.layout.clone(),
                frame_idx,
                &self.descriptors,
            )?;
            Ok(frames[frame_idx].descriptor_set.clone())
        } else {
            Ok(frame.descriptor_set.clone())
        }
    }

    /// Get a Vulkan descriptor set for a given `frame_idx`.
    ///
    /// All descriptor sets created by this struct are returned with offsets, however this method
    /// assumes that any applicable offsets will be `0`, and is safe to use with descriptor sets
    /// that don't need offsets. This allows a descriptor set to be bound and used for all of its
    /// descriptors that _don't_ use offsets, while safely ignoring the ones that do.
    ///
    /// If you actually need to work with descriptor offsets, see
    /// [Self::descriptor_set_with_offsets()] instead.
    ///
    /// # Examples
    /// ```
    /// use std::sync::Arc;
    /// use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
    /// use vulkano::pipeline::{GraphicsPipeline, Pipeline, PipelineBindPoint};
    ///
    /// use gtether::render::descriptor_set::EngineDescriptorSet;
    /// use gtether::render::render_pass::EngineRenderHandler;
    /// use gtether::render::swapchain::Framebuffer;
    ///
    /// struct MyRenderHandler {
    ///     graphics: Arc<GraphicsPipeline>,
    ///     descriptor_set: EngineDescriptorSet,
    /// }
    ///
    /// impl EngineRenderHandler for MyRenderHandler {
    ///     fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
    ///         builder
    ///             .bind_descriptor_sets(
    ///                 PipelineBindPoint::Graphics,
    ///                 self.graphics.layout().clone(),
    ///                 0,
    ///                 self.descriptor_set.descriptor_set(frame.index()).unwrap(),
    ///             ).unwrap()
    ///             /* other bind and draw calls */;
    ///     }
    /// }
    /// ```
    pub fn descriptor_set(&self, frame_idx: usize)
            -> Result<DescriptorSetWithOffsets, Validated<VulkanError>> {
        let base_descriptor_set = self.descriptor_set_impl(frame_idx)?;
        let offsets = self.descriptors.iter()
            .map(|descriptor| descriptor.source.descriptor_offsets(frame_idx))
            .flatten()
            .map(|_| 0);
        Ok(base_descriptor_set.offsets(offsets))
    }

    /// Iterate over Vulkan descriptor sets with offsets for all applicable values.
    ///
    /// This method will query [descriptor sources](VKDescriptorSource) for their offsets, and
    /// produce an iterator over all valid descriptor sets with offsets applied.
    ///
    /// If no descriptor sources use offsets, this can produce an empty iterator.
    ///
    /// # Examples
    /// ```
    /// use std::sync::Arc;
    /// use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
    /// use vulkano::pipeline::{GraphicsPipeline, Pipeline, PipelineBindPoint};
    ///
    /// use gtether::render::descriptor_set::EngineDescriptorSet;
    /// use gtether::render::render_pass::EngineRenderHandler;
    /// use gtether::render::swapchain::Framebuffer;
    ///
    /// struct MyRenderHandler {
    ///     graphics: Arc<GraphicsPipeline>,
    ///     descriptor_set: EngineDescriptorSet,
    /// }
    ///
    /// impl EngineRenderHandler for MyRenderHandler {
    ///     fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
    ///         let descriptor_sets = self.descriptor_set
    ///             .descriptor_set_with_offsets(frame.index()).unwrap();
    ///         for descriptor_set in descriptor_sets {
    ///             builder
    ///                 .bind_descriptor_sets(
    ///                     PipelineBindPoint::Graphics,
    ///                     self.graphics.layout().clone(),
    ///                     0,
    ///                     descriptor_set,
    ///                 ).unwrap()
    ///                 /* other bind and draw calls */;
    ///         }
    ///     }
    /// }
    /// ```
    pub fn descriptor_set_with_offsets(&self, frame_idx: usize)
            -> Result<DescriptorSetWithOffsetsIter, Validated<VulkanError>> {
        let base_descriptor_set = self.descriptor_set_impl(frame_idx)?;
        let offset_iters = self.descriptors.iter()
            .map(|descriptor| descriptor.source.descriptor_offsets(frame_idx))
            .flatten();
        Ok(DescriptorSetWithOffsetsIter::new(base_descriptor_set, offset_iters))
    }
}

/// Builder pattern for [EngineDescriptorSet].
///
/// # Examples
/// ```
/// use std::sync::Arc;
/// use vulkano::descriptor_set::layout::DescriptorSetLayout;
/// use vulkano::{Validated, VulkanError};
///
/// use gtether::render::descriptor_set::{EngineDescriptorSet, VKDescriptorSource};
/// use gtether::render::RenderTarget;
/// #
/// # fn get_descriptor_source_a() -> Arc<dyn VKDescriptorSource> { unreachable!() }
/// # fn get_descriptor_source_b() -> Arc<dyn VKDescriptorSource> { unreachable!() }
///
/// fn create_descriptor_set(
///     target: &Arc<dyn RenderTarget>,
///     layout: Arc<DescriptorSetLayout>,
/// ) -> Result<EngineDescriptorSet, Validated<VulkanError>> {
///     let descriptor_source_a: Arc<dyn VKDescriptorSource> = get_descriptor_source_a();
///     let descriptor_source_b: Arc<dyn VKDescriptorSource> = get_descriptor_source_b();
///
///     EngineDescriptorSet::builder(target)
///         .layout(layout)
///         .descriptor_source(0, descriptor_source_a)
///         .descriptor_source(1, descriptor_source_b)
///         .build()
/// }
/// ```
pub struct EngineDescriptorSetBuilder {
    target: Arc<dyn RenderTarget>,
    layout: Option<Arc<DescriptorSetLayout>>,
    descriptors: SmallVec<[Descriptor; 8]>,
}

impl EngineDescriptorSetBuilder {
    /// Create a new builder.
    ///
    /// It is recommended to use [EngineDescriptorSet::builder()] instead.
    #[inline]
    pub fn new(target: Arc<dyn RenderTarget>) -> Self {
        Self {
            target,
            layout: None,
            descriptors: SmallVec::new(),
        }
    }

    /// Set the descriptor set layout.
    ///
    /// This is a required parameter, and must be set before [building](Self::build).
    #[inline]
    pub fn layout(&mut self, layout: Arc<DescriptorSetLayout>) -> &mut Self {
        self.layout = Some(layout);
        self
    }

    /// Add a descriptor source with a given `binding`.
    #[inline]
    pub fn descriptor_source(&mut self, binding: u32, source: Arc<dyn VKDescriptorSource>) -> &mut Self {
        self.descriptors.push(Descriptor {
            binding,
            source,
        });
        self
    }

    /// Build the [EngineDescriptorSet].
    ///
    /// # Errors
    ///
    /// This can error when the underlying Vulkan descriptor set/s fail to create.
    pub fn build(&self) -> Result<EngineDescriptorSet, Validated<VulkanError>> {
        let layout = self.layout.clone()
            .expect(".layout() must be set");
        let descriptors = self.descriptors.clone();

        let frames = (0..self.target.framebuffer_count() as usize).into_iter()
            .map(|frame_idx| {
                EngineDescriptorSet::create_frame(self.target.device(), layout.clone(), frame_idx, &descriptors)
            })
            .collect::<Result<SmallVec<_>, _>>()?;

        Ok(EngineDescriptorSet {
            device: self.target.device().clone(),
            layout,
            descriptors,
            frames: Mutex::new(frames),
        })
    }
}