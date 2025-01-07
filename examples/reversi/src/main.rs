use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::util::SubscriberInitExt;
use gtether::{Application, Engine, EngineBuilder, EngineMetadata, Registry};
use gtether::console::Console;
use gtether::console::gui::ConsoleGui;
use gtether::console::log::{ConsoleLog, ConsoleLogLayer};
use gtether::gui::window::{CreateWindowInfo, WindowAttributes};
use gtether::render::font::glyph::GlyphFontLoader;
use gtether::render::render_pass::EngineRenderPassBuilder;
use gtether::resource::manager::{LoadPriority, ResourceManager};
use gtether::resource::source::constant::ConstantResourceSource;

struct ReversiApp {
    console: Arc<Console>,
}

impl ReversiApp {
    fn new() -> Self {
        let console = Arc::new(Console::builder()
            .log(ConsoleLog::new(1000))
            .build());

        Self {
            console,
        }
    }
}

impl Application for ReversiApp {
    fn init(&self, engine: &Engine<Self>, registry: &mut Registry) {
        let window = registry.window.create_window(CreateWindowInfo {
            attributes: WindowAttributes::default()
                .with_title("Reversi"),
            ..Default::default()
        });

        let console_font = engine.resources().get_or_load(
            "console_font",
            GlyphFontLoader::new(window.renderer().clone()),
            LoadPriority::Immediate,
        ).wait_blocking().unwrap();
        let console_gui = ConsoleGui::builder(self.console.clone())
            .window(&window)
            .font(console_font)
            .build().unwrap();

        let render_pass = EngineRenderPassBuilder::new(window.renderer())
            .final_color_attachment("final_color", [0.0, 0.0, 0.0, 1.0])
            .begin_subpass()
                .color_attachment("final_color")
                .handler(console_gui.bootstrap_renderer())
            .end_subpass()
            .build();
        window.renderer().set_render_pass(render_pass).unwrap();
    }

    fn tick(&self, engine: &Engine<Self>, delta: Duration) {
        // noop
    }
}

fn main() {
    let app = ReversiApp::new();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive(LevelFilter::WARN.into())
            .add_directive("reversi=debug".parse().unwrap()))
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
            application_name: Some("gTether Example - reversi".to_owned()),
            ..Default::default()
        })
        .app(app)
        .resources(resources)
        .build()
        .start();
}
