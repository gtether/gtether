//! General font and text render handling.
//!
//! In order to do any text rendering with the engine, a [Font] must first be created to work with.
//! The engine provides a [Font] trait as well as a few concrete font types, such as [GlyphFont] for
//! loading .otf or .ttf font files.
//!
//! [Font] traits themselves only represent the abstract details of a font however, and cannot
//! _directly_ be used for rendering. Instead, a [FontCompositor][fc] must be created from a
//! [FontRenderer][fr]. The [FontRenderer][fr] trait represents a particular way of rendering the
//! abstract [Font] into concrete images, and the [FontCompositor][fc] is in charge of managing and
//! rendering one or more [TextLayout][tl]s using that logic.
//!
//! The engine currently provides one implementation of [FontRenderer][fr] -
//! [FontSheetRenderer][fsr], which uses a [FontSheet][fs] created from a [Font].
//!
//! # Examples
//!
//! Load a font and create a compositor
//! ```
//! # use std::sync::Arc;
//! use gtether::render::font::{Font, GlyphFont};
//! use gtether::render::font::compositor::FontCompositor;
//! use gtether::render::font::sheet::{FontSheetRenderer, UnicodeFontSheetMap};
//! # use gtether::render::RenderTarget;
//! #
//! # let render_target: Arc<dyn RenderTarget> = return;
//! # let font_data: Vec<u8> = return;
//!
//! let font = Box::new(GlyphFont::try_from_vec(
//!     render_target.device(),
//!     font_data,
//! ).unwrap()) as Box<dyn Font>;
//!
//! let font_sheet = font.create_sheet(
//!     64.0,
//!     UnicodeFontSheetMap::basic_latin(),
//! );
//!
//! let font_compositor = FontCompositor::new(
//!     render_target.clone(),
//!     FontSheetRenderer::new(
//!         render_target.clone(),
//!         font_sheet.clone(),
//!     ),
//! );
//! ```
//!
//! Render a [TextLayout][tl] (generally done as part of a parent render handler)
//! ```
//! # use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
//! # use gtether::render::font::compositor::FontCompositor;
//! # use gtether::render::font::layout::TextLayout;
//! # let text_layout: TextLayout = return;
//! # let font_compositor: FontCompositor = return;
//! # let command_builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer> = return;
//! #
//! let mut pass = font_compositor.begin_pass(command_builder);
//! pass.layout(&text_layout);
//! pass.end_pass();
//! ```
//!
//! [fc]: compositor::FontCompositor
//! [fr]: compositor::FontRenderer
//! [fs]: sheet::FontSheet
//! [fsr]: sheet::FontSheetRenderer
//! [tl]: layout::TextLayout

use std::fmt;
use std::fmt::Formatter;
use std::sync::Arc;

use ab_glyph;
use ab_glyph::{Font as _Font, ScaleFont as _ScaleFont};
pub use ab_glyph::InvalidFont;
use image::{DynamicImage, Rgba};
use tracing::{event, Level};
use vulkano::{DeviceSize, Validated};
use vulkano::buffer::{Buffer, BufferCreateInfo, BufferUsage};
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage, CopyBufferToImageInfo, PrimaryCommandBufferAbstract};
use vulkano::format::Format;
use vulkano::image::{Image, ImageCreateInfo, ImageType, ImageUsage};
use vulkano::image::view::ImageView;
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter};
use vulkano::sync::GpuFuture;

use sheet::FontSheet;
use sheet::FontSheetMap;
use size::ScaledFontSize;

use crate::render::Device;
use crate::render::font::size::FontSizer;

pub mod compositor;
pub mod layout;
pub mod sheet;
pub mod size;

/// Representation of a loaded font.
///
/// An abstract representation of a font that can't be used directly. Must be used to generate
/// a concrete representation to render with, such as a [FontSheet] (generated via
/// [Font::create_sheet()]).
pub trait Font: Send + Sync + 'static {
    /// Create a concrete [FontSheet] for this [Font].
    ///
    /// The provided `px_scale` is the size in pixels of the generated per-glyph images of the font
    /// sheet, while `mapper` is used to determine what UTF character codes to include in the
    /// generated sheet.
    fn create_sheet(&self, px_scale: f32, mapper: Box<dyn FontSheetMap>) -> Arc<FontSheet>;
}

/// A glyph based font.
///
/// A font comprised of glyph data, loaded from file formats such as .otf/.ttf.
pub struct GlyphFont {
    inner: ab_glyph::FontArc,
    device: Arc<Device>,
}

impl GlyphFont {
    fn new(device: &Arc<Device>, inner: ab_glyph::FontArc) -> Self {
        Self {
            inner,
            device: device.clone(),
        }
    }

    /// Attempt to load a [GlyphFont] from static data.
    ///
    /// Useful for loading fonts embedded with the application, such as with `include_bytes!`.
    pub fn try_from_slice(device: &Arc<Device>, data: &'static [u8]) -> Result<Self, InvalidFont> {
        Ok(Self::new(
            device,
            ab_glyph::FontArc::try_from_slice(data)?,
        ))
    }

    /// Attempt to load a [GlyphFont] from a vector of data.
    ///
    /// Useful for dynamically loading font data at runtime.
    pub fn try_from_vec(device: &Arc<Device>, data: Vec<u8>) -> Result<Self, InvalidFont> {
        Ok(Self::new(
            device,
            ab_glyph::FontArc::try_from_vec(data)?,
        ))
    }
}

impl Font for GlyphFont {
    fn create_sheet(&self, px_scale: f32, mapper: Box<dyn FontSheetMap>) -> Arc<FontSheet> {
        let font = self.inner.clone().into_scaled(ab_glyph::PxScale::from(px_scale));

        let mut bounds_min = glm::vec2(0.0, 0.0);
        let mut bounds_max = glm::vec2(0.0, 0.0);
        let glyphs = mapper.supported_chars()
            .map(|c| {
                let glyph = font.scaled_glyph(c);
                if let Some(outlined) = font.outline_glyph(glyph) {
                    let bounds = outlined.px_bounds();
                    bounds_min = glm::min2(&bounds_min, &glm::vec2(bounds.min.x, bounds.min.y));
                    bounds_max = glm::max2(&bounds_max, &glm::vec2(bounds.max.x, bounds.max.y));
                    Some(outlined)
                } else {
                    if c != ' ' {
                        event!(Level::WARN, ?c, "Unexpected glyph missing outline");
                    }
                    None
                }
            }).collect::<Vec<_>>();

        let img_size = glm::vec2(
            (bounds_max.x - bounds_min.x).ceil() as u32,
            (bounds_max.y - bounds_min.y).ceil() as u32,
        );
        let img_center = glm::vec2(
            // bounds_min will either be (0, 0) or have components less than 0
            bounds_min.x.abs().ceil() as u32,
            bounds_min.y.abs().ceil() as u32,
        );

        let image = Image::new(
            self.device.memory_allocator().clone(),
            ImageCreateInfo {
                image_type: ImageType::Dim2d,
                format: Format::R8G8B8A8_SRGB,
                extent: [img_size.x, img_size.y, 1],
                array_layers: glyphs.len() as u32,
                usage: ImageUsage::TRANSFER_DST | ImageUsage::SAMPLED,
                ..Default::default()
            },
            AllocationCreateInfo::default(),
        ).unwrap();

        let img_byte_size = img_size.x * img_size.y * 4;

        let upload_buffer = Buffer::new_slice(
            self.device.memory_allocator().clone(),
            BufferCreateInfo {
                usage: BufferUsage::TRANSFER_SRC,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_HOST
                    | MemoryTypeFilter::HOST_SEQUENTIAL_WRITE,
                ..Default::default()
            },
            (img_byte_size * glyphs.len() as u32) as DeviceSize,
        ).unwrap();

        for (idx, glyph) in glyphs.into_iter().enumerate() {
            let mut glyph_image = DynamicImage::new_rgba8(img_size.x, img_size.y).to_rgba8();
            if let Some(glyph) = glyph {
                let bounds = glyph.px_bounds();
                let start = glm::vec2(
                    // Do the math in floats as it can contain negative numbers
                    (img_center.x as f32 + bounds.min.x.round()) as u32,
                    (img_center.y as f32 + bounds.min.y.round()) as u32,
                );
                glyph.draw(|x, y, v| {
                    let px = glyph_image.get_pixel_mut(start.x + x, start.y + y);
                    *px = Rgba([
                        255,
                        255,
                        255,
                        px.0[3].saturating_add((v * 255.0) as u8),
                    ]);
                });
            }

            let start_idx = idx * img_byte_size as usize;
            let end_idx = (idx + 1) * img_byte_size as usize;
            upload_buffer.write().unwrap()
                [start_idx..end_idx]
                .copy_from_slice(glyph_image.as_raw());
        }

        let mut uploads = AutoCommandBufferBuilder::primary(
            self.device.command_buffer_allocator().as_ref(),
            self.device.queue().queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        ).map_err(Validated::unwrap)
            .expect("Failed to allocate command builder");

        uploads.copy_buffer_to_image(CopyBufferToImageInfo::buffer_image(
            upload_buffer,
            image.clone(),
        )).unwrap();

        uploads
            .build().unwrap()
            .execute(self.device.queue().clone()).unwrap()
            .then_signal_fence_and_flush().unwrap()
            // TODO: Does this need a timeout?
            .wait(None).unwrap();

        let image_view = ImageView::new_default(image).unwrap();

        Arc::new(FontSheet::new(
            image_view,
            mapper,
            Arc::new(GlyphFontSizer::new(font, img_size)),
        ))
    }
}

struct GlyphFontSizer {
    font: ab_glyph::PxScaleFont<ab_glyph::FontArc>,
    base_size: glm::U32Vec2,
}

impl GlyphFontSizer {
    fn new(font: ab_glyph::PxScaleFont<ab_glyph::FontArc>, base_size: glm::U32Vec2) -> Self {
        Self {
            font,
            base_size,
        }
    }

    fn size_ratio(&self, size: ScaledFontSize) -> f32 {
        size.to_px() / self.base_size.y as f32
    }

    fn width_ratio(&self) -> f32 {
        self.base_size.x as f32 / self.base_size.y as f32
    }
}

impl fmt::Debug for GlyphFontSizer {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("GlyphFontSizer").finish_non_exhaustive()
    }
}

impl FontSizer for GlyphFontSizer {
    #[inline]
    fn height(&self, size: ScaledFontSize) -> f32 {
        size.to_px()
    }

    #[inline]
    fn width(&self, size: ScaledFontSize) -> f32 {
        size.to_px() * self.width_ratio()
    }

    #[inline]
    fn line_gap(&self, size: ScaledFontSize) -> f32 {
        self.font.line_gap() * self.size_ratio(size)
    }

    #[inline]
    fn h_advance(&self, size: ScaledFontSize, c: char) -> f32 {
        self.font.h_advance(self.font.glyph_id(c)) * self.size_ratio(size)
    }

    #[inline]
    fn kern(&self, size: ScaledFontSize, first: char, second: char) -> f32 {
        self.font.kern(self.font.glyph_id(first), self.font.glyph_id(second)) * self.size_ratio(size)
    }
}

