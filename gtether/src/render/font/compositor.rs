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
//! # use std::sync::Arc;
//! # use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
//! use gtether::render::font::compositor::FontCompositor;
//! # use gtether::render::font::compositor::FontRenderer;
//! # use gtether::render::font::layout::TextLayout;
//! # use gtether::render::RenderTarget;
//! #
//! # let font_renderer: Box<dyn FontRenderer> = return;
//! # let builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer> = return;
//! # let layout_a: TextLayout = return;
//! # let layout_b: TextLayout = return;
//!
//! let font_compositor = FontCompositor::new(font_renderer);
//!
//! let mut font_pass = font_compositor.begin_pass(builder);
//!
//! font_pass
//!     .layout(&layout_a)
//!     .layout(&layout_b);
//!
//! font_pass.end_pass();
//! ```

use vulkano::command_buffer::{AutoCommandBufferBuilder, PrimaryAutoCommandBuffer};
use vulkano::render_pass::Subpass;

use crate::render::font::layout::{PositionedChar, TextLayout};

/// Render helper for a concrete font representation.
///
/// These helpers are intended to consume [PositionedChar]s from a [TextLayout], and render them
/// according to said layout.
pub trait FontRenderer: Send + Sync + 'static {
    /// Initialize this FontRenderer for a given subpass.
    ///
    /// Should be called exactly once.
    fn init(&mut self, subpass: &Subpass);

    /// Add the relevant render commands to a command buffer.
    fn build_commands(
        &self,
        builder: &mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>,
        buffer: Vec<PositionedChar>,
    );
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

    /// Initialize this compositor (and it's [FontRenderer]) with a given subpass.
    ///
    /// Should be called exactly once.
    pub fn init(&mut self, subpass: &Subpass) {
        self.renderer.init(subpass);
    }

    /// Begin a pass rendering a collection of [TextLayout]s.
    pub fn begin_pass<'a>(&'a self, builder: &'a mut AutoCommandBufferBuilder<PrimaryAutoCommandBuffer>) -> FontCompositorPass<'a> {
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
    pub fn end_pass(self) {
        let layout_chars = self.layouts.into_iter()
            .map(|layout| layout.iter_build())
            .flatten()
            .collect::<Vec<_>>();
        self.compositor.renderer.build_commands(self.builder, layout_chars);
    }
}