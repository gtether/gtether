extern crate nalgebra_glm as glm;

use std::cell::{OnceCell, RefCell};
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::time::Duration;

use glm::identity;
use tracing::{event, Level};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use vulkano::format::Format;
use vulkano::image::SampleCount;
use vulkano::render_pass::{AttachmentDescription, AttachmentLoadOp, AttachmentStoreOp};

use gtether::console::command::{Command, CommandError, CommandRegistry, CommandTree, ParamCountCheck};
use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::console::{Console, ConsoleStdinReader};
use gtether::gui::input::{InputDelegate, InputDelegateEvent, InputStateLayer, KeyCode};
use gtether::gui::window::{CreateWindowInfo, WindowAttributes, WindowHandle};
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::render::uniform::{Uniform, UniformSet};
use gtether::resource::manager::{LoadPriority, ResourceManager};
use gtether::resource::source::constant::ConstantResourceSource;
use gtether::{Application, Engine, EngineBuilder, EngineMetadata, Registry};

use crate::render::ambient::{AmbientLight, AmbientRendererRefs};
use crate::render::cube::CubeRendererRefs;
use crate::render::directional::{DirectionalRendererRefs, PointLight};
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
struct PointLightListCommand {
    point_lights: Arc<UniformSet<PointLight>>,
}

impl Command for PointLightListCommand {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        ParamCountCheck::Equal(0).check(parameters.len() as u32)?;

        for (idx, point_light) in self.point_lights.read().iter().enumerate() {
            event!(Level::INFO, "{idx}: {point_light:?}");
        }

        Ok(())
    }
}

#[derive(Debug)]
struct PointLightDeleteCommand {
    point_lights: Arc<UniformSet<PointLight>>,
}

impl Command for PointLightDeleteCommand {
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
struct PointLightAddCommand {
    point_lights: Arc<UniformSet<PointLight>>,
}

impl Command for PointLightAddCommand {
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
        point_lights.push(PointLight {
            position: [x, y, z, 1.0],
            color: [r, g, b],
        });

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
    window: OnceCell<WindowHandle>,
    mn: OnceCell<Arc<Uniform<MN>>>,
    vp: OnceCell<Arc<Uniform<VP>>>,
    camera: RefCell<Camera>,
    input: OnceCell<InputDelegate>,
}

impl WindowsApp {
    fn new() -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        ConsoleStdinReader::start(&console);

        Self {
            console,
            window: OnceCell::new(),
            mn: OnceCell::new(),
            vp: OnceCell::new(),
            camera: RefCell::new(Camera::default()),
            input: OnceCell::new(),
        }
    }
}

impl Application for WindowsApp {
    fn init(&self, engine: &Engine<Self>, registry: &mut Registry) {
        let mut cmd_registry = self.console.registry();

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
        //window.set_cursor_visible(false);

        let cube_renderer = CubeRendererRefs::new();
        let ambient_renderer = AmbientRendererRefs::new();
        let directional_renderer = DirectionalRendererRefs::new();

        let console_font = engine.resources().get_or_load(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
            LoadPriority::Immediate,
        ).wait().unwrap();
        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font)
            .build().unwrap();

        let render_pass = EngineRenderPassBuilder::new(window.renderer())
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
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0])
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
            .build();
        window.renderer().set_render_pass(render_pass).unwrap();

        let mn = cube_renderer.mn().clone();
        *mn.write() = MN::new(model);
        let vp = cube_renderer.vp().clone();
        *vp.write() = VP {
            view: self.camera.borrow().view(),
            projection: identity(),
        };

        let ambient_light = ambient_renderer.light().clone();
        cmd_registry.register_command("ambient", Box::new(AmbientLightCommand {
            ambient_light,
        })).unwrap();

        let point_lights = directional_renderer.lights().clone();
        *point_lights.write() = vec![
            PointLight {
                position: [-4.0, -4.0, 0.0, 1.0],
                color: [0.8, 0.8, 0.8],
            },
        ];

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

        self.input.set(window.input_state().create_delegate()).unwrap();
        self.window.set(window).unwrap();
        self.mn.set(mn).unwrap();
        self.vp.set(vp).unwrap();
    }

    fn tick(&self, engine: &Engine<Self>, delta: Duration) {
        let window = self.window.get().unwrap();

        let mn = self.mn.get().unwrap();
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
            let vp = self.vp.get().unwrap();
            {
                let mut vp = vp.write();
                vp.view = camera.view();
            }
        }

        if window.input_state().is_key_pressed(KeyCode::Escape, None).unwrap_or(false) {
            engine.request_exit();
        }
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
        .metadata(EngineMetadata {
            application_name: Some(String::from("gTether Example - windows")),
            ..Default::default()
        })
        .app(app)
        .resources(resources)
        .build()
        .start();
}
