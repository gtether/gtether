//! GUI-specific logic.
//!
//! This module requires the `gui` crate feature, which in turn requires the `render` crate feature.
//!
//! For windowing management via Winit, see the [`window::winit`] module.
//!
//! For input handling, see the [`input`] module.

pub mod input;
pub mod window;
