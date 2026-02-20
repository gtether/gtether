#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

extern crate nalgebra_glm as glm;

use gtether::console::log::ConsoleLogLayer;
use gtether::gui::window::winit::WinitDriver;
use gtether::net::gns::GnsSubsystem;
use gtether::resource::manager::ResourceManager;
use gtether::resource::source::constant::ConstantResourceSource;
use gtether::worker::WorkerPool;
use gtether::EngineBuilder;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::client::ReversiClient;

mod board;
mod bot;
mod render_util;
mod player;
mod server;
mod client;

fn main() {
    let worker_count = std::thread::available_parallelism()
        .unwrap_or(unsafe { NonZeroUsize::new_unchecked(4) })
        .min(unsafe { NonZeroUsize::new_unchecked(4) });
    let workers = Arc::new(WorkerPool::builder()
        .worker_count(worker_count)
        .start());

    let app = ReversiClient::new(workers.clone());

    let subscriber_builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive(LevelFilter::WARN.into())
            .add_directive("reversi=debug".parse().unwrap()));
    #[cfg(not(debug_assertions))]
    let subscriber_builder = subscriber_builder.with_writer(std::io::sink);
    subscriber_builder
        .finish()
        .with(ConsoleLogLayer::new(app.console().log()))
        .init();

    let resources = ResourceManager::builder()
        .source(ConstantResourceSource::builder()
            .resource("console_font", include_bytes!("../assets/RobotoMono/RobotoMono-VariableFont_wght.ttf"))
            .resource("tile.obj", include_bytes!("../assets/tile.obj"))
            .resource("piece.obj", include_bytes!("../assets/piece.obj"))
            .build())
        .worker_config((), &workers)
        .build();

    EngineBuilder::new()
        .app(app)
        .app_driver(WinitDriver::builder()
            .application_name("gTether Example - reversi")
            .build())
        .resources(resources)
        .networking_driver(GnsSubsystem::get())
        .build()
        .start();
}
