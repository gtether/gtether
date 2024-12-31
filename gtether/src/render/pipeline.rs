//! Utilities for Vulkan pipelines.
//!
//! This module contains helper utilities for working with Vulkano pipelines. The primary utility
//! in this module is [EngineGraphicsPipeline], which provides a mechanism for automatically
//! recreating the Vulkano [GraphicsPipeline][vkgp] when required.
//!
//! These utilities are entirely optional, and you are free to roll your own logic for Vulkano
//! pipelines. To allow for better integration with other engine mechanisms, this module also
//! provides the [VKGraphicsPipelineSource] trait, which you can implement in order to use other
//! engine helpers with your own pipeline logic.
//!
//! [vkgp]: vulkano::pipeline::GraphicsPipeline

use parking_lot::Mutex;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use vulkano::pipeline::graphics::viewport::{Viewport, ViewportState};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::GraphicsPipeline;
use vulkano::render_pass::Subpass;

use crate::event::{Event, EventHandler};
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};

/// Helper trait for retrieving a Vulkano [GraphicsPipeline].
pub trait VKGraphicsPipelineSource: Send + Sync {
    /// Get a Vulkano [GraphicsPipeline].
    ///
    /// This pipeline does not need to be constant, and indeed it is expected that new pipelines
    /// may be created as needed when Vulkan resources such as framebuffers get stale.
    fn vk_graphics(&self) -> Arc<GraphicsPipeline>;
}

pub type GenGraphicsPipelineCreateInfoFn = Box<
    dyn (Fn(&Subpass) -> GraphicsPipelineCreateInfo)
    + Send + Sync + 'static
>;

#[derive(Debug, Default)]
struct EngineGraphicsPipelineState {
    subpass: Option<Subpass>,
    pipeline: Option<Arc<GraphicsPipeline>>
}

/// Helper struct for maintaining a Vulkano [GraphicsPipeline].
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// In order to recreate the underlying pipeline, this struct needs to know what
/// [GraphicsPipelineCreateInfo] to use - some of which may be determined dynamically. To facilitate
/// this, this struct takes in a callback that can generate [GraphicsPipelineCreateInfo] from a
/// [Subpass] reference.
///
/// # Examples
/// ```
/// # use vulkano::pipeline::graphics::vertex_input::VertexInputState;
/// use vulkano::pipeline::{PipelineLayout, PipelineShaderStageCreateInfo};
/// use vulkano::pipeline::graphics::color_blend::{ColorBlendAttachmentState, ColorBlendState};
/// use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
/// use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
/// # use vulkano::shader::EntryPoint;
/// use gtether::render::pipeline::EngineGraphicsPipeline;
///
/// # use gtether::render::RendererHandle;
///
/// # fn wrapper(renderer: &RendererHandle, vertex_shader: EntryPoint, fragment_shader: EntryPoint, vertex_input_state: Option<VertexInputState>) {
/// // Assuming some existing vertex/fragment shaders + vertex input state
/// let stages = [
///     PipelineShaderStageCreateInfo::new(vertex_shader),
///     PipelineShaderStageCreateInfo::new(fragment_shader),
/// ];
///
/// let layout = PipelineLayout::new(
///     renderer.target().device().vk_device().clone(),
///     PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
///         .into_pipeline_layout_create_info(renderer.target().device().vk_device().clone())
///         .unwrap(),
/// ).unwrap();
///
/// let base_create_info = GraphicsPipelineCreateInfo {
///     stages: stages.into_iter().collect(),
///     vertex_input_state,
///     // Any other static configuration that never needs to change goes here
///     ..GraphicsPipelineCreateInfo::layout(layout)
/// };
///
/// let graphics = EngineGraphicsPipeline::new(renderer, move |subpass| GraphicsPipelineCreateInfo {
///     // Dynamic configuration goes here
///     // For example, subpass color blending
///     color_blend_state: Some(ColorBlendState::with_attachment_states(
///         subpass.num_color_attachments(),
///         ColorBlendAttachmentState::default(),
///     )),
///     ..base_create_info.clone()
/// });
/// # }
/// ```
///
/// [re]: crate::render::Renderer
pub struct EngineGraphicsPipeline {
    target: Arc<dyn RenderTarget>,
    gen_create_info: GenGraphicsPipelineCreateInfoFn,
    inner: Mutex<EngineGraphicsPipelineState>,
}

impl Debug for EngineGraphicsPipeline {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphicsPipelineCell")
            .field("target", &self.target)
            .field("inner", &self.inner)
            .finish()
    }
}

impl EngineGraphicsPipeline {
    /// Create a new pipeline.
    ///
    /// See [top-level documentation](Self) for more.
    pub fn new(
        renderer: &RendererHandle,
        gen_create_info: impl (Fn(&Subpass) -> GraphicsPipelineCreateInfo)
            + Send + Sync + 'static,
    ) -> Arc<Self> {
        let graphics = Arc::new(Self {
            target: renderer.target().clone(),
            gen_create_info: Box::new(gen_create_info),
            inner: Mutex::new(EngineGraphicsPipelineState::default()),
        });
        renderer.event_bus().register(RendererEventType::Stale, graphics.clone());
        graphics
    }

    /// Initialize this pipeline.
    ///
    /// Should be called exactly once.
    pub fn init(&self, subpass: Subpass) {
        self.inner.lock().subpass = Some(subpass);
    }
}

impl VKGraphicsPipelineSource for EngineGraphicsPipeline {
    /// Get the Vulkano [GraphicsPipeline], creating it if necessary.
    fn vk_graphics(&self) -> Arc<GraphicsPipeline> {
        let mut lock = self.inner.lock();
        if let Some(pipeline) = &lock.pipeline {
            pipeline.clone()
        } else {
            let subpass = lock.subpass.as_ref()
                .expect("GraphicsPipelineCell not initialized");

            // TODO: Make customizable if needed
            let viewport = Viewport {
                offset: [0.0, 0.0],
                extent: self.target.dimensions().into(),
                depth_range: 0.0..=1.0,
            };

            let pipeline = GraphicsPipeline::new(
                self.target.device().vk_device().clone(),
                None,
                GraphicsPipelineCreateInfo {
                    viewport_state: Some(ViewportState {
                        viewports: [viewport].into_iter().collect(),
                        ..Default::default()
                    }),
                    subpass: Some(subpass.clone().into()),
                    ..(self.gen_create_info)(subpass)
                }
            ).unwrap();

            lock.pipeline = Some(pipeline.clone());
            pipeline
        }
    }
}

impl EventHandler<RendererEventType, RendererEventData> for EngineGraphicsPipeline {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale,
                   "GraphicsPipelineCell can only handle 'Stale' Renderer events");
        self.inner.lock().pipeline = None;
    }
}