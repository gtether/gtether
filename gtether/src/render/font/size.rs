//! Sizing and scaling utilities for font rendering.
//!
//! [FontSize] provides a consistent font size metric, while [ScaledFontSize] allows for concrete
//! calculation of pixel size for a given render scale.
//!
//! For complete text sizing needs, [FontSizer] is used to calculate all pixel dimensions for text
//! rendering.

use std::fmt;

/// Size to render a font at.
///
/// The following common units are available:
///  * Pt: Standard font pt scale, generally 1pt ~= 1/72 inches
///  * Px: Explicit pixel height
///
/// For conversion, a DPI of 96 is assumed, so 1pt == 96/72px.
///
/// Default font size is 12pt.
#[derive(Debug, Clone, Copy)]
pub enum FontSize {
    Pt(f32),
    Px(f32),
}

impl Default for FontSize {
    fn default() -> Self {
        FontSize::Pt(12.0)
    }
}

impl FontSize {
    /// Convert the current size to an equivalent using pt units.
    pub fn to_pt(&self) -> f32 {
        match self {
            FontSize::Pt(size) => size.clone(),
            FontSize::Px(size) => (size * 72.0) / 96.0,
        }
    }

    /// Convert the current size to an equivalent using px units.
    pub fn to_px(&self) -> f32 {
        match self {
            FontSize::Pt(size) => (size * 96.0) / 72.0,
            FontSize::Px(size) => size.clone(),
        }
    }

    /// Scale a [FontSize] using a given screen scale ratio.
    #[inline]
    pub fn scale(&self, scale: f32) -> ScaledFontSize {
        ScaledFontSize::new(self.clone(), scale)
    }
}

/// Scaled font size, used to calculate final pixel height.
#[derive(Debug, Clone, Copy)]
pub struct ScaledFontSize {
    size: FontSize,
    scale: f32,
}

impl ScaledFontSize {
    /// Scale a [FontSize] using a given screen scale ratio.
    pub fn new(size: FontSize, scale: f32) -> Self {
        Self {
            size,
            scale,
        }
    }

    /// The unscaled [FontSize].
    #[inline]
    pub fn size(&self) -> FontSize { self.size }

    /// The scale ratio this [ScaledFontSize] uses.
    #[inline]
    pub fn scale(&self) -> f32 { self.scale }

    /// Final pixel height after scaling is applied.
    #[inline]
    pub fn to_px(&self) -> f32 { self.size.to_px() * self.scale }
}

/// Utility trait used to apply scaled sizing operations during text rendering.
///
/// A [FontSizer] is capable of determining the exact pixel dimensions for rendering text on the
/// screen, as well as any gaps or offsets both horizontally and vertically in arranged text.
pub trait FontSizer: fmt::Debug + Send + Sync + 'static {
    /// Maximum pixel height needed to render a single glyph.
    fn height(&self, size: ScaledFontSize) -> f32;
    /// Maximum pixel width needed to render a single glyph.
    fn width(&self, size: ScaledFontSize) -> f32;
    /// Pixel width of buffer space to put between lines.
    fn line_gap(&self, size: ScaledFontSize) -> f32;

    /// Pixel width of a specific glyph.
    #[inline]
    fn h_advance(&self, size: ScaledFontSize, _c: char) -> f32 { self.width(size) }

    /// Kerning to put between two glyphs.
    #[inline]
    fn kern(&self, _size: ScaledFontSize, _first: char, _second: char) -> f32 { 0.0 }

    /// Pixel count to advance between lines; generally consists of max glyph height + line gap.
    #[inline]
    fn v_advance(&self, size: ScaledFontSize) -> f32 { self.height(size) + self.line_gap(size) }
}