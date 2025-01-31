//! General font and text render handling.
//!
//! In order to do any text rendering with the engine, a [Font] must first be created to work with.
//! The engine provides a [Font] trait as well as a few concrete font types, such as [GlyphFont][gf]
//! for loading .otf or .ttf font files.
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
//! # use vulkano::render_pass::Subpass;
//! use gtether::render::font::compositor::FontCompositor;
//! use gtether::render::font::glyph::GlyphFontLoader;
//! use gtether::render::font::sheet::{FontSheet, FontSheetRenderer, UnicodeFontSheetMap};
//! # use gtether::render::{RenderTarget, Renderer};
//! # use gtether::resource::manager::ResourceManager;
//! use gtether::resource::manager::LoadPriority;
//! #
//! # let resource_manager: Arc<ResourceManager> = return;
//! # let renderer: &Arc<Renderer> = return;
//! # let subpass: &Subpass = return;
//! # let render_target: Arc<dyn RenderTarget> = return;
//! # let font_data: Vec<u8> = return;
//!
//! let font = resource_manager.get_or_load(
//!     "my_font",
//!     GlyphFontLoader::new(renderer.clone()),
//!     LoadPriority::Immediate
//! ).wait().unwrap();
//!
//! let font_sheet = FontSheet::from_font(
//!     &font,
//!     renderer.clone(),
//!     64.0,
//!     Arc::new(UnicodeFontSheetMap::basic_latin()),
//! ).unwrap();
//!
//! let font_compositor = FontCompositor::new(
//!     FontSheetRenderer::new(
//!         renderer,
//!         subpass,
//!         font_sheet.clone(),
//!     ),
//! );
//! ```
//!
//! Render a [TextLayout][tl] (generally done as part of a parent render handler)
//! ```
//! use std::sync::Mutex;
//! use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
//! use vulkano::Validated;
//! use gtether::render::font::compositor::FontCompositor;
//! use gtether::render::font::layout::TextLayout;
//! use gtether::render::render_pass::EngineRenderHandler;
//! use gtether::render::VulkanoError;
//!
//! struct MyTextRenderer {
//!     layout: Mutex<TextLayout>,
//!     compositor: FontCompositor,
//! }
//!
//! impl EngineRenderHandler for MyTextRenderer {
//!     fn build_commands(
//!         &self,
//!         builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
//!     ) -> Result<(), Validated<VulkanoError>> {
//!         let layout = self.layout.lock().unwrap();
//!         let mut font_pass = self.compositor.begin_pass(builder);
//!         font_pass.layout(&layout);
//!         font_pass.end_pass()?;
//!         Ok(())
//!     }
//! }
//! ```
//!
//! [gf]: glyph::GlyphFont
//! [fc]: compositor::FontCompositor
//! [fr]: compositor::FontRenderer
//! [fs]: sheet::FontSheet
//! [fsr]: sheet::FontSheetRenderer
//! [tl]: layout::TextLayout

use std::sync::Arc;

pub mod compositor;
pub mod layout;
pub mod sheet;
pub mod size;
pub mod glyph;

use sheet::FontSheetMap;
use size::FontSizer;

/// Iterator that generates raw image data for [Font] characters.
///
/// The iterator itself contains some basic metadata that should be retrieved before starting
/// iteration, such as img sizes.
pub trait CharImgData: ExactSizeIterator<Item=Vec<u8>> + Send {
    /// The width and height in pixels of an individual character image.
    fn img_size(&self) -> glm::TVec2<u32>;

    /// The size in bytes required to contain the full raw data of a single character image.
    ///
    /// Currently, this assumes each pixel is 4 bytes, one each for RGBA.
    fn img_byte_size(&self) -> usize;

    /// The [FontSizer] for this character data.
    fn sizer(&self) -> Arc<dyn FontSizer>;
}

/// Representation of a loaded font.
///
/// An abstract representation of a font that can't be used directly. Must be used to generate
/// a concrete representation to render with, such as a [FontSheet][fs].
///
/// [fs]: sheet::FontSheet
pub trait Font: Send + Sync + 'static {
    /// Generate raw image data for a set of characters defined by the given [FontSheetMap].
    fn char_img_data(&self, px_scale: f32, mapper: &dyn FontSheetMap) -> Box<dyn CharImgData>;
}