//! Render helpers for rendering layouted text.
//!
//! There are two types of render helpers in this module:
//!  * [FontRenderer]: basic helper used for rendering arbitrary [PositionedChar]s with a specific
//!      concrete font. Usually built from the relevant concrete font implemention.
//!  * [FontCompositor]: overall helper used for rendering full text layouts; builds on top of
//!      [FontRenderer].
//!
//! # Examples
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
//!     layout_a: Mutex<TextLayout>,
//!     layout_b: Mutex<TextLayout>,
//!     compositor: FontCompositor,
//! }
//!
//! impl EngineRenderHandler for MyTextRenderer {
//!     fn build_commands(
//!         &self,
//!         builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
//!     ) -> Result<(), Validated<VulkanoError>> {
//!         let layout_a = self.layout_a.lock().unwrap();
//!         let layout_b = self.layout_b.lock().unwrap();
//!
//!         let mut font_pass = self.compositor.begin_pass(builder);
//!
//!         font_pass
//!             .layout(&layout_a)
//!             .layout(&layout_b);
//!
//!         font_pass.end_pass()?;
//!
//!         Ok(())
//!     }
//! }
//! ```

use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::Validated;
use crate::render::font::layout::{PositionedChar, TextLayout};
use crate::render::VulkanoError;

/// Render helper for a concrete font representation.
///
/// These helpers are intended to consume [PositionedChar]s from a [TextLayout], and render them
/// according to said layout.
pub trait FontRenderer: Send + Sync + 'static {
    /// Add the relevant render commands to a command buffer.
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        buffer: Vec<PositionedChar>,
    ) -> Result<(), Validated<VulkanoError>>;
}

/// Top-level render helper for rendering fonts.
///
/// Takes references to multiple [TextLayout]s in a single-pass, and renders them all to a final
/// output.
pub struct FontCompositor {
    renderer: Box<dyn FontRenderer>,
}

impl FontCompositor {
    /// Create a new [FontCompositor] using a specific [FontRenderer], and targeting a specific
    /// [RenderTarget].
    pub fn new(renderer: Box<dyn FontRenderer>) -> Self {
        Self { renderer }
    }

    /// Begin a pass rendering a collection of [TextLayout]s.
    pub fn begin_pass<'a>(
        &'a self,
        builder: &'a mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> FontCompositorPass<'a> {
        FontCompositorPass::new(self, builder)
    }
}

/// Render pass for a collection of [TextLayout]s.
pub struct FontCompositorPass<'a> {
    compositor: &'a FontCompositor,
    builder: &'a mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    layouts: Vec<&'a TextLayout>,
}

impl<'a> FontCompositorPass<'a> {
    fn new(
        compositor: &'a FontCompositor,
        builder: &'a mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
    ) -> Self {
        Self {
            compositor,
            builder,
            layouts: Vec::new(),
        }
    }

    /// Add a [TextLayout] to this pass.
    pub fn layout(&mut self, layout: &'a TextLayout) -> &mut Self {
        self.layouts.push(layout);
        self
    }

    /// End this pass, and commit all rendering commands.
    pub fn end_pass(self) -> Result<(), Validated<VulkanoError>> {
        let layout_chars = self.layouts.into_iter()
            .map(|layout| layout.iter_build())
            .flatten()
            .collect::<Vec<_>>();
        self.compositor.renderer.build_commands(self.builder, layout_chars)
    }
}