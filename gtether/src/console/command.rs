use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::{Formatter};

#[allow(unused)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamCountCompareOp {
    Equal,
    AtLeast,
    AtMost,
}

#[derive(Debug)]
pub enum CommandError {
    InvalidParameter(String),
    InvalidParameterCount{ actual: u32, expected: u32, compare_op: ParamCountCompareOp },
    CommandFailure(Box<dyn Error>),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::InvalidParameter(parameter) =>
                write!(f, "Invalid command parameter: {parameter}"),
            CommandError::InvalidParameterCount {actual, expected, compare_op } => match compare_op {
                ParamCountCompareOp::Equal =>
                    write!(f, "Expected {expected} parameters; got {actual} parameters"),
                ParamCountCompareOp::AtLeast =>
                    write!(f, "Expected at least {expected} parameters; got {actual} parameters"),
                ParamCountCompareOp::AtMost =>
                    write!(f, "Expected at most {expected} parameters; got {actual} parameters"),
            },
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

impl CommandError {
    pub fn check_param_count(actual: u32, expected: u32, compare_op: ParamCountCompareOp) -> Result<(), CommandError> {
        let pass = match compare_op {
            ParamCountCompareOp::Equal => actual == expected,
            ParamCountCompareOp::AtLeast => actual >= expected,
            ParamCountCompareOp::AtMost => actual <= expected,
        };

        match pass {
            true => Ok(()),
            false => Err(CommandError::InvalidParameterCount {actual, expected, compare_op}),
        }
    }
}

pub trait Command {
    fn handle(&self, parameters: &[String]) -> Result<(), CommandError>;
    #[allow(unused)]
    fn options(&self, parameters: &[String]) -> Vec<String> {
        vec![]
    }
}

#[derive(Debug)]
pub enum CommandRegisterError {
    CommandExists(String),
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

enum CommandEntry {
    COMMAND(Box<dyn Command>),
    ALIAS(String),
}

pub struct CommandMap {
    commands: HashMap<String, CommandEntry>,
}

impl CommandMap {
    pub fn new() -> Self {
        CommandMap {
            commands: HashMap::new(),
        }
    }

    fn get(&self, key: &String) -> Option<&Box<dyn Command>> {
        let entry = self.commands.get(key)?;
        match entry {
            CommandEntry::COMMAND(command) => Some(command),
            CommandEntry::ALIAS(alias) => self.get(alias),
        }
    }

    pub fn register_command<S: Into<String>>(
        &mut self,
        key: S,
        command: Box<dyn Command>,
    ) -> Result<(), CommandRegisterError> {
        let key: String = key.into();

        if self.commands.contains_key(&key) {
            return Err(CommandRegisterError::CommandExists(key))
        }

        self.commands.insert(key, CommandEntry::COMMAND(command));
        Ok(())
    }

    pub fn register_alias<S: Into<String>, T: Into<String>>(
        &mut self,
        alias: S,
        key: T,
    ) -> Result<(), CommandRegisterError> {
        let alias: String = alias.into();
        let key: String = key.into();

        if self.commands.contains_key(&alias) {
            return Err(CommandRegisterError::CommandExists(alias))
        } else if !self.commands.contains_key(&key) {
            return Err(CommandRegisterError::AliasTargetMissing(key))
        }

        self.commands.insert(alias, CommandEntry::ALIAS(key));
        Ok(())
    }
}

impl Command for CommandMap {
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
    use std::cell::{Cell};
    use std::fmt::{Display};
    use std::ptr;
    use super::*;

    #[derive(Default)]
    struct StoreCommand {
        value: Cell<Option<String>>,
    }

    impl Command for StoreCommand {
        fn handle(&self, parameters: &[String]) -> Result<(), CommandError> {
            CommandError::check_param_count(parameters.len() as u32, 1, ParamCountCompareOp::AtLeast)?;

            match parameters[0].as_str() {
                "set" => {
                    CommandError::check_param_count(parameters.len() as u32, 2, ParamCountCompareOp::Equal)?;
                    self.value.set(Some(parameters[1].clone()));
                    Ok(())
                },
                "clear" => {
                    CommandError::check_param_count(parameters.len() as u32, 1, ParamCountCompareOp::Equal)?;
                    self.value.set(None);
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
        let mut command = StoreCommand::default();

        assert_eq!(command.value.get_mut().clone(), None);

        command.handle(&["set".into(), "a".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.get_mut().clone(), Some("a".to_owned()));

        command.handle(&["set".into(), "b".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.get_mut().clone(), Some("b".to_owned()));

        command.handle(&["clear".into()])
            .expect("Command should succeed");
        assert_eq!(command.value.get_mut().clone(), None);
    }

    #[test]
    fn test_command_invalid_params() {
        let command = StoreCommand::default();

        assert_matches!(
            command.handle(&[])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 0, expected: 1, compare_op: ParamCountCompareOp::AtLeast}
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
            CommandError::InvalidParameterCount {actual: 1, expected: 2, compare_op: ParamCountCompareOp::Equal}
        );

        assert_matches!(
            command.handle(&["set".into(), "val".into(), "extra".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 3, expected: 2, compare_op: ParamCountCompareOp::Equal}
        );

        assert_matches!(
            command.handle(&["clear".into(), "extra".into()])
                .expect_err("Command should fail"),
            CommandError::InvalidParameterCount {actual: 2, expected: 1, compare_op: ParamCountCompareOp::Equal}
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

    #[derive(Default)]
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

    #[derive(Default)]
    struct NoOpCommand;

    impl Command for NoOpCommand {
        fn handle(&self, _parts: &[String]) -> Result<(), CommandError> {
            Ok(())
        }
    }

    fn create_command_map() -> CommandMap {
        let mut map = CommandMap::new();
        map.register_command("noop", Box::new(NoOpCommand::default()))
            .expect("Command should register");
        map.register_alias("noop2", "noop")
            .expect("Alias to command should register");
        map.register_alias("noop3", "noop2")
            .expect("Alias to alias should register");

        map
    }

    fn create_nested_command_map() -> CommandMap {
        let sub_map = create_command_map();

        let mut map = CommandMap::new();
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