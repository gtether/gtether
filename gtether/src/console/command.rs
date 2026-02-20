use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::{Debug, Formatter};

/// Validation check for parameter counts.
///
/// Can be used to validate that the count of parameters used for a command match certain
/// conditions.
///
/// # Examples
///
/// Simple usage:
/// ```
/// # use gtether::console::command::CommandError;
/// use gtether::console::command::ParamCountCheck;
///
/// # fn inner() -> Result<(), CommandError> {
/// let param_count = 5;
///
/// // Check parameter count equals 3
/// ParamCountCheck::Equal(3).check(param_count)?;
///
/// // Check parameter count is between 3 and 4
/// ParamCountCheck::Range {min: 3, max: 4}.check(param_count)?;
/// # Ok(())
/// # }
/// ```
///
/// Multiple conditions:
/// ```
/// # use gtether::console::command::CommandError;
/// use gtether::console::command::ParamCountCheck;
///
/// # fn inner() -> Result<(), CommandError> {
/// let param_count = 5;
///
/// // Check parameter count is one of:
/// //  - 2
/// //  - between 5 and 7 (inclusive)
/// //  - greater than or equal to 10
/// ParamCountCheck::OneOf(vec![
///     ParamCountCheck::Equal(2),
///     ParamCountCheck::Range {min: 5, max: 7},
///     ParamCountCheck::AtLeast(10),
/// ]).check(param_count)?;
/// ParamCountCheck::Equal(3).check(param_count)?;
/// # Ok(())
/// # }
/// ```
#[allow(unused)]
#[derive(Clone, Debug, PartialEq)]
pub enum ParamCountCheck {
    /// Parameter count should be equal the target.
    Equal(u32),
    /// Parameter count should be equal to or greater than the target.
    AtLeast(u32),
    /// Parameter count should be equal to or less than the target.
    AtMost(u32),
    /// Parameter count should be within the given range (inclusive).
    Range{ min: u32, max: u32 },
    /// Parameter count should match one of the given sub-conditions
    OneOf(Vec<Self>),
}

impl fmt::Display for ParamCountCheck {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ParamCountCheck::Equal(target) =>
                write!(f, "x == {target}"),
            ParamCountCheck::AtLeast(target) =>
                write!(f, "{target} <= x"),
            ParamCountCheck::AtMost(target) =>
                write!(f, "x <= {target}"),
            ParamCountCheck::Range {min, max} =>
                write!(f, "{min} <= x <= {max}"),
            ParamCountCheck::OneOf(sub_conditions) => {
                for (idx, condition) in sub_conditions.iter().enumerate() {
                    if idx != 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{condition}")?;
                }
                Ok(())
            }
        }
    }
}

impl ParamCountCheck {
    /// Check that the given count matches this condition.
    pub fn matches(&self, count: u32) -> bool {
        match self {
            ParamCountCheck::Equal(target) => &count == target,
            ParamCountCheck::AtLeast(target) => target <= &count,
            ParamCountCheck::AtMost(target) => &count <= target,
            ParamCountCheck::Range {min, max} => min <= &count && &count <= max,
            ParamCountCheck::OneOf(conditions) => conditions.iter()
                .any(|condition| condition.matches(count)),
        }
    }

    /// Check that the given count matches this condition, and create the appropriate [CommandError]
    /// if it doesn't.
    pub fn check(self, count: u32) -> Result<(), CommandError> {
        if !self.matches(count) {
            return Err(CommandError::InvalidParameterCount {actual: count, compare_op: self});
        }
        Ok(())
    }
}

/// Errors that can be encountered during command execution.
#[derive(Debug)]
pub enum CommandError {
    /// The given parameter is invalid for this command, or there is no command that matches said
    /// parameter.
    InvalidParameter(String),
    /// The count of parameters is invalid for this command.
    InvalidParameterCount{ actual: u32, compare_op: ParamCountCheck },
    /// Generalized catch-all for all other failure types.
    CommandFailure(Box<dyn Error + Send>),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::InvalidParameter(parameter) =>
                write!(f, "Invalid command parameter: {parameter}"),
            CommandError::InvalidParameterCount {actual, compare_op } =>
                write!(f, "Parameter count ({actual}) does not match constraint(s): {compare_op}"),
            CommandError::CommandFailure(e) =>
                write!(f, "Command failed: {}", e.to_string()),
        }
    }
}

impl Error for CommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CommandError::CommandFailure(e) => e.source(),
            _ => None,
        }
    }
}

/// Command that can be executed to trigger custom logic.
///
/// Must be Send + Sync.
pub trait Command: Debug + Send + Sync + 'static {
    /// Handle the execution of this command.
    ///
    /// Process the given parameters and execute the logic this command represents. If there are
    /// any issues, return a [CommandError].
    ///
    /// Note that implementations should _avoid_ panics if anything goes wrong, and prefer to report
    /// failures as [CommandError]s where possible.
    ///
    /// The input parameters are expected to NOT include the command name, but start after said
    /// command name.
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError>;

    /// Retrieve auto-complete options for this command.
    ///
    /// Given any already partially written parameters, determine the collection of valid
    /// possibilities that the next (or current partially written) parameter could be.
    ///
    /// If auto-completion is undesired or unnecessary, it is safe to return an empty list.
    ///
    /// The input parameters are expected to NOT include the command name, but start after said
    /// command name.
    ///
    /// A blank string ("") should be used as the final parameter to indicate that all possibilities
    /// should be retrieved for that parameter; otherwise it is assumed that possibilities are
    /// wanted for the last parameter specified, fully written or not.
    #[allow(unused)]
    fn options(&self, parameters: &[String]) -> Vec<String> {
        vec![]
    }
}

/// Errors that can be encountered when registering commands.
#[derive(Debug)]
pub enum CommandRegisterError {
    /// A command already exists for the given name/key.
    CommandExists(String),
    /// No command exists for the given alias target.
    AliasTargetMissing(String),
}

impl fmt::Display for CommandRegisterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            CommandRegisterError::CommandExists(command) =>
                write!(f, "Command '{command}' already exists"),
            CommandRegisterError::AliasTargetMissing(command) =>
                write!(f, "No alias target '{command}' exists"),
        }
    }
}

impl Error for CommandRegisterError {}

/// Interface to register commands.
///
/// # Examples
/// ```
/// # use gtether::console::command::{Command, CommandRegistry};
/// # fn inner(registry: &mut impl CommandRegistry, command: Box<dyn Command>) {
/// // Register a command
/// registry.register_command("myCommand", command)
///     .expect("Command failed to register");
///
/// // Register an alias to a command
/// registry.register_alias("myAlias", "myCommand")
///     .expect("Alias failed to register");
/// # }
/// ```
pub trait CommandRegistry {
    /// Register a new command.
    ///
    /// Can fail to register if a command is already registered with the same key.
    fn register_command<S: Into<String>>(
        &mut self,
        key: S,
        command: Box<dyn Command>,
    ) -> Result<&mut Self, CommandRegisterError>;

    /// Register a new alias.
    ///
    /// Can fail to register if an alias is already registered with the same key, or if the alias
    /// target does NOT exist.
    fn register_alias<S: Into<String>, T: Into<String>>(
        &mut self,
        alias: S,
        key: T,
    ) -> Result<&mut Self, CommandRegisterError>;
}

#[derive(Debug)]
enum CommandEntry {
    COMMAND(Box<dyn Command>),
    ALIAS(String),
}

/// A collection of sub-commands.
///
/// A command that holds sub-commands, and delegates to sub-commands based on sub-parameters.
/// Implements both [Command] and [CommandRegistry].
///
/// # Examples
/// ```
/// # use gtether::console::command::{Command, CommandRegistry};
/// use gtether::console::command::CommandTree;
/// # fn inner(registry: &mut impl CommandRegistry, command_a: Box<dyn Command>, command_b: Box<dyn Command>) {
/// let mut command_tree = CommandTree::default();
/// command_tree
///     .register_command("a", command_a).unwrap()
///     .register_command("b", command_b).unwrap()
///     .register_alias("otherB", "b").unwrap();
/// registry.register_command("commands", Box::new(command_tree)).unwrap();
/// # }
/// ```
#[derive(Debug, Default)]
pub struct CommandTree {
    commands: HashMap<String, CommandEntry>,
}

impl CommandTree {
    fn get(&self, key: &String) -> Option<&Box<dyn Command>> {
        let entry = self.commands.get(key)?;
        match entry {
            CommandEntry::COMMAND(command) => Some(command),
            CommandEntry::ALIAS(alias) => self.get(alias),
        }
    }
}

impl CommandRegistry for CommandTree {
    fn register_command<S: Into<String>>(
        &mut self,
        key: S,
        command: Box<dyn Command>,
    ) -> Result<&mut Self, CommandRegisterError> {
        let key: String = key.into();

        if self.commands.contains_key(&key) {
            return Err(CommandRegisterError::CommandExists(key))
        }

        self.commands.insert(key, CommandEntry::COMMAND(command));
        Ok(self)
    }

    fn register_alias<S: Into<String>, T: Into<String>>(
        &mut self,
        alias: S,
        key: T,
    ) -> Result<&mut Self, CommandRegisterError> {
        let alias: String = alias.into();
        let key: String = key.into();

        if self.commands.contains_key(&alias) {
            return Err(CommandRegisterError::CommandExists(alias))
        } else if !self.commands.contains_key(&key) {
            return Err(CommandRegisterError::AliasTargetMissing(key))
        }

        self.commands.insert(alias, CommandEntry::ALIAS(key));
        Ok(self)
    }
}

impl Command for CommandTree {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
        if parameters.len() < 1 {
            return Err(CommandError::InvalidParameter("".into()))
        }

        match self.get(&parameters[0]) {
            Some(command) => command.handle(&parameters[1..]),
            None => Err(CommandError::InvalidParameter(parameters[0].clone())),
        }
    }

    fn options(&self, parameters: &[String]) -> Vec<String> {
        if parameters.len() < 1 {
            return Vec::new()
        } else if parameters.len() == 1 {
            let mut options = self.commands.keys()
                .filter(|key| key.starts_with(&parameters[0]))
                .map(|key| key.clone())
                .collect::<Vec<_>>();
            options.sort();
            return options;
        }

        match self.get(&parameters[0]) {
            Some(command) => command.options(&parameters[1..]),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::{Display};
    use std::ptr;
    use parking_lot::RwLock;
    use super::*;

    #[derive(Default, Debug)]
    struct StoreCommand {
        value: RwLock<Option<String>>,
    }

    impl Command for StoreCommand {
        fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
            ParamCountCheck::Range{min: 1, max: 2}.check(parameters.len() as u32)?;

            match parameters[0].as_str() {
                "set" => {
                    ParamCountCheck::Equal(2).check(parameters.len() as u32)?;
                    *self.value.write() = Some(parameters[1].clone());
                    Ok(())
                },
                "clear" => {
                    ParamCountCheck::Equal(1).check(parameters.len() as u32)?;
                    *self.value.write() = None;
                    Ok(())
                },
                _ => Err(CommandError::InvalidParameter(parameters[0].clone())),
            }
        }

        fn options(&self, parameters: &[String]) -> Vec<String> {
            match parameters.len() {
                1 => ["set".into(), "clear".into()].into_iter()
                    .filter(|val: &String| val.starts_with(&parameters[0]))
                    .collect(),
                _ => vec![],
            }
        }
    }

    #[test]
    fn test_command_options() {
        let command = StoreCommand::default();

        assert_eq!(
            command.options(&["".into()]),
            vec!["set".to_owned(), "clear".to_owned()],
        );
        assert_eq!(
            command.options(&["set".into()]),
            vec!["set".to_owned()],
        );
        assert_eq!(
            command.options(&["cle".into()]),
            vec!["clear".to_owned()],
        );
        assert_eq!(
            command.options(&["invalid".into()]),
            Vec::<String>::new(),
        );
        assert_eq!(
            command.options(&["clear".into(), "extra".into()]),
            Vec::<String>::new(),
        )
    }

    #[test]
    fn test_command_success() {
        let command = StoreCommand::default();

        assert_eq!(command.value.read().clone(), None);

        command.handle(&["set".into(), "a".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.read().clone(), Some("a".to_owned()));

        command.handle(&["set".into(), "b".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.read().clone(), Some("b".to_owned()));

        command.handle(&["clear".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.read().clone(), None);
    }

    #[test]
    fn test_command_invalid_params() {
        let command = StoreCommand::default();

        assert_matches!(
            command.handle(&[])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 0, compare_op: ParamCountCheck::Range{min: 1, max: 2}}
        );

        assert_matches!(
            command.handle(&["invalid".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameter(param) => {
                assert_eq!(param, "invalid");
            }
        );

        assert_matches!(
            command.handle(&["set".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 1, compare_op: ParamCountCheck::Equal(2)}
        );

        assert_matches!(
            command.handle(&["set".into(), "val".into(), "extra".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 3, compare_op: ParamCountCheck::Range{min: 1, max: 2}}
        );

        assert_matches!(
            command.handle(&["clear".into(), "extra".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 2, compare_op: ParamCountCheck::Equal(1)}
        );
    }

    #[derive(Debug)]
    struct CommandFailure;

    impl Display for CommandFailure {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            write!(f, "Command failed")
        }
    }

    impl Error for CommandFailure {}

    #[derive(Default, Debug)]
    struct FailCommand;

    impl Command for FailCommand {
        fn handle(&self, _parts: &[String]) -> Result<(), CommandError> {
            Err(CommandError::CommandFailure(Box::new(CommandFailure {})))
        }
    }

    #[test]
    fn test_command_failure() {
        let command = FailCommand::default();

        assert_matches!(
            command.handle(&[])
                .expect_err("Command should fail"),
            CommandError::CommandFailure(error) => {
                assert_eq!(error.to_string(), "Command failed".to_owned());
            }
        )
    }

    #[derive(Default, Debug)]
    struct NoOpCommand;

    impl Command for NoOpCommand {
        fn handle(&self, _parts: &[String]) -> Result<(), CommandError> {
            Ok(())
        }
    }

    fn create_command_map() -> CommandTree {
        let mut map = CommandTree::default();
        map.register_command("noop", Box::new(NoOpCommand::default()))
            .expect("Command should register")
            .register_alias("noop2", "noop")
            .expect("Alias to command should register")
            .register_alias("noop3", "noop2")
            .expect("Alias to alias should register");
        map
    }

    fn create_nested_command_map() -> CommandTree {
        let sub_map = create_command_map();

        let mut map = CommandTree::default();
        map.register_command("sub", Box::new(sub_map))
            .expect("Subcommand map should register");
        map.register_command("fail", Box::new(FailCommand::default()))
            .expect("Command should register");

        map
    }

    #[test]
    fn test_command_map_register_failure() {
        let mut commands = create_command_map();

        assert_matches!(
            commands.register_command("noop", Box::new(NoOpCommand::default()))
                .expect_err("Register should fail"),
            CommandRegisterError::CommandExists(command) => {
                assert_eq!(command, "noop");
            }
        );

        assert_matches!(
            commands.register_command("noop2", Box::new(NoOpCommand::default()))
                .expect_err("Register should fail"),
            CommandRegisterError::CommandExists(command) => {
                assert_eq!(command, "noop2");
            }
        );

        assert_matches!(
            commands.register_alias("new-alias", "non-existent")
                .expect_err("Register should fail"),
            CommandRegisterError::AliasTargetMissing(command) => {
                assert_eq!(command, "non-existent");
            }
        );
    }

    #[test]
    fn test_command_map_alias() {
        let commands = create_command_map();

        let command = commands.get(&"noop".into())
            .expect("Command should exist");
        let alias = commands.get(&"noop2".into())
            .expect("Alias should exist");
        let alias2 = commands.get(&"noop3".into())
            .expect("Alias to alias should exist");

        assert!(ptr::eq(command, alias));
        assert!(ptr::eq(command, alias2));
    }

    #[test]
    fn test_command_map_options() {
        let commands = create_nested_command_map();


        assert_eq!(
            commands.options(&["".into()]),
            vec!["fail".to_owned(), "sub".to_owned()],
        );
        assert_eq!(
            commands.options(&["sub".into()]),
            vec!["sub".to_owned()],
        );
        assert_eq!(
            commands.options(&["fa".into()]),
            vec!["fail".to_owned()],
        );
        assert_eq!(
            commands.options(&["invalid".into()]),
            Vec::<String>::new(),
        );
        assert_eq!(
            commands.options(&["sub".into(), "".into()]),
            vec!["noop".to_owned(), "noop2".to_owned(), "noop3".to_owned()],
        );
        assert_eq!(
            commands.options(&["sub".into(), "noo".into()]),
            vec!["noop".to_owned(), "noop2".to_owned(), "noop3".to_owned()],
        );
        assert_eq!(
            commands.options(&["sub".into(), "noop2".into()]),
            vec!["noop2".to_owned()],
        );
        assert_eq!(
            commands.options(&["sub".into(), "invalid".into()]),
            Vec::<String>::new(),
        );
        assert_eq!(
            commands.options(&["invalid".into(), "invalid".into()]),
            Vec::<String>::new(),
        );
    }

    #[test]
    fn test_command_map_handle() {
        let commands = create_nested_command_map();

        commands.handle(&["sub".into(), "noop".into()])
            .expect("Command should succeed");
        commands.handle(&["sub".into(), "noop2".into()])
            .expect("Command should succeed");
        commands.handle(&["sub".into(), "noop3".into()])
            .expect("Command should succeed");
        assert_matches!(
            commands.handle(&["sub".into(), "invalid".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameter(param) => {
                assert_eq!(param, "invalid");
            }
        );

        assert_matches!(
            commands.handle(&["fail".into()])
                .expect_err("Command should fail"),
            CommandError::CommandFailure(error) => {
                assert_eq!(error.to_string(), "Command failed".to_owned());
            }
        );
        assert_matches!(
            commands.handle(&["invalid".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameter(param) => {
                assert_eq!(param, "invalid");
            }
        );
    }
}