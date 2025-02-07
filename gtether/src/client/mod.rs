//! Client-side specific logic.
//!
//! This module contains anything that may be considered to be "client-side". This includes e.g.
//! windowing logic and input handling.
//!
//! At a bare minimum, this module contains a [Client] [Side] implementation that can be used to
//! run a simple main loop.
//!
//! Most of the more advanced client-side logic, such as windowing, is locked behind the `gui`
//! feature, as it involves windowing libraries that are platform specific. See the [`gui`] module
//! for more.

use std::time::{Duration, Instant};
use smol::future;
use tracing::warn;

use crate::{Application, EngineStage, EngineState, Side};

#[cfg(feature = "gui")]
pub mod gui;

/// Simple barebones [Side] implementation.
///
/// Can be configured with a specific tick rate to maintain. Tick rate may fall below the given
/// value if ticks take too long, but never above.
///
/// # Examples
/// ```
/// # use std::sync::Arc;
/// # use std::time::Duration;
/// # use async_trait::async_trait;
/// use gtether::client::Client;
/// use gtether::Engine;
/// # use gtether::Application;
/// #
/// # struct MyApp {}
/// #
/// # #[async_trait(?Send)]
/// # impl Application<Client> for MyApp {
/// #     // Implement relevant functions...
/// #     async fn init(&self, engine: &Arc<Engine<Self, Client>>) {}
/// #     fn tick(&self, engine: &Arc<Engine<Self, Client>>, delta: Duration) {}
/// # }
/// #
/// # let app = MyApp {};
///
/// // 60 ticks per second
/// let side = Client::new(60);
///
/// let engine = Engine::builder()
///     .app(app)
///     .side(side)
///     .build();
/// ```
pub struct Client {
    min_tick_duration: Duration,
}

impl Client {
    /// Create a new Client with the given `tick_rate`.
    #[inline]
    pub fn new(tick_rate: usize) -> Self {
        Self {
            min_tick_duration: Duration::from_secs_f32(1.0 / tick_rate as f32),
        }
    }
}

impl Side for Client {
    fn start<A: Application<Self>>(&self, mut engine_state: EngineState<A, Self>) {
        {
            let engine = engine_state.engine();
            future::block_on(engine.app().init(&engine));
            engine_state.set_stage(EngineStage::Running).unwrap();
        }

        let mut last_tick = Instant::now();
        let mut next_tick = last_tick + self.min_tick_duration;
        loop {
            // Sleep until next timeslot
            std::thread::sleep(next_tick - Instant::now());

            let tick_start = Instant::now();
            let delta = tick_start - last_tick;
            let engine = engine_state.engine();
            engine.app().tick(&engine, delta);
            let tick_end = Instant::now();

            // Increment timeslot
            next_tick += self.min_tick_duration;
            let mut skipped_ticks = 0;
            while next_tick < tick_end {
                next_tick += self.min_tick_duration;
                skipped_ticks += 1;
            }
            if skipped_ticks > 0 {
                let tick_time = tick_end - tick_start;
                warn!(skipped_ticks, ?tick_time, "Tick(s) took too long");
            }

            last_tick = tick_start;

            // Check for exit
            if engine_state.stage() == EngineStage::Stopping {
                engine_state.set_stage(EngineStage::Stopped).unwrap();
                break;
            }
        }
    }
}