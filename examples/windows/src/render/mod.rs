use std::fmt::Debug;

use glm::{identity, TMat4, TVec3};
use vulkano::buffer::BufferContents;

pub mod cube;
pub mod ambient;
pub mod directional;

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct MN {
    pub model: TMat4<f32>,
    pub normals: TMat4<f32>,
}

impl MN {
    #[inline]
    pub fn new(model: TMat4<f32>) -> Self {
        Self {
            model,
            normals: glm::inverse_transpose(model),
        }
    }
}

impl Default for MN {
    #[inline]
    fn default() -> Self { Self::new(identity()) }
}

#[derive(BufferContents, Debug, Clone)]
#[repr(C)]
pub struct VP {
    pub view: TMat4<f32>,
    pub projection: TMat4<f32>,
}

impl VP {
    #[allow(unused)]
    #[inline]
    pub fn look_at(eye: &TVec3<f32>, center: &TVec3<f32>, up: &TVec3<f32>) -> Self {
        VP {
            view: glm::look_at(eye, center, up),
            projection: identity(),
        }
    }
}

impl Default for VP {
    #[inline]
    fn default() -> Self {
        Self {
            view: identity(),
            projection: identity(),
        }
    }
}