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

/// Viewport style when creating a pipeline.
///
/// Vulkan by default uses a top-left coordinate system, which is flipped from the classic style
/// of OpenGL. This enumeration allows each graphics pipeline to set their own style.
///
/// Additionally, you can also use a completely custom viewport style, if needed.
#[derive(Clone)]
pub enum ViewportType {
    /// Vulkan's default coordinate system.
    ///
    /// [-1, -1] is top-left, [1, 1] is bottom right.
    TopLeft,
    /// OpenGL style coordinate system.
    ///
    /// [-1, -1] is bottom-left, [1, 1] is top right.
    BottomLeft,
    /// Use a custom function to create the viewport.
    Custom(Arc<dyn (Fn(&Arc<dyn RenderTarget>) -> Viewport) + Send + Sync + 'static>),
}

impl Debug for ViewportType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TopLeft => write!(f, "TopLeft"),
            Self::BottomLeft => write!(f, "BottomLeft"),
            Self::Custom(_) => write!(f, "Custom"),
        }
    }
}

impl Default for ViewportType {
    #[inline]
    fn default() -> Self {
        Self::TopLeft
    }
}

impl<F> From<F> for ViewportType
where
    F: (Fn(&Arc<dyn RenderTarget>) -> Viewport) + Send + Sync + 'static,
{
    #[inline]
    fn from(value: F) -> Self {
        Self::Custom(Arc::new(value))
    }
}

impl ViewportType {
    /// Create a [Viewport] using information from a given [RenderTarget].
    pub fn viewport(&self, target: &Arc<dyn RenderTarget>) -> Viewport {
        match self {
            Self::TopLeft => {
                Viewport {
                    offset: [0.0, 0.0],
                    extent: target.dimensions().into(),
                    depth_range: 0.0..=1.0,
                }
            },
            Self::BottomLeft => {
                let extent: glm::TVec2<f32> = target.dimensions().into();
                Viewport {
                    offset: [0.0, extent.y],
                    extent: [extent.x, -extent.y],
                    depth_range: 0.0..=1.0,
                }
            },
            Self::Custom(create_viewport) => create_viewport(target),
        }
    }
}

#[derive(Debug, Default)]
struct EngineGraphicsPipelineState {
    pipeline: Option<Arc<GraphicsPipeline>>
}

/// Helper struct for maintaining a Vulkano [GraphicsPipeline].
///
/// This struct integrates with a [Renderer][re]'s event system in order to invalidate itself when
/// the [Renderer][re] is stale.
///
/// In order to recreate the underlying pipeline, this struct needs to know what
/// [GraphicsPipelineCreateInfo] to use, and will expect some base-level create info to be given
/// to it. The [viewport state][civs] field does not need to be set, as it will be set on pipeline
/// creation based on the current [render target][rt]'s [dimensions][rtd].
///
/// [re]: crate::render::Renderer
/// [civs]: GraphicsPipelineCreateInfo::viewport_state
/// [rt]: RenderTarget
/// [rtd]: RenderTarget::dimensions
///
/// # Examples
/// ```
/// # use vulkano::pipeline::graphics::vertex_input::VertexInputState;
/// use vulkano::pipeline::{PipelineLayout, PipelineShaderStageCreateInfo};
/// use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
/// use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
/// # use vulkano::render_pass::Subpass;
/// # use vulkano::shader::EntryPoint;
/// use gtether::render::pipeline::{EngineGraphicsPipeline, ViewportType};
/// # use gtether::render::RendererHandle;
///
/// # fn wrapper(renderer: &RendererHandle, subpass: &Subpass, vertex_shader: EntryPoint, fragment_shader: EntryPoint, vertex_input_state: Option<VertexInputState>) {
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
/// let create_info = GraphicsPipelineCreateInfo {
///     stages: stages.into_iter().collect(),
///     vertex_input_state,
///     subpass: Some(subpass.clone().into()),
///     // Any other configuration can go here
///     ..GraphicsPipelineCreateInfo::layout(layout)
/// };
///
/// let graphics = EngineGraphicsPipeline::new(
///     renderer,
///     create_info,
///     ViewportType::default(),
/// );
/// # }
/// ```
#[derive(Debug)]
pub struct EngineGraphicsPipeline {
    target: Arc<dyn RenderTarget>,
    create_info: GraphicsPipelineCreateInfo,
    viewport_type: ViewportType,
    inner: Mutex<EngineGraphicsPipelineState>,
}

impl EngineGraphicsPipeline {
    /// Create a new pipeline.
    ///
    /// See [top-level documentation](Self) for more.
    pub fn new(
        renderer: &RendererHandle,
        create_info: GraphicsPipelineCreateInfo,
        viewport_type: ViewportType,
    ) -> Arc<Self> {
        let graphics = Arc::new(Self {
            target: renderer.target().clone(),
            create_info,
            viewport_type,
            inner: Mutex::new(EngineGraphicsPipelineState::default()),
        });
        renderer.event_bus().register(RendererEventType::Stale, graphics.clone());
        graphics
    }
}

impl VKGraphicsPipelineSource for EngineGraphicsPipeline {
    /// Get the Vulkano [GraphicsPipeline], creating it if necessary.
    fn vk_graphics(&self) -> Arc<GraphicsPipeline> {
        let mut lock = self.inner.lock();
        if let Some(pipeline) = &lock.pipeline {
            pipeline.clone()
        } else {
            let viewport = self.viewport_type.viewport(&self.target);

            let pipeline = GraphicsPipeline::new(
                self.target.device().vk_device().clone(),
                None,
                GraphicsPipelineCreateInfo {
                    viewport_state: Some(ViewportState {
                        viewports: [viewport].into_iter().collect(),
                        ..Default::default()
                    }),
                    ..self.create_info.clone()
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