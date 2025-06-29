extern crate nalgebra_glm as glm;

use async_trait::async_trait;
use glm::identity;
use parking_lot::Mutex;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use tracing::{event, Level};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use gtether::app::Application;
use gtether::gui::input::{InputDelegateEvent, InputStateLayer, KeyCode};
use gtether::gui::window::winit::{CreateWindowInfo, WindowAttributes, WinitDriver};
use gtether::console::command::{Command, CommandError, CommandRegistry, CommandTree, ParamCountCheck};
use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::console::{Console, ConsoleStdinReader};
use gtether::net::driver::NoNetDriver;
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::uniform::{Uniform, UniformSet};
use gtether::resource::manager::{LoadPriority, ResourceManager};
use gtether::resource::source::constant::ConstantResourceSource;
use gtether::util::tick_loop::{TickLoopBuilder, TickLoopHandle};
use gtether::{Engine, EngineBuilder};

use crate::render::ambient::{AmbientLight, AmbientRendererBootstrap};
use crate::render::cube::CubeRendererBootstrap;
use crate::render::directional::{DirectionalRendererBootstrap, PointLight};
use crate::render::{MN, VP};

mod render;

#[derive(Debug)]
struct AmbientLightCommand {
    ambient_light: Arc<Uniform<AmbientLight>>,
}

impl Command for AmbientLightCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(3).check(parameters.len() as u32)?;

        let r: f32 = parameters[0].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
        let g: f32 = parameters[1].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
        let b: f32 = parameters[2].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;

        *self.ambient_light.write() = AmbientLight::new(r, g, b, 1.0);

        Ok(())
    }
}

#[derive(Debug)]
struct PointLightListCommand<const CAP: usize> {
    point_lights: Arc<UniformSet<PointLight, CAP>>,
}

impl<const CAP: usize> Command for PointLightListCommand<CAP> {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(0).check(parameters.len() as u32)?;

        for (idx, point_light) in self.point_lights.read().iter().enumerate() {
            event!(Level::INFO, "{idx}: {point_light:?}");
        }

        Ok(())
    }
}

#[derive(Debug)]
struct PointLightDeleteCommand<const CAP: usize> {
    point_lights: Arc<UniformSet<PointLight, CAP>>,
}

impl<const CAP: usize> Command for PointLightDeleteCommand<CAP> {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(1).check(parameters.len() as u32)?;

        let mut point_lights = self.point_lights.write();

        let idx: usize = parameters[0].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;

        if idx >= point_lights.len() {
            return Err(CommandError::CommandFailure(format!("Index out of bounds: {idx}").as_str().into()))
        }

        point_lights.remove(idx);

        Ok(())
    }
}

#[derive(Debug)]
struct PointLightAddCommand<const CAP: usize> {
    point_lights: Arc<UniformSet<PointLight, CAP>>,
}

impl<const CAP: usize> Command for PointLightAddCommand<CAP> {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::OneOf(vec![
            ParamCountCheck::Equal(3),
            ParamCountCheck::Equal(6),
        ]).check(parameters.len() as u32)?;

        let x: f32 = parameters[0].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
        let y: f32 = parameters[1].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
        let z: f32 = parameters[2].parse()
            .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;

        let mut r: f32 = 1.0;
        let mut g: f32 = 1.0;
        let mut b: f32 = 1.0;
        if parameters.len() == 6 {
            r = parameters[3].parse()
                .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
            g = parameters[4].parse()
                .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
            b = parameters[5].parse()
                .map_err(|e| CommandError::CommandFailure(Box::new(e)))?;
        }

        let mut point_lights = self.point_lights.write();
        point_lights.try_push(PointLight {
            position: [x, y, z, 1.0],
            color: [r, g, b],
        }).map_err(|e| CommandError::CommandFailure(Box::new(e)))?;

        Ok(())
    }
}

struct ConsoleDumpCommand {
    log: Arc<ConsoleLog>,
}

impl Debug for ConsoleDumpCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleDumpCommand").finish_non_exhaustive()
    }
}

impl Command for ConsoleDumpCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::OneOf(vec![
            ParamCountCheck::Equal(0),
            ParamCountCheck::Equal(1),
        ]).check(parameters.len() as u32)?;

        let count: usize = if parameters.len() == 1 {
            parameters[0].parse()
                .map_err(|e| CommandError::CommandFailure(Box::new(e)))?
        } else {
            10
        };

        self.log.iter()
            .rev()
            .take(count)
            .rev()
            .for_each(|log_entry| println!("{log_entry}"));

        Ok(())
    }
}

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

struct WindowsApp {
    console: Arc<Console>,
    tick_loop_handle: Mutex<Option<TickLoopHandle>>,
}

impl WindowsApp {
    fn new() -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        ConsoleStdinReader::start(&console);

        Self {
            console,
            tick_loop_handle: Mutex::new(None),
        }
    }
}

#[async_trait(?Send)]
impl Application for WindowsApp {
    type ApplicationDriver = WinitDriver;
    type NetworkingDriver = NoNetDriver;

    async fn init(&self, engine: &Arc<Engine<Self>>) {
        let mut cmd_registry = self.console.registry();

        let mut model = identity();
        model = glm::translate(&model, &glm::vec3(0.0, 0.0, -5.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 1.0, 0.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(1.0, 0.0, 0.0));
        model = glm::rotate_normalized_axis(&model, 1.0, &glm::vec3(0.0, 0.0, 1.0));

        let window = engine.window_manager().create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Windowing Test"),
            ..Default::default()
        }).await;
        window.set_cursor_visible(false);

        let renderer = window.renderer();

        let cube_renderer = CubeRendererBootstrap::new(renderer);
        let ambient_renderer = AmbientRendererBootstrap::new(renderer);
        let directional_renderer = DirectionalRendererBootstrap::new(
            renderer,
            vec![
                PointLight {
                    position: [-4.0, -4.0, 0.0, 1.0],
                    color: [0.8, 0.8, 0.8],
                },
            ]
        );

        let console_font = engine.resources().get_or_load(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
            LoadPriority::Immediate,
        ).await.unwrap();
        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font)
            .build().unwrap();

        let render_pass = EngineRenderPassBuilder::new(renderer.clone())
            .attachment("color", AttachmentDescription {
                format: Format::A2B10G10R10_UNORM_PACK32,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some([0.0, 0.0, 0.0, 1.0]))
            .attachment("normals", AttachmentDescription {
                format: Format::R16G16B16A16_SFLOAT,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some([0.0, 0.0, 0.0, 1.0]))
            .attachment("depth", AttachmentDescription {
                format: Format::D16_UNORM,
                samples: SampleCount::Sample1,
                load_op: AttachmentLoadOp::Clear,
                store_op: AttachmentStoreOp::DontCare,
                ..Default::default()
            }, Some(1.0))
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0]).unwrap()
            .begin_subpass()
                .color_attachment("color")
                .color_attachment("normals")
                .depth_stencil_attachment("depth")
                .handler(cube_renderer.bootstrap())
            .end_subpass()
            .begin_subpass()
                .input_attachment("color")
                .input_attachment("normals")
                .color_attachment("final_color")
                .handler(ambient_renderer.bootstrap())
                .handler(directional_renderer.bootstrap())
                .handler(console_gui.bootstrap_renderer())
            .end_subpass()
            .build().unwrap();
        window.renderer().set_render_pass(render_pass);

        let mut camera = Camera::default();

        let mn = cube_renderer.mn().clone();
        *mn.write() = MN::new(model);
        let vp = cube_renderer.vp().clone();
        *vp.write() = VP {
            view: camera.view(),
            projection: identity(),
        };

        let ambient_light = ambient_renderer.light().clone();
        cmd_registry.register_command("ambient", Box::new(AmbientLightCommand {
            ambient_light,
        })).unwrap();

        let point_lights = directional_renderer.lights().clone();
        let mut point_light_subcommands = CommandTree::default();
        point_light_subcommands.register_command("list", Box::new(PointLightListCommand {
            point_lights: point_lights.clone()
        })).unwrap();
        point_light_subcommands.register_alias("l", "list").unwrap();
        point_light_subcommands.register_command("delete", Box::new(PointLightDeleteCommand {
            point_lights: point_lights.clone()
        })).unwrap();
        point_light_subcommands.register_alias("del", "delete").unwrap();
        point_light_subcommands.register_alias("d", "delete").unwrap();
        point_light_subcommands.register_command("add", Box::new(PointLightAddCommand {
            point_lights: point_lights.clone()
        })).unwrap();
        point_light_subcommands.register_alias("a", "add").unwrap();
        cmd_registry
            .register_command("point", Box::new(point_light_subcommands)).unwrap()
            .register_alias("points", "point").unwrap();

        cmd_registry.register_command("dump", Box::new(ConsoleDumpCommand {
            log: self.console.log().clone(),
        })).unwrap();

        let input = window.input_state().create_delegate();

        let engine = engine.clone();
        let mut tick_loop_handle = self.tick_loop_handle.lock();
        // Drop/stop any existing tick loop
        tick_loop_handle.take();
        *tick_loop_handle = Some(TickLoopBuilder::new()
            .name("main-loop")
            .tick_rate(60)
            .init(|| Ok::<(), ()>(()))
            .tick(move |_, delta| {
                {
                    let mut mn = mn.write();
                    *mn = MN::new(glm::rotate_normalized_axis(
                        &mn.model,
                        delta.as_secs_f32(),
                        &glm::vec3(0.0, 0.0, 1.0),
                    ));
                }

                let speed = 5.0;
                let rotate_speed = 0.001;
                let distance = delta.as_secs_f32() * speed;

                let mut changed = false;
                let mut orient = camera.orient();
                let right = camera.right();

                for event in input.events() {
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

                if window.input_state().is_key_pressed(KeyCode::KeyW, None).unwrap_or(false) {
                    camera.pos += orient * distance;
                    changed = true;
                }
                if window.input_state().is_key_pressed(KeyCode::KeyS, None).unwrap_or(false) {
                    camera.pos += orient * -distance;
                    changed = true;
                }
                if window.input_state().is_key_pressed(KeyCode::KeyD, None).unwrap_or(false) {
                    camera.pos += right * distance;
                    changed = true;
                }
                if window.input_state().is_key_pressed(KeyCode::KeyA, None).unwrap_or(false) {
                    camera.pos += right * -distance;
                    changed = true;
                }

                if changed {
                    {
                        let mut vp = vp.write();
                        vp.view = camera.view();
                    }
                }

                if window.input_state().is_key_pressed(KeyCode::Escape, None).unwrap_or(false) {
                    engine.stop().unwrap();
                }

                true
            })
            .spawn().unwrap());
    }

    async fn terminate(&self, _engine: &Arc<Engine<Self>>) {
        // Stop/drop any running tick loop
        self.tick_loop_handle.lock().take();
    }
}

fn main() {
    let app = WindowsApp::new();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .finish()
        .with(ConsoleLogLayer::new(app.console.log()))
        .init();

    let resources = ResourceManager::builder()
        .source(ConstantResourceSource::builder()
            .resource("console_font", include_bytes!("RobotoMono/RobotoMono-VariableFont_wght.ttf"))
            .build())
        .build();

    EngineBuilder::new()
        .app(app)
        .app_driver(WinitDriver::builder()
            .application_name("gTether Example - windows")
            .build())
        .resources(resources)
        .networking_driver(NoNetDriver::new())
        .build()
        .start();
}
