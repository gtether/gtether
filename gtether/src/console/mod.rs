use std::{io, thread};
use std::sync::Arc;

use tracing::{event, Level};

use crate::console::command::{Command, CommandError, CommandRegisterError, CommandRegistry, CommandTree};

pub mod command;

/// Struct representing a game "console".
///
/// A [Console] allows for input of debug commands when testing or otherwise manipulating a game's
/// state. Custom [Command]s can be registered at engine initialization, and can be used to execute
/// custom logic.
pub struct Console {
    commands: CommandTree,
}

impl Default for Console {
    fn default() -> Self {
        Console {
            commands: CommandTree::default(),
        }
    }
}

impl Console {
    /// Handle execution of a command string
    ///
    /// Takes a command string (generally an entire line of input) and executes any matching
    /// commands. Will perform a "shlex" split (splitting the input into parts based on standard
    /// shell escaping rules), and match against registered commands based on the first split part.
    ///
    /// If no commands are found, or there is otherwise an error with command execution, will log a
    /// warning event with the details.
    pub fn handle_command(&self, command: String) -> Result<(), CommandError> {
        let parts = shlex::split(&command).unwrap();
        self.commands.handle(parts.as_slice())
            .map_err(|err| {
                event!(Level::WARN, "Command failed: {err}");
                err
            })
    }
}

impl CommandRegistry for Console {
    fn register_command<S: Into<String>>(
        &mut self,
        key: S,
        command: Box<dyn Command>,
    ) -> Result<&mut Self, CommandRegisterError> {
        self.commands.register_command(key, command)?;
        Ok(self)
    }

    fn register_alias<S: Into<String>, T: Into<String>>(
        &mut self,
        alias: S,
        key: T,
    ) -> Result<&mut Self, CommandRegisterError> {
        self.commands.register_alias(alias, key)?;
        Ok(self)
    }
}

/// Entry-point for initializing a stdin reader for a [Console].
///
/// Allows a [Console] to handle commands input from stdin.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use gtether::console::{Console, ConsoleStdinReader};
///
/// let console = Console::default();
/// // Configure the console...
///
/// let console = Arc::new(console);
/// // Store it somewhere as needed...
///
/// ConsoleStdinReader::start(&console);
/// ```
pub struct ConsoleStdinReader {}

impl ConsoleStdinReader {
    /// Start a new [ConsoleStdinReader]
    ///
    /// See main documentation of [ConsoleStdinReader] for more
    pub fn start(console: &Arc<Console>) {
        let console = console.clone();
        thread::spawn(move || {
            for line in io::stdin().lines() {
                // If there is an error, it will already have been logged, and we don't want to do
                // anything more.
                let _ = console.handle_command(line.unwrap());
            }
        });
    }
}