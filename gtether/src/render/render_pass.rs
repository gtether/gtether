use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer, RenderPassBeginInfo, SubpassBeginInfo, SubpassContents, SubpassEndInfo};
use vulkano::format::ClearValue;
use vulkano::image::{Image, ImageCreateInfo, ImageLayout, ImageUsage, SampleCount};
use vulkano::image::view::ImageView;
use vulkano::memory::allocator::AllocationCreateInfo;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentReference, AttachmentStoreOp, RenderPass, RenderPassCreateInfo, Subpass, SubpassDependency, SubpassDescription};
use vulkano::sync::{AccessFlags, DependencyFlags, PipelineStages};

use crate::render::RenderTarget;
use crate::render::swapchain::Framebuffer;

/// Represents a particular attachment for a render pass
pub enum AttachmentBuffer {
    /// Represents a transient attachment used within a render pass, for passing data between
    /// subpasses.
    Transient(Arc<ImageView>),

    /// Marker that represents an attachment that is used as the output for a render pass.
    ///
    /// For example, the final image that gets rendered for presenting in a swapchain would use
    /// this marker.
    Output(),
}

/// Mapping of attachments to their friendly name.
pub trait AttachmentMap {
    /// Retrieves an attachment by name and framebuffer index.
    ///
    /// If there are no attachments associated by that name, or the framebuffer index is out of
    /// bounds, returns None.
    fn get_attachment(&self, name: String, frame_index: usize) -> Option<&AttachmentBuffer>;
    /// Retrieves all attachments (in order) for a particular framebuffer index.
    ///
    /// If the framebuffer index is out of bounds, returns None.
    fn get_attachments_for_frame(&self, frame_index: usize) -> Option<&Vec<AttachmentBuffer>>;
}

/// Represents a Vulkan render pass, and all the logic associated with it.
///
/// The gTether engine provides some standard implementations of [EngineRenderPass], but custom
/// implementations can also be used if desired.
///
/// Implementations must be [Send]able so that they can be passed off to separate render threads.
pub trait EngineRenderPass: Send {
    /// The [vulkano::render_pass::RenderPass] that is created and maintained by this [EngineRenderPass].
    fn render_pass(&self) -> &Arc<RenderPass>;
    /// A mapping of attachment names to [AttachmentBuffer]s.
    fn attachment_map(&self) -> Arc<dyn AttachmentMap>;
    /// Build the render commands used to render a particular frame.
    ///
    /// In general, this delegates to [EngineRenderHandler]s to build specific commands.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
    /// Recreate this [EngineRenderPass] for a given [RenderTarget].
    ///
    /// This is generally called when a render target invalidates it's swapchain for one reason or
    /// another, such as when the surface resizes. It is expected that all resources that need to be
    /// recreated when a swapchain invalidates are done so here.
    fn recreate(&mut self, target: &Arc<dyn RenderTarget>);
}

/// An individual render handler, used for rendering specific details of subpasses.
///
/// [EngineRenderHandler] is usually implemented by engine users for custom rendering logic, but the
/// gTether engine does provide several built-in handlers for certain use-cases.
///
/// Implementations must be [Send]able so that they can be passed off to separate render threads.
pub trait EngineRenderHandler: Send {
    /// Build the specific render commands used to render the part of the frame this handler is
    /// responsible for.
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer);
    /// Recreate this [EngineRenderHandler] for a given [RenderTarget] and [Subpass].
    ///
    /// This is generally called when a render target invalidates it's swapchain for one reason or
    /// another, such as when the surface resizes. It is expected that all resources that need to be
    /// recreated when a swapchain invalidates are done so here.
    fn recreate(&mut self, target: &Arc<dyn RenderTarget>, subpass: &Subpass, attachments: &Arc<dyn AttachmentMap>);
}

struct EngineRenderSubpass {
    subpass: Subpass,
    handlers: Vec<Box<dyn EngineRenderHandler>>,
}

impl EngineRenderSubpass {
    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, frame: &Framebuffer) {
        for handler in &self.handlers {
            handler.build_commands(builder, frame);
        }
    }

    fn recreate(
        &mut self,
        target: &Arc<dyn RenderTarget>,
        attachments: &Arc<dyn AttachmentMap>,
    ) {
        for handler in &mut self.handlers {
            handler.recreate(target, &self.subpass, attachments);
        }
    }
}

struct AttachmentData {
    // indexed by frame, and then by Vulkan index
    buffers: Vec<Vec<AttachmentBuffer>>,
    name_map: HashMap<String, u32>,
}

impl AttachmentData {
    fn new(target: &Arc<dyn RenderTarget>, render_pass: &Arc<RenderPass>, name_map: HashMap<String, u32>) -> Arc<Self> {
        let mut create_infos = render_pass.attachments().iter()
            .map(|atch| ImageCreateInfo {
                extent: target.dimensions().into(),
                format: atch.format,
                usage: ImageUsage::empty(),
                ..Default::default()
            }).collect::<Vec<_>>();

        for subpass in render_pass.subpasses().iter() {
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

        let buffers = (0 .. target.framebuffer_count()).map(|_| {
            create_infos.iter()
                .map(|create_info|
                    if create_info.usage.contains(ImageUsage::TRANSIENT_ATTACHMENT) {
                        AttachmentBuffer::Transient(ImageView::new_default(Image::new(
                            target.device().memory_allocator().clone(),
                            create_info.clone(),
                            AllocationCreateInfo::default(),
                        ).unwrap()).unwrap())
                    } else {
                        AttachmentBuffer::Output()
                    }
                ).collect::<Vec<_>>()
        }).collect::<Vec<_>>();

        Arc::new(Self {
            buffers,
            name_map,
        })
    }
}

impl AttachmentMap for AttachmentData {
    #[inline]
    fn get_attachment(&self, name: String, frame_index: usize) -> Option<&AttachmentBuffer> {
        let atch_idx = self.name_map.get(&name)?.clone();
        self.buffers.get(frame_index)?.get(atch_idx as usize)
    }

    #[inline]
    fn get_attachments_for_frame(&self, frame_index: usize) -> Option<&Vec<AttachmentBuffer>> {
        self.buffers.get(frame_index)
    }
}

struct StandardEngineRenderPass {
    render_pass: Arc<RenderPass>,
    subpasses: Vec<EngineRenderSubpass>,
    attachments: Arc<AttachmentData>,
    clear_values: Vec<Option<ClearValue>>,
}

impl StandardEngineRenderPass {
    fn new(
        target: &Arc<dyn RenderTarget>,
        render_pass: Arc<RenderPass>,
        subpasses: Vec<EngineRenderSubpass>,
        attachment_name_map: HashMap<String, u32>,
        clear_values: Vec<Option<ClearValue>>,
    ) -> Self {
        let attachments = AttachmentData::new(target, &render_pass, attachment_name_map);

        Self {
            render_pass,
            subpasses,
            attachments,
            clear_values,
        }
    }
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

    fn recreate(&mut self, target: &Arc<dyn RenderTarget>) {
        self.attachments = AttachmentData::new(target, &self.render_pass, self.attachments.name_map.clone());
        let attachments = self.attachment_map();
        for subpass in self.subpasses.iter_mut() {
            subpass.recreate(target, &attachments);
        }
    }
}

/// Builder pattern for creating a standard [EngineRenderPass].
pub struct EngineRenderPassBuilder {
    target: Arc<dyn RenderTarget>,
    attachments: Vec<AttachmentDescription>,
    attachment_name_map: HashMap<String, u32>,
    clear_values: Vec<Option<ClearValue>>,
    subpass_infos: Vec<(SubpassDescription, Vec<Box<dyn EngineRenderHandler>>)>,
}

impl EngineRenderPassBuilder {
    /// Start a new builder.
    ///
    /// Requires a [RenderTarget] to use as a basis for the created [EngineRenderPass].
    pub fn new(target: &Arc<dyn RenderTarget>) -> Self {
        Self {
            target: target.clone(),
            attachments: vec![],
            attachment_name_map: HashMap::new(),
            clear_values: vec![],
            subpass_infos: vec![],
        }
    }

    fn attachment_ref(&self, name: &String) -> AttachmentReference {
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
        name: String,
        atch: AttachmentDescription,
        clear_value: Option<ClearValue>,
    ) -> Self {
        let atch_num = self.attachments.len() as u32;
        self.attachments.push(atch);
        self.attachment_name_map.insert(name, atch_num);
        self.clear_values.push(clear_value);
        self
    }

    /// Add an attachment representing the final output color.
    ///
    /// This is a convenience method on top of [Self::attachment()]; the format and similar values
    /// are deduced from information gathered from the [RenderTarget].
    #[allow(unused_mut)]
    pub fn final_color_attachment(mut self, name: String, clear_value: ClearValue) -> Self {
        let format = self.target.format();
        self.attachment(name, AttachmentDescription {
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
    pub fn build(self) -> Box<dyn EngineRenderPass> {
        let vk_device = self.target.device().vk_device();

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

        let subpasses = handler_groups.into_iter().enumerate()
            .map(|(idx, handlers)| EngineRenderSubpass {
                subpass: Subpass::from(render_pass.clone(), idx as u32).unwrap(),
                handlers,
            }).collect::<Vec<_>>();

        Box::new(StandardEngineRenderPass::new(
            &self.target,
            render_pass,
            subpasses,
            self.attachment_name_map,
            self.clear_values,
        ))
    }

    /// Create a simple NoOp [EngineRenderPass], that does nothing but render a black image.
    pub fn noop(target: &Arc<dyn RenderTarget>) -> Box<dyn EngineRenderPass> {
        Self::new(target)
            .final_color_attachment("color".into(), [0.0, 0.0, 0.0, 1.0].into())
            .begin_subpass()
            .color_attachment("color".into())
            .end_subpass()
            .build()
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
    handlers: Vec<Box<dyn EngineRenderHandler>>,
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
    pub fn color_attachment(
        mut self,
        name: String,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::ColorAttachmentOptimal,
            ..self.parent.attachment_ref(&name)
        };
        self.color_attachments.push(Some(attachment));
        self
    }

    /// Define a depth stencil attachment for this subpass.
    ///
    /// The name should match an attachment created in the top-level [EngineRenderPassBuilder].
    pub fn depth_stencil_attachment(
        mut self,
        name: String,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::DepthStencilAttachmentOptimal,
            ..self.parent.attachment_ref(&name)
        };
        self.depth_stencil_attachment = Some(attachment);
        self
    }

    /// Define an input attachment for this subpass.
    ///
    /// The name should match an attachment created in the top-level [EngineRenderPassBuilder].
    pub fn input_attachment(
        mut self,
        name: String,
    ) -> Self {
        let attachment = AttachmentReference {
            layout: ImageLayout::ShaderReadOnlyOptimal,
            ..self.parent.attachment_ref(&name)
        };
        self.input_attachments.push(Some(attachment));
        self
    }

    /// Add an [EngineRenderHandler].
    ///
    /// This handler will be called when it is time to render this subpass. Multiple handlers can
    /// be added, and they will eventually be called in the order that they were added.
    pub fn handler(
        mut self,
        handler: Box<dyn EngineRenderHandler>,
    ) -> Self {
        self.handlers.push(handler);
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
