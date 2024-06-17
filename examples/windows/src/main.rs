extern crate nalgebra_glm as glm;

use std::cell::{OnceCell, RefCell};
use std::sync::Arc;
use std::time::Duration;

use glm::identity;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use gtether::{Engine, EngineBuilder, EngineMetadata, Application, Registry};
use gtether::gui::input::{InputDelegate, InputDelegateEvent, KeyCode};
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::gui::window::{CreateWindowInfo, WindowAttributes, WindowHandle};

use crate::render::{MN, Uniform, VP};
use crate::render::ambient::AmbientRenderer;
use crate::render::cube::CubeRenderer;
use crate::render::directional::{DirectionalRenderer, PointLight};

mod render;

#[derive(Clone, Debug)]
struct Camera {
    pos: glm::TVec3<f32>,
    orient: glm::TVec3<f32>,
    up: glm::TVec3<f32>,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pos: glm::vec3(0.0, 0.0, 0.1),
            orient: glm::vec3(0.0, 0.0, -1.0),
            up: glm::vec3(0.0, 1.0, 0.0),
        }
    }
}

impl Camera {
    #[inline]
    fn view(&self) -> glm::TMat4<f32> {
        glm::look_at(&self.pos, &(self.pos + self.orient()), &self.up)
    }

    #[inline]
    fn orient(&self) -> glm::TVec3<f32> {
        self.orient.normalize()
    }

    #[inline]
    fn right(&self) -> glm::TVec3<f32> {
        self.orient.cross(&self.up).normalize()
    }
}

struct AppCore {
    window: OnceCell<WindowHandle>,
    mn: OnceCell<Arc<Uniform<MN>>>,
    vp: OnceCell<Arc<Uniform<VP>>>,
    camera: RefCell<Camera>,
    input: OnceCell<InputDelegate>,
}

impl AppCore {
    fn new() -> Self {
        Self {
            window: OnceCell::new(),
            mn: OnceCell::new(),
            vp: OnceCell::new(),
            camera: RefCell::new(Camera::default()),
            input: OnceCell::new(),
        }
    }
}

impl Application for AppCore {
    fn init(&self, _engine: &Engine<Self>, registry: &mut Registry) {
        let mut model = identity();
        model = glm::translate(&model, &glm::vec3(0.0, 0.0, -5.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 1.0, 0.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(1.0, 0.0, 0.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 0.0, 1.0));

        let window = registry.window.create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Windowing Test"),
            ..Default::default()
        });
        window.set_cursor_visible(false);

        let cube_renderer = CubeRenderer::new(window.render_target());
        let mn = cube_renderer.mn().clone();
        mn.set(MN::new(model));
        let vp = cube_renderer.vp().clone();
        vp.set(VP {
            view: self.camera.borrow().view(),
            projection: identity(),
        });

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

        self.input.set(window.input_state().create_delegate()).unwrap();
        self.window.set(window).unwrap();
        self.mn.set(mn).unwrap();
        self.vp.set(vp).unwrap();
    }

    fn tick(&self, engine: &Engine<Self>, delta: Duration) {
        let window = self.window.get().unwrap();

        let mn = self.mn.get().unwrap();
        let model = mn.get();
        mn.set(MN::new(glm::rotate_normalized_axis(
            &model.model,
            delta.as_secs_f32(),
            &glm::vec3(0.0, 0.0, 1.0),
        )));

        let speed = 5.0;
        let rotate_speed = 0.001;
        let distance = delta.as_secs_f32() * speed;

        let mut changed = false;
        let mut camera = self.camera.borrow_mut();
        let mut orient = camera.orient();
        let right = camera.right();

        for event in self.input.get().unwrap().events() {
            match event {
                InputDelegateEvent::MouseMotion(motion) => {
                    let mut rotate_matrix = identity();
                    // Avoid miniscule calculations
                    if motion.y.abs() > 0.01 {
                        rotate_matrix = glm::rotate(&rotate_matrix, (motion.y as f32) * rotate_speed, &right);
                        changed = true;
                    }
                    if motion.x.abs() > 0.01 {
                        rotate_matrix = glm::rotate(&rotate_matrix, (-motion.x as f32) * rotate_speed, &camera.up);
                        changed = true;
                    }

                    if changed {
                        let wide_orient = glm::vec4(orient.x, orient.y, orient.z, 1.0);
                        orient = glm::vec4_to_vec3(&(rotate_matrix * wide_orient));
                        camera.orient = orient;
                    }
                },
                _ => {},
            }
        }

        if window.input_state().is_key_pressed(KeyCode::KeyW, None) {
            camera.pos += orient * distance;
            changed = true;
        }
        if window.input_state().is_key_pressed(KeyCode::KeyS, None) {
            camera.pos += orient * -distance;
            changed = true;
        }
        if window.input_state().is_key_pressed(KeyCode::KeyD, None) {
            camera.pos += right * distance;
            changed = true;
        }
        if window.input_state().is_key_pressed(KeyCode::KeyA, None) {
            camera.pos += right * -distance;
            changed = true;
        }

        if changed {
            let vp_uniform = self.vp.get().unwrap();
            let mut vp = vp_uniform.get();
            vp.view = camera.view();
            vp_uniform.set(vp);
        }

        if window.input_state().is_key_pressed(KeyCode::Escape, None) {
            engine.request_exit();
        }
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    let core = AppCore::new();

    EngineBuilder::new()
        .metadata(EngineMetadata {
            application_name: Some(String::from("gTether Example - windows")),
            ..Default::default()
        })
        .app(core)
        .build()
        .start();
}
