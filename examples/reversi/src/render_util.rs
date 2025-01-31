use bytemuck::NoUninit;
use gtether::render::descriptor_set::EngineDescriptorSet;
use gtether::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource, ViewportType};
use gtether::render::render_pass::EngineRenderHandler;
use gtether::render::uniform::{Uniform, UniformSet, UniformValue};
use gtether::render::{FlatVertex, RenderTarget, Renderer, VulkanoError};
use std::sync::Arc;
use parry3d::na::{Isometry3, Perspective3, Point3};
use vulkano::buffer::{BufferContents, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::descriptor_set::layout::DescriptorType;
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, BlendFactor, BlendOp, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;
use vulkano::Validated;
use gtether::render::frame::FrameManagerExt;

#[derive(BufferContents)]
#[repr(C)]
pub struct MN {
    pub model: glm::TMat4<f32>,
    pub normals: glm::TMat4<f32>,
}

#[derive(Debug)]
pub struct ModelTransform {
    pub transform: Isometry3<f32>,
}

impl ModelTransform {
    pub fn new() -> Self {
        Self {
            transform: Isometry3::identity(),
        }
    }
}

impl UniformValue<MN> for ModelTransform {
    #[inline]
    fn buffer_contents(&self) -> MN {
        MN {
            model: self.transform.to_homogeneous(),
            normals: glm::inverse_transpose(self.transform.to_homogeneous()),
        }
    }
}

#[derive(BufferContents)]
#[repr(C)]
pub struct VP {
    pub matrix: glm::TMat4<f32>,
}

#[derive(Debug)]
pub struct Camera {
    pub extent: glm::TVec2<f32>,
    pub view: Isometry3<f32>,
    pub projection: Perspective3<f32>,
}

impl Camera {
    pub fn new(
        render_target: &Arc<dyn RenderTarget>,
        eye: &Point3<f32>,
        target: &Point3<f32>,
        up: &glm::TVec3<f32>,
    ) -> Self {
        let extent = render_target.extent().cast::<f32>();
        Self {
            extent,
            view: Isometry3::look_at_rh(eye, target, up),
            projection: Perspective3::new(
                extent.x / extent.y,
                glm::half_pi(),
                0.01, 100.0,
            ),
        }
    }

    pub fn update(&mut self, render_target: &Arc<dyn RenderTarget>) {
        self.extent = render_target.extent().cast::<f32>();
        self.projection.set_aspect(self.extent.x / self.extent.y);
    }
}

impl UniformValue<VP> for Camera {
    #[inline]
    fn buffer_contents(&self) -> VP {
        VP {
            matrix: self.projection.as_matrix() * self.view.to_homogeneous(),
        }
    }
}

/*impl VP {
    #[allow(unused)]
    #[inline]
    pub fn look_at(eye: &glm::TVec3<f32>, center: &glm::TVec3<f32>, up: &glm::TVec3<f32>) -> Self {
        Self {
            view: glm::look_at(eye, center, up),
            projection: glm::identity(),
        }
    }
}*/

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct AmbientLight {
    pub color: glm::TVec3<f32>,
    pub intensity: f32,
}

impl AmbientLight {
    #[inline]
    pub fn new(color: glm::TVec3<f32>, intensity: f32) -> Self {
        Self {
            color,
            intensity,
        }
    }
}

impl Default for AmbientLight {
    #[inline]
    fn default() -> Self {
        Self::new(glm::vec3(1.0, 1.0, 1.0), 0.1)
    }
}

#[derive(BufferContents, NoUninit, Default, Debug, Clone, Copy)]
#[repr(C)]
pub struct PointLight {
    pub position: glm::TVec4<f32>,
    pub color: glm::TVec3<f32>,
}

mod ambient_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "assets/shaders/ambient.vert",
    }
}

mod ambient_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "assets/shaders/ambient.frag",
    }
}

mod directional_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "assets/shaders/directional.vert",
    }
}

mod directional_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "assets/shaders/directional.frag",
    }
}

pub struct DeferredLightingRenderer {
    pipeline_layout: Arc<PipelineLayout>,
    ambient_graphics: Arc<EngineGraphicsPipeline>,
    directional_graphics: Arc<EngineGraphicsPipeline>,
    screen_buffer: Subbuffer<[FlatVertex]>,
    descriptor_set: EngineDescriptorSet,
}

impl DeferredLightingRenderer {
    fn new(
        renderer: &Arc<Renderer>,
        subpass: &Subpass,
        ambient_light: Arc<Uniform<AmbientLight>>,
        point_lights: Arc<UniformSet<PointLight, 8>>,
    ) -> Self {
        let (ambient_vertex_input_state, ambient_stages) = {
            let vert = ambient_vert::load(renderer.device().vk_device().clone())
                .expect("Failed to create vertex shader module")
                .entry_point("main").unwrap();

            let frag = ambient_frag::load(renderer.device().vk_device().clone())
                .expect("Failed to create fragment shader module")
                .entry_point("main").unwrap();

            let vertex_input_state = Some(FlatVertex::per_vertex()
                .definition(&vert.info().input_interface)
                .unwrap());

            let stages = [
                PipelineShaderStageCreateInfo::new(vert),
                PipelineShaderStageCreateInfo::new(frag),
            ];

            (vertex_input_state, stages)
        };

        let (directional_vertex_input_state, directional_stages) = {
            let vert = directional_vert::load(renderer.device().vk_device().clone())
                .expect("Failed to create vertex shader module")
                .entry_point("main").unwrap();

            let frag = directional_frag::load(renderer.device().vk_device().clone())
                .expect("Failed to create fragment shader module")
                .entry_point("main").unwrap();

            let vertex_input_state = Some(FlatVertex::per_vertex()
                .definition(&vert.info().input_interface)
                .unwrap());

            let stages = [
                PipelineShaderStageCreateInfo::new(vert),
                PipelineShaderStageCreateInfo::new(frag),
            ];

            (vertex_input_state, stages)
        };

        let pipeline_layout = {
            let mut layout_create_info = PipelineDescriptorSetLayoutCreateInfo::from_stages(
                ambient_stages.iter().chain(&directional_stages)
            );
            // Binding #3 using a dynamic uniform buffer for multiple point lights
            layout_create_info.set_layouts[0].bindings
                .get_mut(&3).unwrap()
                .descriptor_type = DescriptorType::UniformBufferDynamic;
            PipelineLayout::new(
                renderer.device().vk_device().clone(),
                layout_create_info
                    .into_pipeline_layout_create_info(renderer.device().vk_device().clone()).unwrap(),
            ).unwrap()
        };
        let descriptor_layout = pipeline_layout.set_layouts().get(0).unwrap().clone();

        let base_create_info = GraphicsPipelineCreateInfo {
            subpass: Some(subpass.clone().into()),
            color_blend_state: Some(ColorBlendState::with_attachment_states(
                subpass.num_color_attachments(),
                ColorBlendAttachmentState {
                    blend: Some(AttachmentBlend {
                        src_color_blend_factor: BlendFactor::One,
                        dst_color_blend_factor: BlendFactor::One,
                        color_blend_op: BlendOp::Add,
                        src_alpha_blend_factor: BlendFactor::One,
                        dst_alpha_blend_factor: BlendFactor::One,
                        alpha_blend_op: BlendOp::Min,
                    }),
                    ..Default::default()
                },
            )),
            input_assembly_state: Some(InputAssemblyState::default()),
            rasterization_state: Some(RasterizationState {
                cull_mode: CullMode::Back,
                ..Default::default()
            }),
            multisample_state: Some(MultisampleState::default()),
            ..GraphicsPipelineCreateInfo::layout(pipeline_layout.clone())
        };

        let ambient_graphics = EngineGraphicsPipeline::new(
            renderer,
            GraphicsPipelineCreateInfo {
                stages: ambient_stages.into_iter().collect(),
                vertex_input_state: ambient_vertex_input_state,
                ..base_create_info.clone()
            },
            ViewportType::TopLeft,
        );
        let directional_graphics = EngineGraphicsPipeline::new(
            renderer,
            GraphicsPipelineCreateInfo {
                stages: directional_stages.into_iter().collect(),
                vertex_input_state: directional_vertex_input_state,
                ..base_create_info
            },
            ViewportType::TopLeft,
        );

        let screen_buffer = FlatVertex::screen_buffer(
            renderer.device().memory_allocator().clone()
        );

        let descriptor_set = EngineDescriptorSet::builder(renderer.clone())
            .layout(descriptor_layout)
            .descriptor_source(0, renderer.frame_manager().attachment_descriptor("color"))
            .descriptor_source(1, renderer.frame_manager().attachment_descriptor("normals"))
            .descriptor_source(2, ambient_light)
            .descriptor_source(3, point_lights)
            .build();

        Self {
            pipeline_layout,
            ambient_graphics,
            directional_graphics,
            screen_buffer,
            descriptor_set,
        }
    }
}

impl EngineRenderHandler for DeferredLightingRenderer {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        let ambient_graphics = self.ambient_graphics.vk_graphics();
        let directional_graphics = self.directional_graphics.vk_graphics();

        builder.bind_vertex_buffers(0, self.screen_buffer.clone())?;

        builder
            .bind_pipeline_graphics(ambient_graphics.clone())?
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.pipeline_layout.clone(),
                0,
                self.descriptor_set.descriptor_set().map_err(VulkanoError::from_validated)?,
            )?
            .draw(self.screen_buffer.len() as u32, 1, 0, 0)?;

        builder.bind_pipeline_graphics(directional_graphics.clone())?;
        let descriptor_sets = self.descriptor_set
            .descriptor_set_with_offsets().map_err(VulkanoError::from_validated)?;
        for descriptor_set in descriptor_sets {
            builder
                .bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    self.pipeline_layout.clone(),
                    0,
                    descriptor_set,
                )?
                .draw(self.screen_buffer.len() as u32, 1, 0, 0)?;
        }

        Ok(())
    }
}

pub struct DeferredLightingRendererBootstrap {
    ambient_light: Arc<Uniform<AmbientLight>>,
    point_lights: Arc<UniformSet<PointLight, 8>>,
}

impl DeferredLightingRendererBootstrap {
    #[inline]
    pub fn new(renderer: &Arc<Renderer>, lights: impl IntoIterator<Item=PointLight>) -> Arc<Self> {
        Arc::new(Self {
            ambient_light: Arc::new(Uniform::new(
                renderer.device().clone(),
                renderer.frame_manager(),
                AmbientLight::default(),
            ).unwrap()),
            point_lights: Arc::new(UniformSet::new(
                renderer.device().clone(),
                renderer.frame_manager(),
                lights,
            ).unwrap()),
        })
    }

    pub fn bootstrap(self: &Arc<Self>)
            -> impl FnOnce(&Arc<Renderer>, &Subpass) -> DeferredLightingRenderer {
        let self_clone = self.clone();
        move |renderer, subpass| {
            DeferredLightingRenderer::new(
                renderer,
                subpass,
                self_clone.ambient_light.clone(),
                self_clone.point_lights.clone(),
            )
        }
    }

    #[inline]
    pub fn ambient_light(&self) -> &Arc<Uniform<AmbientLight>> { &self.ambient_light }

    #[inline]
    pub fn point_lights(&self) -> &Arc<UniformSet<PointLight, 8>> { &self.point_lights }
}