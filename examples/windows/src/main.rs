extern crate nalgebra_glm as glm;

use std::{thread, time};

use glm::identity;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use gtether::{Engine, EngineMetadata};
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::window::CreateWindowInfo;

use crate::render::{MN, VP};
use crate::render::ambient::AmbientRenderer;
use crate::render::cube::CubeRenderer;
use crate::render::directional::{DirectionalRenderer, PointLight};

mod render;

fn main() {
    let engine = Engine::new(
        EngineMetadata {
            application_name: Some(String::from("gTether Example - windows")),
            ..Default::default()
        }
    );

    println!("Engine started");

    let mut model = identity();
    model = glm::translate(&model, &glm::vec3(0.0, 0.0, -5.0));
    model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 1.0, 0.0));
    model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(1.0, 0.0, 0.0));
    model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 0.0, 1.0));

    let window = engine.window_manager().create_window(CreateWindowInfo {
        title: Some("Windowing Test".into()),
        ..Default::default()
    });
    println!("Window created");

    let cube_renderer = CubeRenderer::new(window.render_target());
    let mn = cube_renderer.mn().clone();
    mn.set(MN::new(model));
    let vp = cube_renderer.vp().clone();
    vp.set(VP::look_at(
        &glm::vec3(0.0, 0.0, 0.1),
        &glm::vec3(0.0, 0.0, 0.0),
        &glm::vec3(0.0, 1.0, 0.0),
    ));

    let ambient_renderer = AmbientRenderer::new(window.render_target());
    // let ambient_light = ambient_renderer.light().clone();

    let directional_renderer = DirectionalRenderer::new(window.render_target());
    let point_lights = directional_renderer.lights().clone();
    point_lights.set(vec![
        PointLight {
            position: [-4.0, -4.0, 0.0, 1.0],
            color: [1.0, 1.0, 1.0],
        },
    ]);

    let render_pass = EngineRenderPassBuilder::new(window.render_target())
        .attachment("color".into(), AttachmentDescription {
            format: Format::A2B10G10R10_UNORM_PACK32,
            samples: SampleCount::Sample1,
            load_op: AttachmentLoadOp::Clear,
            store_op: AttachmentStoreOp::DontCare,
            ..Default::default()
        }, Some([0.0, 0.0, 0.0, 1.0].into()))
        .attachment("normals".into(), AttachmentDescription {
            format: Format::R16G16B16A16_SFLOAT,
            samples: SampleCount::Sample1,
            load_op: AttachmentLoadOp::Clear,
            store_op: AttachmentStoreOp::DontCare,
            ..Default::default()
        }, Some([0.0, 0.0, 0.0, 1.0].into()))
        .attachment("depth".into(), AttachmentDescription {
            format: Format::D16_UNORM,
            samples: SampleCount::Sample1,
            load_op: AttachmentLoadOp::Clear,
            store_op: AttachmentStoreOp::DontCare,
            ..Default::default()
        }, Some(1.0.into()))
        .final_color_attachment("final_color".into(), [0.0, 0.0, 0.0, 1.0].into())
        .begin_subpass()
        .color_attachment("color".into())
        .color_attachment("normals".into())
        .depth_stencil_attachment("depth".into())
        .handler(Box::new(cube_renderer))
        .end_subpass()
        .begin_subpass()
        .input_attachment("color".into())
        .input_attachment("normals".into())
        .color_attachment("final_color".into())
        .handler(Box::new(ambient_renderer))
        .handler(Box::new(directional_renderer))
        .end_subpass()
        .build();
    window.set_render_pass(render_pass);
    println!("RenderPass initialized");

    while engine.window_manager().get_window_count() > 0 {
        let model = mn.get();
        mn.set(MN::new(glm::rotate_normalized_axis(&model.model, 0.01, &glm::vec3(0.0, 0.0, 1.0))));

        thread::sleep(time::Duration::from_secs_f32(1.0 / 60.0));
    }
    println!("All windows closed, exiting");

    engine.exit();
}
