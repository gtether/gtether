# Reversi Example

This example showcases the following subsystems:
 * Rendering
 * Networking

When running, the app will start up with a blank screen. To start a game, open the console with `~`
and enter the command `host`.

## Console Commands

Common arguments:
* `[arg]`: An optional argument
* `<arg>`: A required argument
* `address`: Uses the format `[ip]:[port]`, where either `ip` or `port` may be omitted, in 
  which case the default will be used - your systems default network interface for `ip`, and 
  `19502` for `port`.

Commands:
* `host [address]`: Start a game server using the given address. If the address is omitted 
  entirely, uses defaults for `ip` and `port`.
* `connect [address]`: Connect to a running game server using the given address. If the address 
  is omitted entirely, uses defaults for `ip` and `port` - this effectively connects to a 
  locally running server.
* `close`: Close any currently active servers or connections.
* `name <name>`: Set your preferred name for using in new games. Must be set before hosting or 
  connecting to a game.

## TODO

Add menu UI instead of pure console commands once the engine has UI support.