//! Concrete representation of a [Font][f] in the form of a sheet of glyph images.
//!
//! [FontSheet]s are one method of representing a sized and scaled [Font][f] in a concrete form,
//! allowing it to be rendered. [FontSheet]s are by nature limited to an explicit subsection of
//! character codes, however. If you need to represent a significantly large amount of Unicode, then
//! [FontSheet]s may not be a great way to do it.
//!
//! [f]: crate::render::font::Font

use std::sync::Arc;

use vulkano::buffer::{Buffer, BufferContents, BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::image::sampler::{Filter, Sampler, SamplerCreateInfo};
use vulkano::image::view::ImageView;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::pipeline::graphics::color_blend::{AttachmentBlend, ColorBlendAttachmentState, ColorBlendState};
use vulkano::pipeline::graphics::input_assembly::InputAssemblyState;
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::{CullMode, RasterizationState};
use vulkano::pipeline::graphics::vertex_input::{Vertex, VertexDefinition};
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{Pipeline, PipelineBindPoint, PipelineLayout, PipelineShaderStageCreateInfo};
use vulkano::render_pass::Subpass;

use crate::render::font::compositor::FontRenderer;
use crate::render::font::layout::PositionedChar;
use crate::render::font::size::FontSizer;
use crate::render::image::ImageSampler;
use crate::render::pipeline::{EngineGraphicsPipeline, VKGraphicsPipelineSource};
use crate::render::{FlatVertex, RenderTarget, RendererHandle};

/// Starting character code (inclusive) of the Unicode Latin chart
pub const UNICODE_LATIN_START: u32 = 0x20;
/// Ending character code (exclusive) of the Unicode Latin chart
pub const UNICODE_LATIN_END: u32 = 0x7F;

/// Mapping layer between character codes and [FontSheet] indices.
///
/// A [FontSheet] can only represent a subset of all possible character codes, so a [FontSheetMap]
/// serves to map between a given character code and the relevant index in the [FontSheet].
pub trait FontSheetMap: Send + Sync + 'static {
    /// Map a character code to a [FontSheet] index, or None if it doesn't exist in the [FontSheet].
    fn map_char_direct(&self, c: char) -> Option<u32>;
    /// The index to use as a replacement for all character codes that don't map; possibly None.
    fn replacement(&self) -> Option<u32>;
    /// The total count of indices possible to map to.
    fn count(&self) -> u32;
    /// Map a character code to a [FontSheet] index, or [FontSheetMap::replacement()] if it doesn't
    /// exist in the [FontSheet].
    fn map_char(&self, c: char) -> Option<u32> {
        self.map_char_direct(c).or(self.replacement())
    }
    /// All possible character codes that can map to an index.
    fn supported_chars(&self) -> Box<dyn Iterator<Item=char>>;
}

/// [FontSheetMap] for standard Unicode chart mapping.
///
/// Covers a finite sequential section of the Unicode space.
pub struct UnicodeFontSheetMap {
    char_start: u32,
    length: u32,
}

impl UnicodeFontSheetMap {
    /// Create a [UnicodeFontSheetMap] that covers `block_begin` to `block_end` indices in the
    /// Unicode space.
    pub fn new(block_begin: u32, block_end: u32) -> Box<dyn FontSheetMap> {
        Box::new(Self {
            char_start: block_begin,
            length: block_end - block_begin,
        })
    }

    /// A font sheet mapping that spans latin unicode \0020 (space) through \007E (~), which makes
    /// up the printable characters of the first unicode block "C0 Controls and Basic Latin".
    ///
    /// See also: https://www.unicode.org/charts/PDF/U0000.pdf
    pub fn basic_latin() -> Box<dyn FontSheetMap> {
        Self::new(UNICODE_LATIN_START, UNICODE_LATIN_END)
    }
}

impl FontSheetMap for UnicodeFontSheetMap {
    fn map_char_direct(&self, c: char) -> Option<u32> {
        let c_idx = c as u32;
        if c_idx < self.char_start || c_idx >= (self.char_start + self.length) {
            None
        } else {
            Some(c_idx - self.char_start)
        }
    }

    fn replacement(&self) -> Option<u32> {
        Some(self.length)
    }

    fn count(&self) -> u32 {
        // +1 for the replacement char
        self.length + 1
    }

    fn supported_chars(&self) -> Box<dyn Iterator<Item=char>> {
        Box::new(
            (self.char_start..(self.char_start + self.length))
                .map(|char_idx| char::from_u32(char_idx).unwrap())
                .chain([char::REPLACEMENT_CHARACTER].into_iter())
        )
    }
}

/// Pre-sized sheet of images representing an explicit subsection of a [Font][f].
///
/// [FontSheet]s are usually comprised of the following:
///  * Sheet of actual images
///  * [FontSheetMap] used to map character codes to image indices
///  * [FontSizer] used to size and position rendered text using this [FontSheet]
///
/// [f]: crate::render::font::Font
pub struct FontSheet {
    sheet: Arc<ImageView>,
    mapper: Box<dyn FontSheetMap>,
    sizer: Arc<dyn FontSizer>,
}

impl FontSheet {
    pub fn new(
        sheet: Arc<ImageView>,
        mapper: Box<dyn FontSheetMap>,
        sizer: Arc<dyn FontSizer>,
    ) -> Self {
        Self {
            sheet,
            mapper,
            sizer,
        }
    }

    /// A Vulkan ImageView for the array of glyph images this [FontSheet] represents.
    #[inline]
    pub fn image_view(&self) -> &Arc<ImageView> { &self.sheet }

    /// [FontSheetMap] for this [FontSheet].
    #[inline]
    pub fn mapper(&self) -> &Box<dyn FontSheetMap> { &self.mapper }

    /// [FontSizer] for this [FontSheet].
    #[inline]
    pub fn sizer(&self) -> &Arc<dyn FontSizer> { &self.sizer }
}

#[derive(BufferContents, Vertex)]
#[repr(C)]
struct GlyphInstance {
    #[format(R32_SFLOAT)]
    index: f32,
    #[format(R32G32_SFLOAT)]
    offset: [f32; 2],
    #[format(R32G32_SFLOAT)]
    scale: [f32; 2],
    #[format(R32G32B32_SFLOAT)]
    color: [f32; 3],
}

mod text_vert {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "src/render/font/shaders/sheet.vert",
    }
}

mod text_frag {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "src/render/font/shaders/sheet.frag",
    }
}

/// [FontRenderer] for [FontSheet].
pub struct FontSheetRenderer {
    target: Arc<dyn RenderTarget>,
    font_sheet: Arc<FontSheet>,
    graphics: Arc<EngineGraphicsPipeline>,
    font_sampler: Arc<ImageSampler>,
    glyph_buffer: Subbuffer<[FlatVertex]>,
}

impl FontSheetRenderer {
    /// Create a [FontSheetRenderer] from a [FontSheet].
    pub fn new(renderer: &RendererHandle, font_sheet: Arc<FontSheet>) -> Box<dyn FontRenderer> {
        let target = renderer.target();

        let text_vert = text_vert::load(target.device().vk_device().clone())
            .expect("Failed to create vertex shader module")
            .entry_point("main").unwrap();

        let text_frag = text_frag::load(target.device().vk_device().clone())
            .expect("Failed to create fragment shader module")
            .entry_point("main").unwrap();

        let vertex_input_state = Some([FlatVertex::per_vertex(), GlyphInstance::per_instance()]
            .definition(&text_vert.info().input_interface)
            .unwrap()
        );

        let stages = [
            PipelineShaderStageCreateInfo::new(text_vert),
            PipelineShaderStageCreateInfo::new(text_frag),
        ];

        let layout = PipelineLayout::new(
            target.device().vk_device().clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(target.device().vk_device().clone())
                .unwrap(),
        ).unwrap();

        let base_create_info = GraphicsPipelineCreateInfo {
            stages: stages.into_iter().collect(),
            vertex_input_state,
            input_assembly_state: Some(InputAssemblyState::default()),
            rasterization_state: Some(RasterizationState {
                cull_mode: CullMode::Back,
                ..Default::default()
            }),
            multisample_state: Some(MultisampleState::default()),
            ..GraphicsPipelineCreateInfo::layout(layout)
        };

        let graphics = EngineGraphicsPipeline::new(
            renderer,
            move |subpass| GraphicsPipelineCreateInfo {
                color_blend_state: Some(ColorBlendState::with_attachment_states(
                    subpass.num_color_attachments(),
                    ColorBlendAttachmentState {
                        blend: Some(AttachmentBlend::alpha()),
                        ..Default::default()
                    },
                )),
                ..base_create_info.clone()
            },
        );

        let font_sampler = ImageSampler::new(
            renderer,
            graphics.clone(),
            font_sheet.image_view().clone(),
            0,
            Sampler::new(
                target.device().vk_device().clone(),
                SamplerCreateInfo {
                    mag_filter: Filter::Linear,
                    min_filter: Filter::Linear,
                    ..Default::default()
                }
            ).unwrap(),
        );

        let glyph_buffer = FlatVertex::buffer(
            target.device().memory_allocator().clone(),
            glm::vec2(0.0, 0.0),
            glm::vec2(1.0, 1.0),
        );

        Box::new(Self {
            target: target.clone(),
            font_sheet,
            graphics,
            font_sampler,
            glyph_buffer,
        })
    }
}

impl FontRenderer for FontSheetRenderer {
    fn init(&mut self, subpass: &Subpass) {
        self.graphics.init(subpass.clone());
    }

    fn build_commands(&self, builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>, buffer: Vec<PositionedChar>) {
        let graphics = self.graphics.vk_graphics();

        let mapper = self.font_sheet.mapper();
        let sizer = self.font_sheet.sizer();
        let screen_size = self.target.dimensions();
        let screen_scale = glm::vec2(
            // 2.0 because Vulkan screen coords go from -1.0 to 1.0
            2.0 / screen_size.width() as f32,
            2.0 / screen_size.height() as f32,
        );

        let glyphs = buffer.into_iter()
            .filter_map(|layout_char| {
                if let Some(index) = mapper.map_char(layout_char.value) {
                    Some(GlyphInstance {
                        index: index as f32,
                        offset: [
                            layout_char.position.x * screen_scale.x - 1.0,
                            layout_char.position.y * screen_scale.y - 1.0,
                        ],
                        scale: [
                            sizer.width(layout_char.size) * screen_scale.x,
                            sizer.height(layout_char.size) * screen_scale.y,
                        ],
                        color: layout_char.color.into(),
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        if glyphs.is_empty() {
            // Nothing to render, we're done here
            return;
        }

        // TODO: Reuse the buffer somehow?
        let instance_buffer = Buffer::from_iter(
            self.target.device().memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::VERTEX_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_DEVICE
                    | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            glyphs,
        ).unwrap();
        let instance_buffer_len = instance_buffer.len() as u32;

        builder
            .bind_pipeline_graphics(graphics.clone()).unwrap()
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                graphics.layout().clone(),
                0,
                self.font_sampler.descriptor_set(),
            ).unwrap()
            .bind_vertex_buffers(0, (
                self.glyph_buffer.clone(),
                instance_buffer,
            )).unwrap()
            .draw(
                self.glyph_buffer.len() as u32,
                instance_buffer_len,
                0,
                0,
            ).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unicode_map_basic_latin() {
        let basic_latin_chars = format!(
            "{}{}{}",
            " !\"#$%&'()*+,-./0123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            "[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~",
            char::REPLACEMENT_CHARACTER,
        );
        let basic_latin_char_count = basic_latin_chars.chars().count();

        let mapper = UnicodeFontSheetMap::basic_latin();

        assert_eq!(mapper.supported_chars().collect::<String>(), basic_latin_chars);
        assert_eq!(mapper.count() as usize, basic_latin_char_count);
        assert_eq!(mapper.replacement(), Some((basic_latin_char_count - 1) as u32));
    }
}