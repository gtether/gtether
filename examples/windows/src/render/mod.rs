use std::cell::OnceCell;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, OnceLock};

use glm::{identity, TMat4, TVec3};
use parking_lot::RwLock;
use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::descriptor_set::{PersistentDescriptorSet, WriteDescriptorSet};
use vulkano::image::view::ImageView;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::{GraphicsPipeline, Pipeline};
use vulkano::pipeline::graphics::vertex_input::Vertex;

use gtether::render::render_pass::{AttachmentBuffer, AttachmentMap};
use gtether::render::RenderTarget;

pub mod cube;
pub mod ambient;
pub mod directional;

struct UniformData<T: Clone + BufferContents> {
    value: T,
    descriptor_set: OnceLock<Arc<PersistentDescriptorSet>>,
}

impl<T: Clone + BufferContents> Debug for UniformData<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UniformData")
            .finish()
    }
}

impl<T: Clone + BufferContents> UniformData<T> {
    #[inline]
    fn new(value: T) -> Self {
        Self {
            value,
            descriptor_set: OnceLock::new(),
        }
    }

    fn get_or_init_descriptor_set(&self, refs: &UniformRefs, set_index: u32) -> &Arc<PersistentDescriptorSet> {
        self.descriptor_set.get_or_init(|| {
            let graphics = refs.graphics.as_ref()
                .expect(".recreate() must be called before .descriptor_set() can be accessed");

            let buffer = Buffer::from_data(
                refs.target.device().memory_allocator().clone(),
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
                refs.target.device().descriptor_set_allocator(),
                graphics.layout().set_layouts().get(set_index as usize).unwrap().clone(),
                [
                    WriteDescriptorSet::buffer(0, buffer),
                ],
                [],
            ).unwrap()
        })
    }
}

#[derive(Debug)]
struct UniformRefs {
    target: Arc<dyn RenderTarget>,
    graphics: Option<Arc<GraphicsPipeline>>,
}

#[derive(Debug)]
pub struct Uniform<T: Clone + BufferContents> {
    inner: RwLock<UniformData<T>>,
    refs: RwLock<UniformRefs>,
    set_index: u32,
}

impl<T: Clone + BufferContents> Uniform<T> {
    #[inline]
    fn new(
        value: T,
        target: &Arc<dyn RenderTarget>,
        set_index: u32,
    ) -> Self {
        Uniform {
            inner: RwLock::new(UniformData::new(value)),
            refs: RwLock::new(UniformRefs {
                target: target.clone(),
                graphics: None,
            }),
            set_index,
        }
    }

    fn recreate(&self, target: &Arc<dyn RenderTarget>, graphics: &Arc<GraphicsPipeline>) {
        self.inner.write().descriptor_set.take();
        *self.refs.write() = UniformRefs {
            target: target.clone(),
            graphics: Some(graphics.clone()),
        }
    }

    #[inline]
    pub fn get(&self) -> T { self.inner.read().value.clone() }

    #[inline]
    pub fn set(&self, value: T) { *self.inner.write() = UniformData::new(value); }

    fn descriptor_set(&self) -> Arc<PersistentDescriptorSet> {
        self.inner.read().get_or_init_descriptor_set(
            &self.refs.read(),
            self.set_index,
        ).clone()
    }
}

#[derive(Debug)]
pub struct UniformSet<T: Clone + BufferContents> {
    inner: RwLock<Vec<UniformData<T>>>,
    refs: RwLock<UniformRefs>,
    set_index: u32,
}

impl<T: Clone + BufferContents> UniformSet<T> {
    #[inline]
    fn new(
        values: Vec<T>,
        target: &Arc<dyn RenderTarget>,
        set_index: u32,
    ) -> Self {
        let uniforms = values.into_iter().map(|value| {
            UniformData::new(value)
        }).collect();

        Self {
            inner: RwLock::new(uniforms),
            refs: RwLock::new(UniformRefs {
                target: target.clone(),
                graphics: None,
            }),
            set_index,
        }
    }

    fn recreate(&self, target: &Arc<dyn RenderTarget>, graphics: &Arc<GraphicsPipeline>) {
        for uniform in self.inner.write().iter_mut() {
            uniform.descriptor_set.take();
        }
        *self.refs.write() = UniformRefs {
            target: target.clone(),
            graphics: Some(graphics.clone()),
        }
    }

    #[inline]
    pub fn get(&self) -> Vec<T> {
        self.inner.read().iter().map(|uniform| {
            uniform.value.clone()
        }).collect()
    }

    #[inline]
    pub fn set(&self, values: Vec<T>) {
        *self.inner.write() = values.into_iter().map(|value| {
            UniformData::new(value)
        }).collect();
    }

    fn descriptor_sets(&self) -> Vec<Arc<PersistentDescriptorSet>> {
        self.inner.read().iter().map(|uniform| {
            uniform.get_or_init_descriptor_set(
                &self.refs.read(),
                self.set_index,
            ).clone()
        }).collect()
    }
}

struct AttachmentSet {
    target: Arc<dyn RenderTarget>,
    graphics: Option<Arc<GraphicsPipeline>>,
    set_index: u32,
    input_buffers: Vec<Vec<Arc<ImageView>>>,
    descriptor_sets: OnceCell<Vec<Arc<PersistentDescriptorSet>>>,
}

impl AttachmentSet {
    fn new(target: &Arc<dyn RenderTarget>, set_index: u32) -> Self {
        Self {
            target: target.clone(),
            graphics: None,
            set_index,
            input_buffers: vec![],
            descriptor_sets: OnceCell::new(),
        }
    }

    fn recreate(
        &mut self,
        target: &Arc<dyn RenderTarget>,
        graphics: &Arc<GraphicsPipeline>,
        attachments: &Arc<dyn AttachmentMap>,
        input_attachment_names: &[String],
    ) {
        self.target = target.clone();
        self.graphics = Some(graphics.clone());

        self.input_buffers = (0..target.framebuffer_count()).into_iter().map(|frame_idx| {
            input_attachment_names.iter().map(|color| {
                match attachments.get_attachment(color.clone(), frame_idx as usize).unwrap() {
                    AttachmentBuffer::Transient(image_view) => image_view.clone(),
                    _ => panic!("'{color}' attachment was not transient?"),
                }
            }).collect()
        }).collect();

        self.descriptor_sets.take();
    }

    fn descriptor_set(&self, frame_idx: usize) -> Option<&Arc<PersistentDescriptorSet>> {
        self.descriptor_sets.get_or_init(|| {
            let graphics = self.graphics.as_ref()
                .expect(".recreate() must be called before .descriptor_set() can be accessed");

            let set_layout = graphics.layout().set_layouts().get(self.set_index as usize).unwrap();
            (0..self.target.framebuffer_count()).into_iter().map(|frame_idx| {
                PersistentDescriptorSet::new(
                    self.target.device().descriptor_set_allocator(),
                    set_layout.clone(),
                    self.input_buffers[frame_idx as usize].iter().enumerate().map(|(idx, buffer)| {
                        WriteDescriptorSet::image_view(idx as u32, buffer.clone())
                    }).collect::<Vec<_>>(),
                    [],
                ).unwrap()
            }).collect()
        }).get(frame_idx)
    }
}

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