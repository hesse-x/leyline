use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgAction, Command, builder::OsStringValueParser, error::ErrorKind};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub command: Option<CommandSpec>,
    pub verbosity: Verbosity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchRequest {
    DefaultShell,
    Command(CommandSpec),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Verbosity {
    #[default]
    Warn,
    Info,
    Debug,
    Trace,
}

impl Cli {
    #[must_use]
    pub fn launch_request(&self) -> LaunchRequest {
        self.command
            .clone()
            .map_or(LaunchRequest::DefaultShell, LaunchRequest::Command)
    }
}

#[derive(Debug)]
pub enum ParseOutcome {
    Run(Cli),
    Print { text: String, success: bool },
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> ParseOutcome {
    let command = Command::new("leyline")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A fast native Wayland terminal")
        .disable_help_subcommand(true)
        .arg(
            Arg::new("verbose")
                .short('v')
                .action(ArgAction::Count)
                .help("Increase logging verbosity (repeat up to three times)"),
        )
        .arg(
            Arg::new("execute")
                .short('e')
                .value_name("PROGRAM [ARG]...")
                .value_parser(OsStringValueParser::new())
                .num_args(1..)
                .allow_hyphen_values(true)
                .trailing_var_arg(true)
                .help("Run a command directly in the terminal PTY"),
        );

    match command.try_get_matches_from(args) {
        Ok(matches) => {
            let verbosity = match matches.get_count("verbose") {
                0 => Verbosity::Warn,
                1 => Verbosity::Info,
                2 => Verbosity::Debug,
                _ => Verbosity::Trace,
            };
            let mut values = matches
                .get_many::<OsString>("execute")
                .into_iter()
                .flatten()
                .cloned();
            let mut program = values.next();
            if program.as_deref() == Some(OsStr::new("--")) {
                program = values.next();
            }
            if matches.contains_id("execute") && program.is_none() {
                return ParseOutcome::Print {
                    success: false,
                    text: "error: the argument '-e <PROGRAM [ARG]...>' requires a program\n\nUsage: leyline -e <PROGRAM [ARG]...>\n"
                        .into(),
                };
            }
            let command = program.map(|program| CommandSpec {
                program,
                args: values.collect(),
            });
            ParseOutcome::Run(Cli { command, verbosity })
        }
        Err(error) => ParseOutcome::Print {
            success: matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ),
            text: error.to_string(),
        },
    }
}

pub fn display_os(value: &OsStr) -> String {
    let escaped = value
        .to_string_lossy()
        .chars()
        .flat_map(char::escape_default)
        .collect::<String>();
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Cli {
        match parse(args.iter().map(OsString::from)) {
            ParseOutcome::Run(cli) => cli,
            ParseOutcome::Print { text, .. } => panic!("unexpected parse result: {text}"),
        }
    }

    #[test]
    fn preserves_execute_argv_and_hyphen_values() {
        let cli = run(&["leyline", "-e", "printf", "a b", "--help", "-v"]);
        assert_eq!(
            cli.command,
            Some(CommandSpec {
                program: "printf".into(),
                args: vec!["a b".into(), "--help".into(), "-v".into()],
            })
        );
        assert_eq!(cli.verbosity, Verbosity::Warn);
    }

    #[test]
    fn optional_separator_is_not_a_program() {
        assert_eq!(
            run(&["leyline", "-e", "--", "echo", "ok"]).command,
            run(&["leyline", "-e", "echo", "ok"]).command
        );
    }

    #[test]
    fn separator_without_program_is_an_error() {
        assert!(matches!(
            parse(["leyline", "-e", "--"].map(OsString::from)),
            ParseOutcome::Print { success: false, .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_non_utf8_command_arguments() {
        use std::os::unix::ffi::OsStringExt;
        let invalid = OsString::from_vec(vec![b'x', 0xff]);
        let args = vec![
            OsString::from("leyline"),
            "-e".into(),
            "echo".into(),
            invalid.clone(),
        ];
        let ParseOutcome::Run(cli) = parse(args) else {
            panic!("parse failed")
        };
        assert_eq!(cli.command.expect("command").args, vec![invalid]);
    }
}
