//! Text layouting.
//!
//! [TextLayout] provides a comprehensive text layouting structure, to handle most text rendering
//! needs. It allows extensive configuration, from the layout canvas size to alignment or formatting
//! of text.
//!
//! Note that [TextLayout] is actually rendering agnostic, and can be used to build positioned text
//! data that a proper renderer can further process to create image output.
//!
//! # Examples
//!
//! Simple usage and building positioned text
//! ```
//! extern crate nalgebra_glm as glm;
//!
//! # use std::sync::Arc;
//! use gtether::render::font::layout::{LayoutAlignment, LayoutHorizontalAlignment, TextLayout, TextLayoutCreateInfo};
//! # use gtether::render::font::size::FontSizer;
//! #
//! # let font_sizer: Arc<dyn FontSizer> = return;
//!
//! let mut layout = TextLayout::new(
//!     font_sizer.clone(),
//!     TextLayoutCreateInfo {
//!         canvas_offset: glm::vec2(0.0, 0.0),
//!         canvas_size: Some(glm::vec2(100.0, 50.0)),
//!         alignment: LayoutAlignment {
//!             horizontal: LayoutHorizontalAlignment::Center,
//!             ..Default::default()
//!         },
//!         word_wrap: true,
//!         ..Default::default()
//!     },
//!     1.0,
//! );
//!
//! layout
//!     .text("Hello world!")
//!     .newline()
//!     .color(glm::vec3(0.0, 0.0, 1.0))
//!     .text("Hello world in blue!");
//!
//! let positioned_text = layout.iter_build().collect::<Vec<_>>();
//! ```
//!
//! Create a [TextLayout] from a [RenderTarget], with the canvas size defaulting to the
//! [RenderTarget]'s dimensions.
//! ```
//! # use std::sync::Arc;
//! use gtether::render::font::layout::{LayoutAlignment, LayoutVerticalAlignment, TextLayout, TextLayoutCreateInfo};
//! # use gtether::render::font::size::FontSizer;
//! # use gtether::render::RenderTarget;
//! #
//! # let render_target: Arc<dyn RenderTarget> = return;
//! # let font_sizer: Arc<dyn FontSizer> = return;
//!
//! let mut layout = TextLayout::for_render_target(
//!     &render_target,
//!     font_sizer.clone(),
//!     TextLayoutCreateInfo {
//!         alignment: LayoutAlignment {
//!             vertical: LayoutVerticalAlignment::Bottom,
//!             ..Default::default()
//!         },
//!         ..Default::default()
//!     }
//! );
//! ```

use std::cell::OnceCell;
use std::cmp::min;
use std::fmt;
use std::fmt::{Display, Formatter, Write};
use std::slice;
use std::sync::Arc;
use parking_lot::RwLock;
use crate::NonExhaustive;
use crate::render::font::size::{FontSize, FontSizer, ScaledFontSize};
use crate::render::RenderTarget;

/// Horizontal text alignment settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutHorizontalAlignment {
    /// Align text to the left.
    Left,
    /// Align text in the center.
    Center,
    /// Align text to the right.
    Right,
}

impl Default for LayoutHorizontalAlignment {
    #[inline]
    fn default() -> Self { LayoutHorizontalAlignment::Left }
}

impl LayoutHorizontalAlignment {
    /// Calculate the offset from a left-aligned coordinate system for this alignment.
    ///
    /// The `outer` value is the width of the canvas to align inside, and the `inner` value is the
    /// width of the text that needs to be aligned.
    pub fn offset(&self, inner: f32, outer: f32) -> f32 {
        match self {
            LayoutHorizontalAlignment::Left => 0.0,
            LayoutHorizontalAlignment::Center => (outer - inner) / 2.0,
            LayoutHorizontalAlignment::Right => outer - inner,
        }
    }
}

/// Vertical text alignment settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutVerticalAlignment {
    /// Align text to the top.
    Top,
    /// Align text to the center.
    Center,
    /// Align text to the bottom.
    Bottom,
}

impl Default for LayoutVerticalAlignment {
    #[inline]
    fn default() -> Self { LayoutVerticalAlignment::Top }
}

impl LayoutVerticalAlignment {
    /// Calculate the offset from a top-aligned coordinate system for this alignment.
    ///
    /// The `outer` value is the height of the canvas to align inside, and the `inner` value is the
    /// height of the text that needs to be aligned.
    pub fn offset(&self, inner: f32, outer: f32) -> f32 {
        match self {
            LayoutVerticalAlignment::Top => 0.0,
            LayoutVerticalAlignment::Center => (outer - inner) / 2.0,
            LayoutVerticalAlignment::Bottom => outer - inner,
        }
    }
}

/// Comprehensive alignment settings for both horizontal and vertical alignment.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LayoutAlignment {
    pub horizontal: LayoutHorizontalAlignment,
    pub vertical: LayoutVerticalAlignment,
}

impl LayoutAlignment {
    /// Calculate the offset from a top-left-aligned coordinate system for these alignments.
    ///
    /// The `outer` value is the size of the canvas to align inside, and the `inner` value is the
    /// size of the text that needs to be aligned.
    #[inline]
    pub fn offset(&self, inner: &glm::TVec2<f32>, outer: &glm::TVec2<f32>) -> glm::TVec2<f32> {
        glm::vec2(
            self.horizontal.offset(inner.x, outer.x),
            self.vertical.offset(inner.y, outer.y),
        )
    }
}

/// Character data that has been positioned according to a [TextLayout].
#[derive(Debug, Clone)]
pub struct PositionedChar {
    /// Actual `char` value.
    pub value: char,
    /// Position of the character on the final image.
    pub position: glm::TVec2<f32>,
    /// Font size of the character
    pub size: ScaledFontSize,
    /// Color of the character
    pub color: glm::TVec3<f32>,
}

/// Canvas size and offset that a [TextLayout] uses.
#[derive(Debug, Clone, Copy)]
pub struct TextLayoutCanvas {
    /// Offset in px from the render target's origin that the [TextLayout] is located at.
    pub offset: glm::TVec2<f32>,
    /// Width/height in px of the [TextLayout].
    pub size: glm::TVec2<f32>,
}

/// Formatting information associated with a [TextLayout].
#[derive(Debug)]
pub struct TextLayoutInfo {
    sizer: Arc<dyn FontSizer>,
    canvas: RwLock<TextLayoutCanvas>,
    max_line_count: Option<usize>,
    size: ScaledFontSize,
    alignment: LayoutAlignment,
    word_wrap: bool,
}

impl TextLayoutInfo {
    /// Canvas size and offset that the [TextLayout] uses.
    #[inline]
    pub fn canvas(&self) -> TextLayoutCanvas { self.canvas.read().clone() }

    /// Maximum count of logical lines allowed in this [TextLayout].
    ///
    /// If None, then there is no maximum.
    #[inline]
    pub fn max_line_count(&self) -> Option<usize> { self.max_line_count }

    /// [Font size](ScaledFontSize) of text in this [TextLayout].
    #[inline]
    pub fn size(&self) -> ScaledFontSize { self.size }

    /// Horizontal and vertical [alignment](LayoutAlignment) of text in this [TextLayout].
    #[inline]
    pub fn alignment(&self) -> LayoutAlignment { self.alignment }

    /// Whether text word wraps when it is wider than the layout canvas width.
    #[inline]
    pub fn word_wrap(&self) -> bool { self.word_wrap }
}

/// Information needed to create a [TextLayout].
///
/// Create via default and use update syntax;
/// ```
/// use gtether::render::font::layout::TextLayoutCreateInfo;
/// let create_info = TextLayoutCreateInfo {
///     // Anything you need to specify goes here...
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone)]
pub struct TextLayoutCreateInfo {
    /// Offset of the layout's canvas within the total image. Default is (0, 0).
    pub canvas_offset: glm::TVec2<f32>,
    /// Size of the layout's canvas. Default is None, must be overridden.
    pub canvas_size: Option<glm::TVec2<f32>>,
    /// Optional maximum count of lines in this layout. Default is None.
    pub max_line_count: Option<usize>,
    /// [FontSize] for this layout. Default is [FontSize::default()].
    pub size: FontSize,
    /// Text alignment to use for this layout. Default is [LayoutAlignment::default()].
    pub alignment: LayoutAlignment,
    /// Whether this layout word wraps lines. Default is false.
    pub word_wrap: bool,
    pub _ne: NonExhaustive,
}

impl Default for TextLayoutCreateInfo {
    fn default() -> Self {
        Self {
            canvas_offset: glm::vec2(0.0, 0.0),
            canvas_size: None,
            max_line_count: None,
            size: FontSize::default(),
            alignment: LayoutAlignment::default(),
            word_wrap: false,
            _ne: NonExhaustive(()),
        }
    }
}

impl TextLayoutCreateInfo {
    fn into_info(self, sizer: Arc<dyn FontSizer>, scale: f32) -> TextLayoutInfo {
        TextLayoutInfo {
            sizer,
            canvas: RwLock::new(TextLayoutCanvas {
                offset: self.canvas_offset,
                size: self.canvas_size
                    .expect("TextLayoutCreateInfo::canvas_size is required to be Some()"),
            }),
            max_line_count: self.max_line_count,
            size: self.size.scale(scale),
            alignment: self.alignment,
            word_wrap: self.word_wrap,
        }
    }
}

/// Character with additional formatting attached, such as color.
#[derive(Debug, Clone, Copy)]
pub struct FormattedChar {
    pub value: char,
    width: f32,
    // TODO: Replace 'color' with 'format' struct when more formatting options (like italic/bold)
    //  are required
    pub color: glm::TVec3<f32>
}

impl FormattedChar {
    fn new(info: &TextLayoutInfo, value: char, color: glm::TVec3<f32>) -> Self {
        FormattedChar {
            value,
            width: info.sizer.h_advance(info.size, value),
            color,
        }
    }

    /// Actual `char` value.
    #[inline]
    pub fn value(&self) -> char { self.value }

    /// Width in px of this character's glyph.
    #[inline]
    pub fn width(&self) -> f32 { self.width }

    /// Color of this character.
    #[inline]
    pub fn color(&self) -> glm::TVec3<f32> { self.color }
}

/// [FormattedChar] with additional line positioning data.
#[derive(Debug, Clone, Copy)]
pub struct LineChar {
    inner: FormattedChar,
    x: f32,
    line_no: usize,
}

impl LineChar {
    fn new(info: &TextLayoutInfo, value: char, color: glm::TVec3<f32>) -> Self {
        LineChar {
            inner: FormattedChar::new(info, value, color),
            x: 0.0,
            line_no: 0,
        }
    }

    /// Inner [FormattedChar] value.
    #[inline]
    pub fn value(&self) -> &FormattedChar { &self.inner }

    /// Relative x-position of this character in the line.
    #[inline]
    pub fn x(&self) -> f32 { self.x }

    /// Line number of this character (0-indexed).
    #[inline]
    pub fn line_no(&self) -> usize { self.line_no }
}

impl From<FormattedChar> for LineChar {
    fn from(value: FormattedChar) -> Self {
        Self {
            inner: value,
            x: 0.0,
            line_no: 0,
        }
    }
}

impl From<&LineChar> for LineChar {
    fn from(value: &LineChar) -> Self { value.clone() }
}

#[derive(Debug, Clone, Copy)]
struct LineSegment {
    start: usize,
    end: usize,
}

impl LineSegment {
    fn new(start: usize, end: usize) -> Self {
        assert!(start <= end);
        Self {
            start,
            end,
        }
    }
}

/// Iterator over the physical portion of a line.
///
/// Yields individual characters.
pub struct PhysicalLineIter<'a> {
    chars: &'a [LineChar],
    idx: usize,
    r_idx: usize,
}

impl<'a> PhysicalLineIter<'a> {
    fn new(chars: &'a [LineChar]) -> Self {
        Self {
            chars,
            idx: 0,
            r_idx: chars.len(),
        }
    }

    /// Width in px of this physical line.
    pub fn width(&self) -> f32 {
        match self.chars.last() {
            Some(last) => last.x + last.inner.width,
            None => 0.0,
        }
    }
}

impl<'a> Iterator for PhysicalLineIter<'a> {
    type Item = &'a LineChar;

    fn next(&mut self) -> Option<Self::Item> {
        if self.len() > 0 {
            let r = self.chars.get(self.idx);
            self.idx += 1;
            r
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let size = self.r_idx - self.idx;
        (size, Some(size))
    }
}

impl ExactSizeIterator for PhysicalLineIter<'_> {}

impl DoubleEndedIterator for PhysicalLineIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.len() > 0 {
            self.r_idx -= 1;
            self.chars.get(self.r_idx)
        } else {
            None
        }
    }
}

/// Iterator over a full logical line.
///
/// Yields physical sub-sections of a line. If there is no word wrap, only one physical line will
/// be yield. If there is word wrap, multiple physical lines may be yield depending on if the line
/// needs to wrap.
pub struct LogicalLineIter<'a> {
    chars: &'a Vec<LineChar>,
    iter: slice::Iter<'a, LineSegment>,
}

impl<'a> LogicalLineIter<'a> {
    fn phys_line_iter(&self, segment: &LineSegment) -> PhysicalLineIter<'a> {
        PhysicalLineIter::new(&self.chars[segment.start..segment.end])
    }
}

impl<'a> Iterator for LogicalLineIter<'a> {
    type Item = PhysicalLineIter<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|segment| self.phys_line_iter(segment))
    }

    fn size_hint(&self) -> (usize, Option<usize>) { self.iter.size_hint() }
}

impl ExactSizeIterator for LogicalLineIter<'_> {}

impl DoubleEndedIterator for LogicalLineIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.iter.next_back().map(|segment| self.phys_line_iter(segment))
    }
}

/// Full [LogicalLine] of a text layout.
///
/// Represents an individual line of a text layout. Depending on whether word wrap is applied, this
/// logical line may be comprised of more than one physical line.
#[derive(Debug)]
pub struct LogicalLine {
    chars: Vec<LineChar>,
    segments: Vec<LineSegment>,
    info: Arc<TextLayoutInfo>,
    cache_width: OnceCell<f32>,
    cache_height: OnceCell<f32>,
}

impl LogicalLine {
    fn new(info: Arc<TextLayoutInfo>) -> Self {
        Self {
            chars: vec![],
            segments: vec![LineSegment::new(0, 0)],
            info,
            cache_width: OnceCell::new(),
            cache_height: OnceCell::new(),
        }
    }

    fn for_buffer<T>(info: Arc<TextLayoutInfo>, buffer: T) -> Self
    where
        T: IntoIterator,
        T::Item: Into<LineChar>,
    {
        let chars = buffer.into_iter()
            .map(T::Item::into)
            .collect();
        let mut line = Self {
            chars,
            segments: vec![],
            info,
            cache_width: OnceCell::new(),
            cache_height: OnceCell::new(),
        };
        line.adjust_trailing(0);
        line
    }

    fn for_string(info: Arc<TextLayoutInfo>, val: &str, color: glm::TVec3<f32>) -> Self {
        Self::for_buffer(
            info.clone(),
            val.chars().map(|c| FormattedChar::new(&info, c, color)),
        )
    }

    /// Iterate over the physical lines within this [LogicalLine].
    #[inline]
    pub fn physical_lines(&self) -> LogicalLineIter<'_> {
        LogicalLineIter {
            chars: &self.chars,
            iter: self.segments.iter(),
        }
    }

    /// Width in pixels of this [LogicalLine].
    ///
    /// If there are multiple physical lines, this is equivalent to the maximum width of physical
    /// lines.
    #[inline]
    pub fn width(&self) -> f32 {
        *self.cache_width.get_or_init(|| {
            self.physical_lines()
                .map(|phys_line| phys_line.width())
                .fold(0.0, f32::max)
        })
    }

    /// Height in pixels of this [LogicalLine].
    ///
    /// If there are multiple physical lines, this includes the combined height + line gap of all
    /// physical lines.
    #[inline]
    pub fn height(&self) -> f32 {
        *self.cache_height.get_or_init(|| {
            self.info.sizer.v_advance(self.info.size) * (self.physical_lines().len() - 1) as f32
                + self.info.sizer.height(self.info.size)
        })
    }

    /// Count of characters in this [LogicalLine].
    #[inline]
    pub fn len(&self) -> usize { self.chars.len() }

    /// Count of physical lines in this [LogicalLine].
    #[inline]
    pub fn line_count(&self) -> usize { self.segments.len() }

    fn move_cursor_up(&self, cursor: &mut glm::TVec2<usize>) -> Option<f32> {
        let curr = &self.chars[cursor.x];
        if curr.line_no > 0 {
            // Move into the previous physical line
            cursor.x = self.index_for_x(curr.x, curr.line_no - 1);
            None
        } else if cursor.y > 0 {
            // Move into the previous logical line
            cursor.x = 0;
            cursor.y -= 1;
            Some(curr.x)
        } else {
            // At the very top already
            None
        }
    }

    fn move_cursor_down(&self, cursor: &mut glm::TVec2<usize>, line_max: usize) -> Option<f32> {
        let curr = &self.chars[cursor.x];
        if curr.line_no < (self.line_count() - 1) {
            // Move into the next physical line
            cursor.x = self.index_for_x(curr.x, curr.line_no + 1);
            None
        } else if cursor.y < (line_max - 1) {
            // Move into the next logical line
            cursor.x = 0;
            cursor.y += 1;
            Some(curr.x)
        } else {
            // At the very bottom already
            None
        }
    }

    fn index_for_x(&self, x: f32, line_no: usize) -> usize {
        let phys_line = self.physical_lines().nth(line_no).unwrap();

        let mut prev: Option<(usize, &LineChar)> = None;
        let mut curr: Option<(usize, &LineChar)> = None;
        for (idx, layout_char) in phys_line.enumerate() {
            if layout_char.x > x {
                curr = Some((idx, layout_char));
                break;
            } else {
                prev = Some((idx, layout_char));
            }
        }

        let local_idx = match (prev, curr) {
            (None, None) => 0,
            (None, Some((idx, curr))) => {
                if x < (curr.x - x) {
                    0
                } else {
                    idx
                }
            },
            (Some((idx, _prev)), None) => idx,
            (Some((idx_prev, prev)), Some((idx_curr, curr))) => {
                if (x - prev.x) < (curr.x - x) {
                    idx_prev
                } else {
                    idx_curr
                }
            },
        };

        self.segments[line_no].start + local_idx
    }

    fn adjust_trailing(&mut self, idx: usize) {
        let canvas_size = self.info.canvas.read().size;

        let (mut prev_char, mut x, mut line_no) = if idx > 0 {
            let prev = &self.chars[idx - 1];
            (Some(prev.inner.value), prev.x + prev.inner.width, prev.line_no)
        } else {
            (None, 0.0, 0)
        };

        let mut line_start = match self.segments.get(line_no) {
            Some(line) => line.start,
            None => 0,
        };
        self.segments.truncate(line_no);

        for (local_idx, line_char) in self.chars[idx..].iter_mut().enumerate() {
            let curr_idx = idx + local_idx;
            let mut kern = match prev_char {
                Some(prev_char) => self.info.sizer.kern(self.info.size, prev_char, line_char.inner.value),
                None => 0.0,
            };
            let mut new_x = x + kern + line_char.inner.width;

            // Word-wrap rules:
            //  1. The new line end x-pos would be beyond the canvas size
            //  2. The current x-pos is not zero (i.e. the first char in the line); this ensures at
            //      at least one character per line
            // TODO: Word-wrap by whitespace instead of by character
            if self.info.word_wrap && new_x > canvas_size.x && x > 0.0 {
                line_no += 1;
                self.segments.push(LineSegment::new(line_start, curr_idx));
                line_start = curr_idx;
                x = 0.0;
                kern = 0.0;
                new_x = line_char.inner.width;
                prev_char = None;
            } else {
                prev_char = Some(line_char.inner.value);
            }

            line_char.x = x + kern;
            line_char.line_no = line_no;

            x = new_x;
        }
        self.segments.push(LineSegment::new(line_start, self.chars.len()));

        self.invalidate_size_caches();
    }

    fn invalidate_size_caches(&mut self) {
        self.cache_width.take();
        self.cache_height.take();
    }

    // TODO: Make insert operations public?
    fn insert(&mut self, idx: usize, val: &str, color: glm::TVec3<f32>) {
        for (i_c, c) in val.chars().enumerate() {
            self.chars.insert(idx + i_c, LineChar::new(&self.info, c, color));
        }
        self.adjust_trailing(idx);
    }

    fn insert_buffer<T>(&mut self, idx: usize, val: T) -> usize
    where
        T: IntoIterator,
        T::Item: Into<LineChar>
    {
        let mut count_c = 0;
        for (i_c, c) in val.into_iter().enumerate() {
            self.chars.insert(idx + i_c, c.into());
            count_c += 1;
        }
        self.adjust_trailing(idx);
        count_c
    }

    fn delete(&mut self, idx: usize) {
        self.chars.remove(idx);
        self.adjust_trailing(idx);
    }

    fn split_off(&mut self, info: Arc<TextLayoutInfo>, idx: usize) -> Self {
        let remainder = Self::for_buffer(info, self.chars.split_off(idx));
        self.adjust_trailing(idx);
        remainder
    }

    /// Create an iterator that builds [PositionedChar]s as it iterates.
    #[inline]
    pub fn iter_build(&self, offset: glm::TVec2<f32>) -> LayoutLineBuildIter<'_> {
        LayoutLineBuildIter::new(self, offset)
    }
}

impl Display for LogicalLine {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for c in &self.chars {
            f.write_char(c.inner.value)?
        }
        Ok(())
    }
}

/// Iterator that builds [PositionedChar]s.
pub struct LayoutLineBuildIter<'a> {
    line: &'a LogicalLine,
    logical_iter: LogicalLineIter<'a>,
    physical_iter: Option<PhysicalLineIter<'a>>,
    logical_offset: glm::TVec2<f32>,
    physical_offset: glm::TVec2<f32>,
}

impl<'a> LayoutLineBuildIter<'a> {
    fn new(line: &'a LogicalLine, logical_offset: glm::TVec2<f32>) -> Self {
        let canvas_size = line.info.canvas.read().size;
        let mut logical_iter = line.physical_lines();
        let physical_iter = logical_iter.next();
        let physical_offset = match &physical_iter {
            Some(p_line) => glm::vec2(
                line.info.alignment.horizontal.offset(p_line.width(), canvas_size.x),
                0.0,
            ),
            None => glm::vec2(0.0, 0.0),
        };
        Self {
            line,
            logical_iter,
            physical_iter,
            logical_offset,
            physical_offset,
        }
    }
}

impl Iterator for LayoutLineBuildIter<'_> {
    type Item = PositionedChar;

    fn next(&mut self) -> Option<Self::Item> {
        let info = &self.line.info;
        let (canvas_offset, canvas_size) = {
            let canvas = info.canvas.read();
            (canvas.offset, canvas.size)
        };

        let maybe_c = self.physical_iter.as_mut()?.next();
        let c = match maybe_c {
            Some(c) => c,
            None => {
                self.physical_iter = self.logical_iter.next();
                self.physical_offset = glm::vec2(
                    match &self.physical_iter {
                        Some(p_line) => info.alignment.horizontal.offset(
                            p_line.width(),
                            canvas_size.x,
                        ),
                        None => 0.0,
                    },
                    self.physical_offset.y + info.sizer.v_advance(info.size),
                );
                self.physical_iter.as_mut()?.next()?
            }
        };

        Some(PositionedChar {
            value: c.inner.value,
            position: canvas_offset + self.logical_offset + self.physical_offset + glm::vec2(c.x, 0.0),
            size: info.size,
            color: c.inner.color,
        })
    }
}

/// Layout to position text inside.
///
/// Each layout represents a specific canvas size, which is a subsection of the total image the
/// layout exists in.
///
/// There is some formatting that is applied to an entire layout, and some formatting that can be
/// applied to specific characters. Layout formatting includes:
///  * [Font][f]/[FontSize]
///  * [LayoutAlignment]
///  * word wrap
///
/// Per-character formatting includes:
///  * color
///
/// # Sizing and alignment
///
/// All text is positioned within the layout according to the given [LayoutAlignment], both
/// horizontally and vertically. If the text extends outside the layout however - e.g. when word
/// wrap isn't enabled or there are too many lines of text - it will continue to be rendered beyond
/// the layout's canvas.
///
/// For example, if the text is bottom aligned but there are more lines than will fit in the layout,
/// the earlier lines will be rendered above the top of the layout (but no lines will be rendered
/// below the bottom of the layout).
///
/// [f]: crate::render::font::Font
#[derive(Debug)]
pub struct TextLayout {
    info: Arc<TextLayoutInfo>,
    lines: Vec<LogicalLine>,
    cursor: glm::TVec2<usize>,
    color: glm::TVec3<f32>,
    cache_width: OnceCell<f32>,
    cache_height: OnceCell<f32>,
}

// TODO: Add additional formatting blocks for e.g. underlining or highlighting
// TODO: Support clipping any glyphs/blocks that extend pass the canvas
impl TextLayout {
    /// Create a new [TextLayout] using a specific [FontSizer], layout info, and display scale.
    pub fn new(
        sizer: Arc<dyn FontSizer>,
        create_info: TextLayoutCreateInfo,
        scale: f32,
    ) -> Self {
        let mut layout = Self {
            info: Arc::new(create_info.into_info(sizer, scale)),
            lines: vec![],
            cursor: glm::vec2(0, 0),
            color: glm::vec3(1.0, 1.0, 1.0),
            cache_width: OnceCell::new(),
            cache_height: OnceCell::new(),
        };
        layout.lines.push(LogicalLine::new(layout.info.clone()));
        layout
    }

    /// Create a new [TextLayout] using a specific [FontSizer] and layout info, with some portions
    /// filled from a [RenderTarget].
    ///
    /// In particular, the following is configured:
    ///  * The scale factor is pulled from the [RenderTarget]
    ///  * If a canvas size is not provided, it will default to the [RenderTarget]'s dimensions
    pub fn for_render_target(
        target: &Arc<dyn RenderTarget>,
        sizer: Arc<dyn FontSizer>,
        mut create_info: TextLayoutCreateInfo,
    ) -> Self {
        if create_info.canvas_size.is_none() {
            create_info.canvas_size = Some(target.dimensions().into());
        }
        Self::new(sizer, create_info, target.scale_factor() as f32)
    }

    fn invalidate_size_caches(&mut self) {
        self.cache_width.take();
        self.cache_height.take();
    }

    /// Current (x, y) text cursor position.
    #[inline]
    pub fn cursor(&self) -> &glm::TVec2<usize> { &self.cursor }

    /// Iterator over the [LogicalLine]s of this layout.
    #[inline]
    pub fn lines(&self) -> slice::Iter<'_, LogicalLine> {
        self.lines.iter()
    }

    /// Set the color used for adding new text with.
    ///
    /// Any text added after this method call will use the new color.
    pub fn color(&mut self, color: glm::TVec3<f32>) -> &mut Self {
        self.color = color;
        self
    }

    /// Move the cursor to the specified (x, y) position.
    ///
    /// If the position is out of bounds, will clamp it inside the appropriate bounds.
    pub fn move_cursor_to(&mut self, pos: glm::TVec2<usize>) -> &mut Self {
        self.cursor.y = min(pos.y, self.lines.len());
        self.cursor.x = min(pos.x, self.lines[self.cursor.y].chars.len());
        self
    }

    /// Move the cursor left one position.
    ///
    /// If the cursor is at the beginning of a line, will wrap around to the end of the previous
    /// line.
    ///
    /// If the cursor is at the beginning of the layout, nothing happens.
    pub fn move_cursor_left(&mut self) -> &mut Self {
        if self.cursor.x == 0 {
            if self.cursor.y > 0 {
                self.cursor.y -= 1;
                self.cursor.x = self.lines[self.cursor.y].chars.len();
            }
        } else {
            self.cursor.x -= 1;
        }
        self
    }

    /// Move the cursor right one position.
    ///
    /// If the cursor is at the end of a line, will wrap around to the beginning of the next line.
    ///
    /// If the cursor is at the end of the layout, nothing happens.
    pub fn move_cursor_right(&mut self) -> &mut Self {
        if self.cursor.x >= self.lines[self.cursor.y].chars.len() {
            if self.cursor.y < self.lines.len() - 1 {
                self.cursor.y += 1;
                self.cursor.x = 0;
            }
        } else {
            self.cursor.x += 1;
        }
        self
    }

    /// Move the cursor up one line.
    ///
    /// If the cursor is at the top line, nothing happens.
    pub fn move_cursor_up(&mut self) -> &mut Self {
        let overflow_x = self.lines[self.cursor.y].move_cursor_up(&mut self.cursor);
        if let Some(overflow_x) = overflow_x {
            let line = &self.lines[self.cursor.y];
            self.cursor.x = line.index_for_x(overflow_x, line.line_count() - 1);
        }
        self
    }

    /// Move the cursor down one line.
    ///
    /// If the cursor is at the bottom line, nothing happens.
    pub fn move_cursor_down(&mut self) -> &mut Self {
        let overflow_x = self.lines[self.cursor.y]
            .move_cursor_down(&mut self.cursor, self.lines.len());
        if let Some(overflow_x) = overflow_x {
            let line = &self.lines[self.cursor.y];
            self.cursor.x = line.index_for_x(overflow_x, 0);
        }
        self
    }

    /// Move the cursor to the beginning of the current line.
    pub fn move_cursor_home(&mut self) -> &mut Self {
        self.cursor.x = 0;
        self
    }

    /// Move the cursor to the end of the current line.
    pub fn move_cursor_end(&mut self) -> &mut Self {
        self.cursor.x = self.lines[self.cursor.y].chars.len();
        self
    }

    /// Create a new line break at the current cursor position.
    ///
    /// Any text following the current cursor position will be moved to the new line. Will also move
    /// the cursor to the beginning of the new line.
    pub fn newline(&mut self) -> &mut Self {
        if let Some(max_line_count) = self.info.max_line_count {
            if self.lines.len() >= max_line_count {
                return self;
            }
        }

        let remainder = self.lines[self.cursor.y].split_off(self.info.clone(), self.cursor.x);
        self.lines.insert(self.cursor.y + 1, remainder);
        self.cursor = glm::vec2(0, self.cursor.y + 1);

        self.invalidate_size_caches();

        self
    }

    /// Delete the character at the current cursor position.
    pub fn delete_char(&mut self) -> &mut Self {
        if self.cursor.x < self.lines[self.cursor.y].len() {
            self.lines[self.cursor.y].delete(self.cursor.x);
        }

        self.invalidate_size_caches();

        self
    }

    /// Clear all lines and text from this layout.
    pub fn clear(&mut self) -> &mut Self {
        self.lines = vec![LogicalLine::new(self.info.clone())];
        self.cursor = glm::vec2(0, 0);

        self.invalidate_size_caches();

        self
    }

    /// The [TextLayoutInfo] details associated with this layout.
    #[inline]
    pub fn info(&self) -> &TextLayoutInfo { self.info.as_ref() }

    /// Width in px of the text in this layout.
    ///
    /// Note that this is NOT the canvas width, but the width of the text inside the canvas, and can
    /// be smaller (or even larger) than the canvas width. Generally, this will be the maximum width
    /// of any individual line in the layout.
    #[inline]
    pub fn width(&self) -> f32 {
        *self.cache_width.get_or_init(|| {
            self.lines()
                .map(|line| line.width())
                .fold(0.0, f32::max)
        })
    }

    /// Height in px of the text in this layout.
    ///
    /// Note that this is NOT the canvas height, but the height of the text inside the canvas, and
    /// can be smaller (or even larger) than the canvas height. Generally, this will be the combined
    /// height of all individual lines in the layout, plus any line gaps between them.
    #[inline]
    pub fn height(&self) -> f32 {
        *self.cache_height.get_or_init(|| {
            self.lines().map(|line| line.height()).sum::<f32>()
                + self.info.sizer.line_gap(self.info.size) * (self.lines.len() - 1) as f32
        })
    }

    /// Add a chunk of text to the layout at the current cursor position.
    ///
    /// If the text has any newline characters in it, will break the text into multiple logical
    /// lines while adding.
    pub fn text(&mut self, text: &str) -> &mut Self {
        let mut first = true;
        for substring in text.split('\n') {
            if !first { self.newline(); }
            self.lines[self.cursor.y].insert(self.cursor.x, substring, self.color);
            self.cursor.x += substring.chars().count();
            first = false;
        }

        self.invalidate_size_caches();

        self
    }

    /// Add the text of another [TextLayout] to this layout at the current cursor position.
    pub fn text_layout(&mut self, other: &TextLayout) -> &mut Self {
        let mut first = true;
        for line in &other.lines {
            if !first { self.newline(); }
            self.cursor.x += self.lines[self.cursor.y]
                .insert_buffer(self.cursor.x, line.physical_lines().flatten());
            first = false;
        }

        self.invalidate_size_caches();

        self
    }

    /// Resize this layout.
    pub fn resize(&mut self, canvas_offset: glm::TVec2<f32>, canvas_size: glm::TVec2<f32>) {
        {
            let mut canvas = self.info.canvas.write();
            canvas.offset = canvas_offset;
            canvas.size = canvas_size;
        }
        for line in &mut self.lines {
            line.adjust_trailing(0);
        }
        self.invalidate_size_caches();
    }

    /// Interpret an input event to control this [TextLayout].
    ///
    /// If a key that can be represented as text is pressed, will add the text to the layout.
    /// Otherwise, will attempt to move the cursor and/or modify layout text appropriately.
    #[cfg(feature = "gui")]
    pub fn handle_input_event(&mut self, event: &crate::gui::input::InputDelegateEvent) {
        use crate::gui::input::{ElementState, InputDelegateEvent, LogicalKey};
        use winit::keyboard::NamedKey;

        match event {
            InputDelegateEvent::Key(event) => {
                if event.state == ElementState::Pressed {
                    match &event.logical_key {
                        LogicalKey::Named(NamedKey::Home) => { self.move_cursor_home(); },
                        LogicalKey::Named(NamedKey::End) => { self.move_cursor_end(); },
                        LogicalKey::Named(NamedKey::Enter) => { self.newline(); },
                        LogicalKey::Named(NamedKey::Backspace) => {
                            if self.cursor != glm::vec2(0, 0) {
                                self.move_cursor_left();
                                self.delete_char();
                            }
                        },
                        LogicalKey::Named(NamedKey::Delete) => { self.delete_char(); },
                        LogicalKey::Named(NamedKey::Space) => { self.text(" "); },
                        LogicalKey::Character(c) => { self.text(c); },
                        _ => {}
                    }
                }
            },
            // TODO: Handle mouse inputs?
            _ => {}
        }
    }

    /// Create an iterator that builds [PositionedChar]s from this layout.
    #[inline]
    pub fn iter_build(&self) -> TextLayoutBuildIter<'_> {
        TextLayoutBuildIter::new(self)
    }
}

impl Display for TextLayout {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for line in &self.lines {
            if !first { f.write_char('\n')?; }
            Display::fmt(line, f)?;
            first = false;
        }
        Ok(())
    }
}

/// Iterator that builds [PositionedChar]s.
pub struct TextLayoutBuildIter<'a> {
    layout: &'a TextLayout,
    lines_iter: slice::Iter<'a, LogicalLine>,
    sub_iter: Option<LayoutLineBuildIter<'a>>,
    offset: glm::TVec2<f32>,
}

impl<'a> TextLayoutBuildIter<'a> {
    fn new(layout: &'a TextLayout) -> Self {
        let offset = glm::vec2(
            0.0,
            match layout.info.alignment.vertical {
                LayoutVerticalAlignment::Top => 0.0,
                LayoutVerticalAlignment::Center =>
                    (layout.info.canvas.read().size.y - layout.height()) / 2.0,
                LayoutVerticalAlignment::Bottom =>
                    layout.info.canvas.read().size.y - layout.height(),
            }
        );
        let mut lines_iter = layout.lines();
        let sub_iter = lines_iter.next()
            .map(|line| line.iter_build(offset));
        Self {
            layout,
            lines_iter,
            sub_iter,
            offset,
        }
    }
}

impl Iterator for TextLayoutBuildIter<'_> {
    type Item = PositionedChar;

    fn next(&mut self) -> Option<Self::Item> {
        let mut maybe_c: Option<Self::Item> = None;
        while maybe_c.is_none() {
            let sub_iter = self.sub_iter.as_mut()?;
            maybe_c = sub_iter.next();
            if maybe_c.is_none() {
                self.offset.y += sub_iter.line.height()
                    + self.layout.info.sizer.line_gap(self.layout.info.size);
                self.sub_iter = self.lines_iter.next()
                    .map(|line| line.iter_build(self.offset));
            }
        }
        maybe_c
    }
}

// TODO: Unittests