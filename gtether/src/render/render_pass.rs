use ahash::{AHashMap, AHashSet, HashMap, HashMapExt};
use itertools::Itertools;
use std::sync::Arc;
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer, RenderPassBeginInfo, SubpassBeginInfo, SubpassContents, SubpassEndInfo};
use vulkano::format::ClearValue;
use vulkano::image::{ImageLayout, SampleCount};
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentReference, AttachmentStoreOp, RenderPass, RenderPassCreateInfo, Subpass, SubpassDependency, SubpassDescription};
use vulkano::sync::{AccessFlags, DependencyFlags, PipelineStages};
use vulkano::{Validated, VulkanError};

use crate::render::frame::EngineFramebuffer;
use crate::render::{EngineDevice, RenderTarget, RenderTargetExt, Renderer, VulkanoError};

/// Represents a Vulkan render pass, and all the logic associated with it.
///
/// The gTether engine provides some standard implementations of [EngineRenderPass], but custom
/// implementations can also be used if desired.
///
/// Implementations must be [Send]/[Sync] so that they can be passed off to separate render threads.
pub trait EngineRenderPass: Send + Sync + 'static {
    /// The Vulkano [RenderPass] that is created and maintained by this [EngineRenderPass].
    fn vk_render_pass(&self) -> &Arc<RenderPass>;

    /// A mapping of attachment names to attachment indices.
    fn attachment_name_map(&self) -> HashMap<String, u32>;

    /// Build the render commands used to render a particular frame.
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        frame: &EngineFramebuffer,
    ) -> Result<(), Validated<VulkanoError>>;
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
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>>;
}

struct EngineRenderSubpass {
    handlers: Vec<Box<dyn EngineRenderHandler>>,
}

impl EngineRenderSubpass {
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Result<(), Validated<VulkanoError>> {
        for handler in &self.handlers {
            handler.build_commands(builder)?;
        }
        Ok(())
    }
}

struct StandardEngineRenderPass {
    render_pass: Arc<RenderPass>,
    subpasses: Vec<EngineRenderSubpass>,
    attachment_name_map: HashMap<String, u32>,
    clear_values: Vec<Option<ClearValue>>,
}

impl EngineRenderPass for StandardEngineRenderPass {
    #[inline]
    fn vk_render_pass(&self) -> &Arc<RenderPass> { &self.render_pass }

    #[inline]
    fn attachment_name_map(&self) -> HashMap<String, u32> { self.attachment_name_map.clone() }

    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        frame: &EngineFramebuffer,
    ) -> Result<(), Validated<VulkanoError>> {

        builder.begin_render_pass(
            RenderPassBeginInfo {
                clear_values: self.clear_values.clone(),
                ..RenderPassBeginInfo::framebuffer(frame.buffer().clone())
            },
            SubpassBeginInfo {
                contents: SubpassContents::Inline,
                ..Default::default()
            },
        )?;

        let mut first = true;
        for subpass in self.subpasses.iter() {
            if !first {
                builder.next_subpass(
                    SubpassEndInfo::default(),
                    SubpassBeginInfo {
                        contents: SubpassContents::Inline,
                        ..Default::default()
                    },
                )?;
            }
            subpass.build_commands(builder)?;
            first = false;
        }

        builder.end_render_pass(SubpassEndInfo::default())?;
        Ok(())
    }
}

type CreateHandlerFn = Box<dyn FnOnce(&Arc<Renderer>, &Subpass)
    -> Box<dyn EngineRenderHandler>>;

struct SubpassInfo {
    description: SubpassDescription,
    create_handlers: Vec<CreateHandlerFn>,
    attachments: AHashSet<u32>,
}

/// Builder pattern for creating a standard [EngineRenderPass].
pub struct EngineRenderPassBuilder {
    renderer: Arc<Renderer>,
    attachments: Vec<AttachmentDescription>,
    attachment_name_map: HashMap<String, u32>,
    clear_values: Vec<Option<ClearValue>>,
    subpass_infos: Vec<SubpassInfo>,
}

impl EngineRenderPassBuilder {
    /// Start a new builder.
    ///
    /// Requires a [Renderer] to use as a basis for the created [EngineRenderPass].
    pub fn new(renderer: Arc<Renderer>) -> Self {
        Self {
            renderer,
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
    pub fn final_color_attachment(
        mut self,
        name: impl Into<String>,
        clear_value: impl Into<ClearValue>,
    ) -> Result<Self, Validated<VulkanError>> {
        let format = self.renderer.target().format(self.renderer.device())?;
        Ok(self.attachment(name.into(), AttachmentDescription {
            format,
            samples: SampleCount::try_from(1).unwrap(),
            load_op: AttachmentLoadOp::Clear,
            store_op: AttachmentStoreOp::Store,
            ..Default::default()
        }, Some(clear_value)))
    }

    /// Begin building a new subpass for this render pass.
    #[inline]
    #[allow(unused_mut)]
    pub fn begin_subpass(mut self) -> EngineRenderSubpassBuilder { EngineRenderSubpassBuilder::new(self) }

    /// Build the [EngineRenderPass].
    ///
    /// # Errors
    ///
    /// Errors when any of the Vulkano objects fail to create.
    pub fn build(self) -> Result<Arc<dyn EngineRenderPass>, Validated<VulkanoError>> {
        let mut attachment_to_subpass_map: AHashMap<u32, AHashSet<u32>> = AHashMap::new();
        for (subpass_id, subpass_info) in self.subpass_infos.iter().enumerate() {
            for attachment in &subpass_info.attachments {
                attachment_to_subpass_map.entry(attachment.clone())
                    .or_default()
                    .insert(subpass_id as u32);
            }
        }

        let dependencies = self.subpass_infos.iter().enumerate()
            .map(|(dst_id, subpass_info)| {
                let dst_id = dst_id as u32;
                subpass_info.description.input_attachments.iter()
                    .flatten()
                    .map(|atch_ref| {
                        attachment_to_subpass_map[&atch_ref.attachment].iter().cloned()
                    })
                    .flatten()
                    .unique()
                    .map(move |src_id| SubpassDependency {
                        src_subpass: Some(src_id),
                        dst_subpass: Some(dst_id),
                        src_stages: PipelineStages::ALL_GRAPHICS,
                        dst_stages: PipelineStages::ALL_GRAPHICS,
                        src_access: AccessFlags::MEMORY_READ | AccessFlags::MEMORY_WRITE,
                        dst_access: AccessFlags::MEMORY_READ | AccessFlags::MEMORY_WRITE,
                        dependency_flags: DependencyFlags::BY_REGION,
                        ..Default::default()
                    })
            })
            .flatten()
            .collect::<Vec<_>>();

        let (subpasses, handler_groups): (Vec<_>, Vec<_>)
            = self.subpass_infos.into_iter()
            .map(|subpass_info| (subpass_info.description, subpass_info.create_handlers))
            .unzip();

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
            self.renderer.device().vk_device().clone(),
            RenderPassCreateInfo {
                attachments,
                subpasses,
                dependencies,
                ..Default::default()
            }
        ).map_err(|v| v.map(Into::into))?;

        let subpasses = handler_groups.into_iter().enumerate()
            .map(|(idx, handlers)| {
                let subpass = Subpass::from(render_pass.clone(), idx as u32).unwrap();
                let handlers = handlers.into_iter()
                    .map(|handler| handler(&self.renderer, &subpass))
                    .collect::<Vec<_>>();
                EngineRenderSubpass {
                    handlers,
                }
            }).collect::<Vec<_>>();

        Ok(Arc::new(StandardEngineRenderPass {
            render_pass,
            subpasses,
            attachment_name_map: self.attachment_name_map,
            clear_values: self.clear_values,
        }))
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
    /// fully create the Vulkano [RenderPass] and it's [Subpasses](Subpass) before handlers are
    /// created, as many handlers depend on these during creation.
    ///
    /// This handler will be called when it is time to render this subpass. Multiple handlers can
    /// be added, and they will be executed during rendering in the order that they were added.
    pub fn handler<H>(
        mut self,
        handler: impl (FnOnce(&Arc<Renderer>, &Subpass) -> H) + 'static,
    ) -> Self
    where
        H: EngineRenderHandler
    {
        self.handlers.push(Box::new(|renderer, subpass| {
            Box::new(handler(renderer, subpass))
        }));
        self
    }

    /// Build this subpass and return to the top-level [EngineRenderPassBuilder].
    pub fn end_subpass(mut self) -> EngineRenderPassBuilder {
        let attachments = self.color_attachments.iter()
            .chain(&[self.depth_stencil_attachment.clone()])
            .flatten()
            .map(|atch_ref| atch_ref.attachment)
            .collect::<AHashSet<_>>();

        let all_attachments = &attachments | &self.input_attachments.iter()
            .flatten()
            .map(|atch_ref| atch_ref.attachment)
            .collect::<AHashSet<_>>();

        let preserve_attachments = (0 .. self.parent.attachments.len() as u32)
            .filter(|atch_id| !all_attachments.contains(atch_id))
            .collect::<Vec<_>>();

        let description = SubpassDescription {
            color_attachments: self.color_attachments,
            depth_stencil_attachment: self.depth_stencil_attachment,
            input_attachments: self.input_attachments,
            preserve_attachments,
            ..Default::default()
        };

        self.parent.subpass_infos.push(SubpassInfo {
            description,
            create_handlers: self.handlers,
            attachments,
        });
        self.parent
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
/// # use vulkano::{Validated, VulkanError};
/// # use gtether::render::{EngineDevice, RenderTarget};
/// # use gtether::render::render_pass::{EngineRenderPass};
/// use gtether::render::{Renderer, RendererConfig, VulkanoError};
/// use gtether::render::render_pass::NoOpEngineRenderPass;
///
/// # fn wrapper(target: Arc<dyn RenderTarget>, device: Arc<EngineDevice>, render_pass: Arc<dyn EngineRenderPass>) -> Result<(), Validated<VulkanoError>> {
/// // Note that Renderer::new() already does exactly this
/// let noop_render_pass = NoOpEngineRenderPass::new(&target, &device)
///     .map_err(VulkanoError::from_validated)?;
/// let renderer = Renderer::with_render_pass(target, device, noop_render_pass)?;
///
/// // ... Do some work to create an actual render pass ...
///
/// renderer.set_render_pass(render_pass);
/// # Ok(())
/// # }
/// ```
pub struct NoOpEngineRenderPass {
    vk_render_pass: Arc<RenderPass>,
}

impl NoOpEngineRenderPass {
    /// Create a new [NoOpEngineRenderPass].
    pub fn new(
        target: &Arc<dyn RenderTarget>,
        device: &Arc<EngineDevice>,
    ) -> Result<Arc<dyn EngineRenderPass>, Validated<VulkanError>> {
        let vk_render_pass = vulkano::single_pass_renderpass!(
            device.vk_device().clone(),
            attachments: {
                color: {
                    format: target.format(device)?,
                    samples: 1,
                    load_op: Clear,
                    store_op: Store,
                },
            },
            pass: {
                color: [color],
                depth_stencil: {},
            },
        )?;

        Ok(Arc::new(Self {
            vk_render_pass,
        }))
    }
}

impl EngineRenderPass for NoOpEngineRenderPass {
    #[inline]
    fn vk_render_pass(&self) -> &Arc<RenderPass> {
        &self.vk_render_pass
    }

    #[inline]
    fn attachment_name_map(&self) -> HashMap<String, u32> {
        HashMap::new()
    }

    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        frame: &EngineFramebuffer,
    ) -> Result<(), Validated<VulkanoError>> {
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
            )?
            .end_render_pass(SubpassEndInfo::default())?;
        Ok(())
    }
}