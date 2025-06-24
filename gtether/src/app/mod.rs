/// Application traits and implementations.
///
/// This module contains traits and logic that are relevant to how an [Application] is driven by
/// the [Engine].

use async_trait::async_trait;
use std::sync::Arc;

use crate::app::driver::AppDriver;
use crate::net::driver::NetDriver;
use crate::Engine;

pub mod driver;

/// User provided application.
///
/// The Application is the main entry point for user-defined logic in the [Engine]. It is
/// implemented by the user, and consists of a series of callbacks that the engine executes at
/// defined points in its lifecycle. These callback hooks are asynchronous, which allows additional
/// subsystems to execute in parallel if required.
///
/// # Drivers
///
/// An Application also defines the type of any [drivers](self#drivers) that the [Engine] will use,
/// via associated types. Unfortunately, associated types
/// [cannot yet have defaults](https://github.com/rust-lang/rust/issues/29661), so users will need
/// to explicitly define all of them. There are several basic implementations provided by the engine,
/// however.
///
/// ## [ApplicationDriver](AppDriver)
///
/// This driver controls how the engine lifecycle is managed, and it maintains the responsibility
/// of calling most of the lifecycle hooks that exist on [Application]. These include:
/// * [Application::init()], called once when the application first launches.
/// * [Application::resume()], called whenever the application resumes from a suspended state. May
///   not be relevant for all platforms.
/// * [Application::suspend()], called whenever the application enters a suspended state. May not
///   be relevant for all platforms.
/// * [Application::terminate()], called once when the application is exiting.
///
/// Implementations provided by the engine:
/// * [MinimalAppDriver](driver::MinimalAppDriver) - The barest minimum of functionality needed to
///   execute the above hooks.
/// * [WinitDriver](crate::gui::window::winit::WinitDriver) - A windowing driver using
///   [Winit](https://github.com/rust-windowing/winit).
///
/// ## [NetworkingDriver](NetDriver)
///
/// This driver controls the engine's networking stack.
///
/// Implementations provided by the engine:
/// * [NoNetDriver](crate::net::driver::NoNetDriver) - A "noop" driver that doesn't allow for any
///   network communication.
/// * [GnsClientDriver](crate::net::gns::GnsClientDriver)/[GnsServerDriver](crate::net::gns::GnsServerDriver)
///   - Networking driver that uses [GameNetworkingSockets](https://github.com/hussein-aitlahcen/gns-rs).
///
/// # Examples
///
/// ```
/// use async_trait::async_trait;
/// use gtether::app::Application;
/// use gtether::app::driver::MinimalAppDriver;
/// use gtether::net::driver::NoNetDriver;
///
/// struct BasicApp {}
///
/// #[async_trait(?Send)]
/// impl Application for BasicApp {
///     type ApplicationDriver = MinimalAppDriver;
///     type NetworkingDriver = NoNetDriver;
///
///     // Any of the relevant hooks can be implemented here, such as init()
/// }
/// ```
#[async_trait(?Send)]
pub trait Application: Sized + Send + Sync + 'static {
    /// [AppDriver] type that will be used to drive the engine lifecycle.
    ///
    /// The simplest type to use is [MinimalAppDriver](driver::MinimalAppDriver).
    type ApplicationDriver: AppDriver;

    /// [NetDriver] type that will be used to handle networking for this application.
    ///
    /// If networking is not required, [NoNetDriver](net::driver::NoNetDriver) is an appropriate
    /// type to use.
    type NetworkingDriver: NetDriver;

    /// Initialize this Application.
    ///
    /// This will be called once when the [Engine] is [initializing](crate::EngineStage::Init).
    #[allow(unused_variables)]
    async fn init(&self, engine: &Arc<Engine<Self>>) {}

    /// Resume this Application.
    ///
    /// This will be called once whenever the [Engine] is [resumed](crate::EngineStage::Resuming).
    #[allow(unused_variables)]
    async fn resume(&self, engine: &Arc<Engine<Self>>) {}

    /// Suspend this Application.
    ///
    /// This will be called once whenever the [Engine] is [suspended](crate::EngineStage::Suspending).
    #[allow(unused_variables)]
    async fn suspend(&self, engine: &Arc<Engine<Self>>) {}

    /// Terminate this Application.
    ///
    /// This will be called once when the [Engine] is being [stopped](crate::EngineStage::Stopping).
    #[allow(unused_variables)]
    async fn terminate(&self, engine: &Arc<Engine<Self>>) {}
}