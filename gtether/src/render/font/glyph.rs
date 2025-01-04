use ab_glyph::{Font as _Font, OutlinedGlyph, ScaleFont as _ScaleFont};
use image::{DynamicImage, Rgba};
use std::collections::VecDeque;
use std::fmt;
use std::fmt::Formatter;
use std::sync::Arc;
use async_trait::async_trait;
use glm::TVec2;
use smol::io::AsyncReadExt;
use tracing::warn;

pub use ab_glyph::InvalidFont;

use crate::render::font::sheet::FontSheetMap;
use crate::render::font::{CharImgData, Font};
use crate::render::{RendererEventType, RendererHandle};
use crate::render::font::size::{FontSizer, ScaledFontSize};
use crate::resource::{ResourceLoadError, ResourceLoader, ResourceMut, ResourceReadData};

/// A glyph based font.
///
/// A font comprised of glyph data, loaded from file formats such as .otf/.ttf.
pub struct GlyphFont {
    inner: ab_glyph::FontArc,
}

impl GlyphFont {
    fn new(inner: ab_glyph::FontArc) -> Self {
        Self {
            inner,
        }
    }

    /// Attempt to load a [GlyphFont] from static data.
    ///
    /// Useful for loading fonts embedded with the application, such as with `include_bytes!`.
    ///
    /// Note: It is recommended to create fonts using resource loading via [GlyphFontLoader] instead.
    pub fn try_from_slice(data: &'static [u8]) -> Result<Self, InvalidFont> {
        Ok(Self::new(
            ab_glyph::FontArc::try_from_slice(data)?,
        ))
    }

    /// Attempt to load a [GlyphFont] from a vector of data.
    ///
    /// Useful for dynamically loading font data at runtime.
    ///
    /// Note: It is recommended to create fonts using resource loading via [GlyphFontLoader] instead.
    pub fn try_from_vec(data: Vec<u8>) -> Result<Self, InvalidFont> {
        Ok(Self::new(
            ab_glyph::FontArc::try_from_vec(data)?,
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

struct GlyphCharImgData {
    glyphs: VecDeque<Option<OutlinedGlyph>>,
    glyph_count: usize,
    img_size: glm::TVec2<u32>,
    img_center: glm::TVec2<u32>,
    sizer: Arc<GlyphFontSizer>,
}

impl GlyphCharImgData {
    fn new(font: ab_glyph::PxScaleFont<ab_glyph::FontArc>, mapper: &dyn FontSheetMap) -> Self {
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
                        warn!(?c, "Unexpected glyph missing outline");
                    }
                    None
                }
            }).collect::<VecDeque<_>>();
        let glyph_count = glyphs.len();

        let img_size = glm::vec2(
            (bounds_max.x - bounds_min.x).ceil() as u32,
            (bounds_max.y - bounds_min.y).ceil() as u32,
        );
        let img_center = glm::vec2(
            // bounds_min will either be (0, 0) or have components less than 0
            bounds_min.x.abs().ceil() as u32,
            bounds_min.y.abs().ceil() as u32,
        );

        let sizer = Arc::new(GlyphFontSizer::new(font, img_size));

        Self {
            glyphs,
            glyph_count,
            img_size,
            img_center,
            sizer,
        }
    }
}

impl Iterator for GlyphCharImgData {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let glyph = self.glyphs.pop_front()?;
        let mut image = DynamicImage::new_rgba8(self.img_size.x, self.img_size.y).to_rgba8();

        if let Some(glyph) = glyph {
            let bounds = glyph.px_bounds();
            let start = glm::vec2(
                // Do the math in floats as it can contain negative numbers
                (self.img_center.x as f32 + bounds.min.x.round()) as u32,
                (self.img_center.y as f32 + bounds.min.y.round()) as u32,
            );
            glyph.draw(|x, y, v| {
                let px = image.get_pixel_mut(start.x + x, start.y + y);
                *px = Rgba([
                    255,
                    255,
                    255,
                    px.0[3].saturating_add((v * 255.0) as u8),
                ]);
            });
        } // Else leave the image blank

        Some(image.into_raw())
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.glyph_count, Some(self.glyph_count))
    }
}

impl ExactSizeIterator for GlyphCharImgData {}

impl CharImgData for GlyphCharImgData {
    #[inline]
    fn img_size(&self) -> TVec2<u32> { self.img_size }

    #[inline]
    fn img_byte_size(&self) -> usize {
        self.img_size.x as usize * self.img_size.y as usize * 4
    }

    #[inline]
    fn sizer(&self) -> Arc<dyn FontSizer> { self.sizer.clone() }
}

impl Font for GlyphFont {
    fn char_img_data(&self, px_scale: f32, mapper: &dyn FontSheetMap) -> Box<dyn CharImgData> {
        Box::new(GlyphCharImgData::new(
            self.inner.clone().into_scaled(ab_glyph::PxScale::from(px_scale)),
            mapper,
        ))
    }
}

/// [ResourceLoader] for a [GlyphFont].
///
/// Resource updating will be synced with a provided [Renderer][rdr], so that updates only occur
/// between render passes.
///
/// [rdr]: RendererHandle
pub struct GlyphFontLoader {
    renderer: RendererHandle,
}

impl GlyphFontLoader {
    /// Create a new [GlyphFontLoader].
    #[inline]
    pub fn new(renderer: RendererHandle) -> Self { Self { renderer } }
}

#[async_trait]
impl ResourceLoader<dyn Font> for GlyphFontLoader {
    async fn load(&self, mut data: ResourceReadData) -> Result<Box<dyn Font>, ResourceLoadError> {
        let mut buffer = Vec::new();
        data.read_to_end(&mut buffer).await
            .map_err(ResourceLoadError::from_error)?;
        GlyphFont::try_from_vec(buffer)
            .map(|v| Box::new(v) as Box<dyn Font>)
            .map_err(ResourceLoadError::from_error)
    }

    async fn update(&self, resource: ResourceMut<dyn Font>, data: ResourceReadData)
                    -> Result<(), ResourceLoadError>
    {
        let new_value = self.load(data).await?;
        self.renderer.event_bus().register_once(RendererEventType::PostRender, move |_| {
            resource.replace(new_value);
        }).wait().await
            .map_err(ResourceLoadError::from_error)
    }
}