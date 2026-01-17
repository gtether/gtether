use std::{io, thread};
use std::sync::Arc;
use parking_lot::{RwLock, RwLockWriteGuard};

use tracing::{event, Level};

use crate::console::command::{Command, CommandError, CommandRegisterError, CommandRegistry, CommandTree};
use crate::console::log::ConsoleLog;

pub mod command;
#[cfg(feature = "gui")]
pub mod gui;
pub mod log;

/// Struct representing a game "console".
///
/// A [Console] allows for input of debug commands when testing or otherwise manipulating a game's
/// state. Custom [Command]s can be registered at engine initialization, and can be used to execute
/// custom logic.
pub struct Console {
    commands: RwLock<CommandTree>,
    log: Arc<ConsoleLog>,
}

impl Console {
    /// Alias for [ConsoleBuilder::default()], that doesn't require bringing [ConsoleBuilder] into
    /// scope.
    #[inline]
    pub fn builder() -> ConsoleBuilder {
        ConsoleBuilder::default()
    }

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
        self.commands.read().handle(parts.as_slice())
            .map_err(|err| {
                event!(Level::WARN, "Command failed: {err}");
                err
            })
    }

    /// Get a [ConsoleCommandRegistry] that can register commands to this [Console].
    ///
    /// NOTE: The [ConsoleCommandRegistry] holds a write lock on this [Console], so it is highly
    /// recommended not to store it anywhere, as it will prevent the [Console] from handling
    /// command input while it is active.
    ///
    /// See [ConsoleCommandRegistry] for more.
    pub fn registry(&self) -> ConsoleCommandRegistry<'_> {
        ConsoleCommandRegistry {
            inner: self.commands.write(),
        }
    }

    /// Get a reference to this [Console]'s [ConsoleLog].
    #[inline]
    pub fn log(&self) -> &Arc<ConsoleLog> { &self.log }
}

/// Builder for creating a new [Console].
///
/// It is recommended to use [Console::builder()] to create a builder, in order to avoid needing to
/// bring [ConsoleBuilder] into scope.
///
/// # Examples
///
/// Basic console using defaults.
/// ```
/// use gtether::console::Console;
///
/// let console = Console::builder().build();
/// ```
///
/// Console with modified [log record][record] buffer size.
/// ```
/// use gtether::console::Console;
/// use gtether::console::log::ConsoleLog;
///
/// let console = Console::builder()
///     .log(ConsoleLog::new(50))
///     .build();
/// ```
///
/// [record]: log::ConsoleLogRecord
#[derive(Default)]
pub struct ConsoleBuilder {
    log: Option<ConsoleLog>,
}

impl ConsoleBuilder {
    /// Specify a custom [ConsoleLog].
    ///
    /// If not specified, a default [ConsoleLog] will be created.
    pub fn log(mut self, log: ConsoleLog) -> Self {
        self.log = Some(log);
        self
    }

    /// Build the [Console] and consume this builder.
    pub fn build(self) -> Console {
        Console {
            commands: RwLock::new(CommandTree::default()),
            log: Arc::new(self.log.unwrap_or_default()),
        }
    }
}

/// Temporary registry for adding [Command]s to a [Console].
///
/// Holds a write lock on the relevant [Console], so should not be stored anywhere beyond immediate
/// use to add new [Command]s.
///
/// # Examples
///
/// ```
/// # use gtether::console::command::{Command, CommandError};
/// # use gtether::console::Console;
/// use gtether::console::command::CommandRegistry;
///
/// # #[derive(Debug)]
/// # struct MyCommand {}
/// #
/// # impl Command for MyCommand {
/// #     fn handle(&self, parameters: &[String]) -> Result<(), CommandError> { Ok(()) }
/// #     fn options(&self, parameters: &[String]) -> Vec<String> { Vec::new() }
/// # }
/// #
/// # impl MyCommand {
/// #     fn new() -> Self { Self {} }
/// # }
/// #
/// # let console = Console::builder().build();
/// #
/// console.registry()
///     .register_command("my-command", Box::new(MyCommand::new())).unwrap()
///     .register_alias("my-alias", "my-command").unwrap();
///
/// ```
pub struct ConsoleCommandRegistry<'a> {
    inner: RwLockWriteGuard<'a, CommandTree>,
}

impl CommandRegistry for ConsoleCommandRegistry<'_> {
    fn register_command<S: Into<String>>(
        &mut self,
        key: S,
        command: Box<dyn Command>,
    ) -> Result<&mut Self, CommandRegisterError> {
        self.inner.register_command(key, command)?;
        Ok(self)
    }

    fn register_alias<S: Into<String>, T: Into<String>>(
        &mut self,
        alias: S,
        key: T,
    ) -> Result<&mut Self, CommandRegisterError> {
        self.inner.register_alias(alias, key)?;
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
/// let console = Console::builder().build();
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