use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer, RenderPassBeginInfo, SubpassBeginInfo, SubpassContents, SubpassEndInfo};
use vulkano::format::ClearValue;
use vulkano::image::view::ImageView;
use vulkano::image::{Image, ImageCreateInfo, ImageLayout, ImageUsage, SampleCount};
use vulkano::memory::allocator::AllocationCreateInfo;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentReference, AttachmentStoreOp, RenderPass, RenderPassCreateInfo, Subpass, SubpassDependency, SubpassDescription};
use vulkano::sync::{AccessFlags, DependencyFlags, PipelineStages};

use crate::event::{Event, EventHandler};
use crate::render::swapchain::Framebuffer;
use crate::render::{RenderTarget, RendererEventData, RendererEventType, RendererHandle};
use crate::render::attachment::{AttachmentMap, AttachmentType};

/// Represents a Vulkan render pass, and all the logic associated with it.
///
/// The gTether engine provides some standard implementations of [EngineRenderPass], but custom
/// implementations can also be used if desired.
///
/// Implementations must be [Send]/[Sync] so that they can be passed off to separate render threads.
pub trait EngineRenderPass: Send + Sync + 'static {
    /// The Vulkano [RenderPass] that is created and maintained by this [EngineRenderPass].
    fn render_pass(&self) -> &Arc<RenderPass>;

    /// A mapping of attachment names to [AttachmentType]s.
    fn attachment_map(&self) -> Arc<dyn AttachmentMap>;

    /// Build the render commands used to render a particular frame.
    ///
    /// In general, this delegates to [EngineRenderHandler]s to build specific commands.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
}

/// An individual render handler, used for rendering specific details of subpasses.
///
/// [EngineRenderHandler] is usually implemented by engine users for custom rendering logic, but the
/// gTether engine does provide several built-in handlers for certain use-cases.
///
/// Implementations must be [Send]/[Sync] so that they can be passed off to separate render threads.
pub trait EngineRenderHandler: Send + Sync + 'static {
    /// Build the specific render commands used to render the part of the frame this handler is
    /// responsible for.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
}

struct EngineRenderSubpass {
    handlers: Vec<Box<dyn EngineRenderHandler>>,
}

impl EngineRenderSubpass {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        for handler in &self.handlers {
            handler.build_commands(builder, frame);
        }
    }
}

struct AttachmentData {
    vk_render_pass: Arc<RenderPass>,
    // indexed by frame, and then by Vulkan index
    buffers: RwLock<Vec<Vec<AttachmentType>>>,
    name_map: HashMap<String, u32>,
}

impl AttachmentData {
    fn create_buffers(
        target: &Arc<dyn RenderTarget>,
        vk_render_pass: &Arc<RenderPass>,
    ) -> Vec<Vec<AttachmentType>> {
        let mut create_infos = vk_render_pass.attachments().iter()
            .map(|atch| ImageCreateInfo {
                extent: target.dimensions().into(),
                format: atch.format,
                usage: ImageUsage::empty(),
                ..Default::default()
            }).collect::<Vec<_>>();

        for subpass in vk_render_pass.subpasses().iter() {
            // TODO: Need to go through *_resolve attachments?
            for color_atch in subpass.color_attachments.iter().flatten() {
                create_infos[color_atch.attachment as usize].usage = ImageUsage::COLOR_ATTACHMENT;
            }
            for depth_stencil_atch in subpass.depth_stencil_attachment.iter() {
                create_infos[depth_stencil_atch.attachment as usize].usage = ImageUsage::DEPTH_STENCIL_ATTACHMENT | ImageUsage::TRANSIENT_ATTACHMENT;
            }
            for input_atch in subpass.input_attachments.iter().flatten() {
                create_infos[input_atch.attachment as usize].usage |= ImageUsage::TRANSIENT_ATTACHMENT | ImageUsage::INPUT_ATTACHMENT;
            }
        }

        (0 .. target.framebuffer_count()).map(|_| {
            create_infos.iter()
                .map(|create_info|
                    if create_info.usage.contains(ImageUsage::TRANSIENT_ATTACHMENT) {
                        AttachmentType::Transient(ImageView::new_default(Image::new(
                            target.device().memory_allocator().clone(),
                            create_info.clone(),
                            AllocationCreateInfo::default(),
                        ).unwrap()).unwrap())
                    } else {
                        AttachmentType::Output
                    }
                ).collect::<Vec<_>>()
        }).collect::<Vec<_>>()
    }

    fn new(target: &Arc<dyn RenderTarget>, render_pass: &Arc<RenderPass>, name_map: HashMap<String, u32>) -> Arc<Self> {
        Arc::new(Self {
            vk_render_pass: render_pass.clone(),
            buffers: RwLock::new(Self::create_buffers(target, render_pass)),
            name_map,
        })
    }
}

impl AttachmentMap for AttachmentData {
    #[inline]
    fn get_attachment(&self, name: String, frame_index: usize) -> Option<AttachmentType> {
        let atch_idx = self.name_map.get(&name)?.clone();
        self.buffers.read().get(frame_index)?.get(atch_idx as usize).map(|v| v.clone())
    }

    #[inline]
    fn get_attachments_for_frame(&self, frame_index: usize) -> Option<Vec<AttachmentType>> {
        self.buffers.read().get(frame_index).map(|v| v.clone())
    }
}

impl EventHandler<RendererEventType, RendererEventData> for AttachmentData {
    fn handle_event(&self, event: &mut Event<RendererEventType, RendererEventData>) {
        assert_eq!(event.event_type(), &RendererEventType::Stale);
        *self.buffers.write() = Self::create_buffers(event.target(), &self.vk_render_pass);
    }
}

struct StandardEngineRenderPass {
    render_pass: Arc<RenderPass>,
    subpasses: Vec<EngineRenderSubpass>,
    attachments: Arc<AttachmentData>,
    clear_values: Vec<Option<ClearValue>>,
}

impl EngineRenderPass for StandardEngineRenderPass {
    #[inline]
    fn render_pass(&self) -> &Arc<RenderPass> { &self.render_pass }

    #[inline]
    fn attachment_map(&self) -> Arc<dyn AttachmentMap> { self.attachments.clone() }

    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {

        builder.begin_render_pass(
            RenderPassBeginInfo {
                clear_values: self.clear_values.clone(),
                ..RenderPassBeginInfo::framebuffer(frame.buffer().clone())
            },
            SubpassBeginInfo {
                contents: SubpassContents::Inline,
                ..Default::default()
            },
        ).unwrap();

        let mut first = true;
        for subpass in self.subpasses.iter() {
            if !first {
                builder.next_subpass(
                    SubpassEndInfo::default(),
                    SubpassBeginInfo {
                        contents: SubpassContents::Inline,
                        ..Default::default()
                    },
                ).unwrap();
            }
            subpass.build_commands(builder, frame);
            first = false;
        }

        builder.end_render_pass(SubpassEndInfo::default()).unwrap();
    }
}

type CreateHandlerFn = Box<dyn FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>)
    -> Box<dyn EngineRenderHandler>>;

/// Builder pattern for creating a standard [EngineRenderPass].
pub struct EngineRenderPassBuilder {
    renderer: RendererHandle,
    attachments: Vec<AttachmentDescription>,
    attachment_name_map: HashMap<String, u32>,
    clear_values: Vec<Option<ClearValue>>,
    subpass_infos: Vec<(SubpassDescription, Vec<CreateHandlerFn>)>,
}

impl EngineRenderPassBuilder {
    /// Start a new builder.
    ///
    /// Requires a [RendererHandle] to use as a basis for the created [EngineRenderPass].
    pub fn new(renderer: &RendererHandle) -> Self {
        Self {
            renderer: renderer.clone(),
            attachments: vec![],
            attachment_name_map: HashMap::new(),
            clear_values: vec![],
            subpass_infos: vec![],
        }
    }

    fn attachment_ref(&self, name: &str) -> AttachmentReference {
        let attachment = self.attachment_name_map.get(name).unwrap().clone();
        AttachmentReference {
            attachment,
            ..Default::default()
        }
    }

    /// Add an attachment to the render pass.
    ///
    /// A name is used to map the attachment within engine logic, but this name is not used within
    /// the render pipelines and shaders themselves.
    ///
    /// A clear value must be specified if the attachment's load op is CLEAR.
    pub fn attachment(
        mut self,
        name: impl Into<String>,
        atch: AttachmentDescription,
        clear_value: Option<impl Into<ClearValue>>,
    ) -> Self {
        let atch_num = self.attachments.len() as u32;
        self.attachments.push(atch);
        self.attachment_name_map.insert(name.into(), atch_num);
        self.clear_values.push(clear_value.map(Into::into));
        self
    }

    /// Add an attachment representing the final output color.
    ///
    /// This is a convenience method on top of [Self::attachment()]; the format and similar values
    /// are deduced from information gathered from the [RenderTarget].
    #[allow(unused_mut)]
    pub fn final_color_attachment(mut self, name: impl Into<String>, clear_value: impl Into<ClearValue>) -> Self {
        let format = self.renderer.target().format();
        self.attachment(name.into(), AttachmentDescription {
            format,
            samples: SampleCount::try_from(1).unwrap(),
            load_op: AttachmentLoadOp::Clear,
            store_op: AttachmentStoreOp::Store,
            ..Default::default()
        }, Some(clear_value))
    }

    /// Begin building a new subpass for this render pass.
    #[inline]
    #[allow(unused_mut)]
    pub fn begin_subpass(mut self) -> EngineRenderSubpassBuilder { EngineRenderSubpassBuilder::new(self) }

    /// Build the [EngineRenderPass].
    pub fn build(self) -> Arc<dyn EngineRenderPass> {
        let target = self.renderer.target();
        let vk_device = target.device().vk_device();

        // Ripped from the ordered_passes_renderpass() macro from Vulkano, as no changes are needed
        // to this logic
        let dependencies = (0..self.subpass_infos.len().saturating_sub(1) as u32)
            .map(|id| {
                SubpassDependency {
                    src_subpass: id.into(),
                    dst_subpass: (id + 1).into(),
                    src_stages: PipelineStages::ALL_GRAPHICS,
                    dst_stages: PipelineStages::ALL_GRAPHICS,
                    src_access: AccessFlags::MEMORY_READ | AccessFlags::MEMORY_WRITE,
                    dst_access: AccessFlags::MEMORY_READ | AccessFlags::MEMORY_WRITE,
                    dependency_flags: DependencyFlags::BY_REGION,
                    ..Default::default()
                }
            }).collect::<Vec<_>>();

        let (subpasses, handler_groups): (Vec<_>, Vec<_>)
            = self.subpass_infos.into_iter().unzip();

        // TODO: This block will result in an user-override of initial_layout being possible, but
        //       final_layout will always be determined by this block instead. Perhaps fix?
        let mut attachments = self.attachments;
        for subpass in subpasses.iter() {
            subpass.color_attachments.iter()
                .chain(&subpass.color_resolve_attachments)
                .chain(&[subpass.depth_stencil_attachment.clone()])
                .chain(&[subpass.depth_stencil_resolve_attachment.clone()])
                .chain(&subpass.input_attachments)
                .flatten()
                .for_each(|atch_ref| {
                    let attachment = &mut attachments[atch_ref.attachment as usize];
                    attachment.final_layout = atch_ref.layout;
                    if attachment.initial_layout == ImageLayout::Undefined {
                        attachment.initial_layout = attachment.final_layout;
                    }
                });
        }

        let render_pass = RenderPass::new(
            vk_device.clone(),
            RenderPassCreateInfo {
                attachments,
                subpasses,
                dependencies,
                ..Default::default()
            }
        ).unwrap();

        let attachments = AttachmentData::new(
            target,
            &render_pass,
            self.attachment_name_map,
        );
        self.renderer.event_bus().register(RendererEventType::Stale, attachments.clone());
        let dyn_attachments: Arc<dyn AttachmentMap> = attachments.clone();

        let subpasses = handler_groups.into_iter().enumerate()
            .map(|(idx, handlers)| {
                let subpass = Subpass::from(render_pass.clone(), idx as u32).unwrap();
                let handlers = handlers.into_iter()
                    .map(|handler| handler(&self.renderer, &subpass, &dyn_attachments))
                    .collect::<Vec<_>>();
                EngineRenderSubpass {
                    handlers,
                }
            }).collect::<Vec<_>>();

        Arc::new(StandardEngineRenderPass {
            render_pass,
            subpasses,
            attachments,
            clear_values: self.clear_values,
        })
    }
}

/// Sub-builder pattern for creating a subpass for an [EngineRenderPass].
///
/// This builder cannot be created directly, but instead should be created via
/// [EngineRenderPassBuilder::begin_subpass()]
pub struct EngineRenderSubpassBuilder {
    parent: EngineRenderPassBuilder,
    color_attachments: Vec<Option<AttachmentReference>>,
    // TODO: Add support for color_resolve_attachments
    depth_stencil_attachment: Option<AttachmentReference>,
    // TODO: Add support for depth_stencil_resolve
    input_attachments: Vec<Option<AttachmentReference>>,
    handlers: Vec<CreateHandlerFn>,
}

impl EngineRenderSubpassBuilder {
    fn new(parent: EngineRenderPassBuilder) -> Self {
        Self {
            parent,
            color_attachments: vec![],
            depth_stencil_attachment: None,
            input_attachments: vec![],
            handlers: vec![],
        }
    }

    /// Define a color attachment for this subpass.
    ///
    /// The name should match an attachment created in the top-level [EngineRenderPassBuilder].
    pub fn color_attachment<'a>(
        mut self,
        name: impl Into<&'a str>,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::ColorAttachmentOptimal,
            ..self.parent.attachment_ref(name.into())
        };
        self.color_attachments.push(Some(attachment));
        self
    }

    /// Define a depth stencil attachment for this subpass.
    ///
    /// The name should match an attachment created in the top-level [EngineRenderPassBuilder].
    pub fn depth_stencil_attachment<'a>(
        mut self,
        name: impl Into<&'a str>,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::DepthStencilAttachmentOptimal,
            ..self.parent.attachment_ref(name.into())
        };
        self.depth_stencil_attachment = Some(attachment);
        self
    }

    /// Define an input attachment for this subpass.
    ///
    /// The name should match an attachment created in the top-level [EngineRenderPassBuilder].
    pub fn input_attachment<'a>(
        mut self,
        name: impl Into<&'a str>,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::ShaderReadOnlyOptimal,
            ..self.parent.attachment_ref(name.into())
        };
        self.input_attachments.push(Some(attachment));
        self
    }

    /// Add an [EngineRenderHandler].
    ///
    /// Handlers are added via a callback that creates said handler. This allows the builder to
    /// fully create the Vulkano [RenderPass] and it's [Subpass]es before handlers are created, as
    /// many handlers depend on these during creation.
    ///
    /// This handler will be called when it is time to render this subpass. Multiple handlers can
    /// be added, and they will be executed during rendering in the order that they were added.
    pub fn handler<H>(
        mut self,
        handler: impl (FnOnce(&RendererHandle, &Subpass, &Arc<dyn AttachmentMap>) -> H) + 'static,
    ) -> Self
    where
        H: EngineRenderHandler
    {
        self.handlers.push(Box::new(|renderer, subpass, attachments| {
            Box::new(handler(renderer, subpass, attachments))
        }));
        self
    }

    /// Build this subpass and return to the top-level [EngineRenderPassBuilder].
    pub fn end_subpass(mut self) -> EngineRenderPassBuilder {
        let all_attachments = self.color_attachments.iter()
            .chain(&[self.depth_stencil_attachment.clone()])
            .chain(&self.input_attachments)
            .flatten()
            .map(|attachment| attachment.attachment)
            .collect::<HashSet<_>>();

        let preserve_attachments = (0 .. self.parent.attachments.len() as u32)
            .filter(|atch_id| !all_attachments.contains(atch_id))
            .collect::<Vec<_>>();

        let subpass_description = SubpassDescription {
            color_attachments: self.color_attachments,
            depth_stencil_attachment: self.depth_stencil_attachment,
            input_attachments: self.input_attachments,
            preserve_attachments,
            ..Default::default()
        };

        self.parent.subpass_infos.push((subpass_description, self.handlers));
        self.parent
    }
}

struct NoOpAttachmentMap {}

impl AttachmentMap for NoOpAttachmentMap {
    fn get_attachment(&self, name: String, _frame_index: usize) -> Option<AttachmentType> {
        if name.as_str() == "color" {
            Some(AttachmentType::Output)
        } else {
            None
        }
    }
    fn get_attachments_for_frame(&self, _frame_index: usize) -> Option<Vec<AttachmentType>> {
        Some(vec![AttachmentType::Output])
    }
}

/// A simple NoOp [EngineRenderPass] that does nothing except render a black image.
///
/// This "NoOp" render pass is suitable for early initialization, e.g. for starting a [Renderer]
/// with.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use gtether::render::{RenderTarget};
/// # use gtether::render::render_pass::{EngineRenderPass};
/// use gtether::render::Renderer;
/// use gtether::render::render_pass::NoOpEngineRenderPass;
///
/// # fn wrapper(target: &Arc<dyn RenderTarget>, render_pass: Arc<dyn EngineRenderPass>) {
/// let noop_render_pass = NoOpEngineRenderPass::new(target);
/// let (mut renderer, _) = Renderer::new(target, noop_render_pass);
///
/// // ... Do some work to create an actual render pass ...
///
/// renderer.set_render_pass(render_pass);
/// # }
/// ```
pub struct NoOpEngineRenderPass {
    vk_render_pass: Arc<RenderPass>,
}

impl NoOpEngineRenderPass {
    /// Create a new [NoOpEngineRenderPass].
    pub fn new(target: &Arc<dyn RenderTarget>) -> Arc<dyn EngineRenderPass> {
        let format = target.format();
        let vk_render_pass = vulkano::single_pass_renderpass!(
            target.device().vk_device().clone(),
            attachments: {
                color: {
                    format: format,
                    samples: 1,
                    load_op: Clear,
                    store_op: Store,
                },
            },
            pass: {
                color: [color],
                depth_stencil: {},
            },
        ).unwrap();

        Arc::new(Self {
            vk_render_pass,
        })
    }
}

impl EngineRenderPass for NoOpEngineRenderPass {
    fn render_pass(&self) -> &Arc<RenderPass> {
        &self.vk_render_pass
    }

    fn attachment_map(&self) -> Arc<dyn AttachmentMap> {
        Arc::new(NoOpAttachmentMap {})
    }

    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        builder
            .begin_render_pass(
                RenderPassBeginInfo {
                    clear_values: vec![Some([0.0, 0.0, 0.0, 1.0].into())],
                    ..RenderPassBeginInfo::framebuffer(frame.buffer().clone())
                },
                SubpassBeginInfo {
                    contents: SubpassContents::Inline,
                    ..Default::default()
                },
            ).unwrap()
            .end_render_pass(SubpassEndInfo::default()).unwrap();
    }
}