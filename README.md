# g(ame)Tether

![License](https://img.shields.io/badge/license-BSD--2--clause%20Plus%20Patent-blue.svg)

## What It Is

Highly concurrent multiplayer focused game engine, with an emphasis on realtime streamable asset
management.

### Goals

* **Fast**: Logic should be quick and efficient, as well as highly concurrent and/or parallel. Async
  is used where it makes sense, e.g. for IO operations.
* **Modular**: Most subsystems should have swappable implementations to fit different technologies
  and/or platforms.
* **Multiplayer-Orientated**: This engine is designed to be used for multiplayer games, and its 
  design should reflect that.

## What It Isn't

An all-purpose game engine. gTether isn't designed to cater to any situation, but is rather 
intended for a specific category of game. gTether is also not designed to be a "simple" game 
engine. While we avoid complexity for the sake of complexity, we will prioritize function over 
ease-of-use.

If a simple all-purpose game engine is what you are looking for, you may be interested in
[Bevy](https://bevy.org/) instead.

## Getting Started

Take a look at the examples, especially the [Reversi example](examples/reversi/README.md).

## Licensing

gTether is free and open source. It is licensed under
[BSD-2-Clause-Plus-Patent](https://opensource.org/license/BSDplusPatent). This serves as an
alternative to dual licensing under MIT/Apache-2.0, which is commonly used in the broader Rust
ecosystem.