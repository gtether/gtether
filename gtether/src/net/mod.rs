//! Networking logic.
//!
//! This module contains logic related to game networking. Rather than directly using a third-party
//! library for networking, this engine provides its own API for game networking, and hooks into
//! a third-party library underneath. This allows the underlying library to be swapped without
//! needing to restructure any code.
//!
//! gTether provides an implementation of the API using
//! [Game Networking Sockets](https://github.com/ValveSoftware/GameNetworkingSockets), but further
//! implementations can be easily made by implementing the provided interfaces.

pub mod client;
pub mod gns;
pub mod server;