use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgAction, Command, builder::OsStringValueParser, error::ErrorKind};

use crate::terminfo::TerminalIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub command: Option<CommandSpec>,
    pub verbosity: Verbosity,
    pub terminal_identity: Option<TerminalIdentity>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Launch,
    Terminfo(TerminfoOperation),
    Doctor(DoctorOperation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminfoOperation {
    Print,
    Check { database: Option<OsString> },
    Install { database: Option<OsString> },
    Uninstall { database: Option<OsString> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DoctorOperation {
    Terminfo,
    Ssh { host: OsString, json: bool },
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

#[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
pub fn parse(args: impl IntoIterator<Item = OsString>) -> ParseOutcome {
    let command = Command::new("leyline")
        .version(env!("CARGO_PKG_VERSION"))
        .about("A fast native Wayland terminal")
        .disable_help_subcommand(true)
        .arg(
            Arg::new("term")
                .long("term")
                .value_name("IDENTITY")
                .value_parser(["leyline", "xterm-256color"])
                .help("Select leyline (default) or xterm-256color compatibility identity"),
        )
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
        )
        .subcommand(
            Command::new("terminfo")
                .about("Inspect or manage the Leyline terminfo entry")
                .subcommand_required(true)
                .subcommand(Command::new("print"))
                .subcommand(database_command("check"))
                .subcommand(
                    database_command("install").arg(
                        Arg::new("user")
                            .long("user")
                            .required(true)
                            .action(ArgAction::SetTrue),
                    ),
                )
                .subcommand(
                    database_command("uninstall").arg(
                        Arg::new("user")
                            .long("user")
                            .required(true)
                            .action(ArgAction::SetTrue),
                    ),
                ),
        )
        .subcommand(
            Command::new("doctor")
                .about("Diagnose terminfo and tmux capability propagation")
                .subcommand_required(true)
                .subcommand(Command::new("terminfo"))
                .subcommand(
                    Command::new("ssh")
                        .arg(
                            Arg::new("host")
                                .required(true)
                                .value_parser(OsStringValueParser::new()),
                        )
                        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue)),
                ),
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
            let terminal_identity = matches
                .get_one::<String>("term")
                .and_then(|value| TerminalIdentity::parse(value));
            let operation = parse_operation(&matches);
            if operation != Operation::Launch && command.is_some() {
                return ParseOutcome::Print {
                    success: false,
                    text: "error: '-e' cannot be combined with a management subcommand\n".into(),
                };
            }
            ParseOutcome::Run(Cli {
                command,
                verbosity,
                terminal_identity,
                operation,
            })
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

fn database_command(name: &'static str) -> Command {
    Command::new(name).arg(
        Arg::new("database")
            .long("database")
            .value_name("PATH")
            .value_parser(OsStringValueParser::new()),
    )
}

fn parse_operation(matches: &clap::ArgMatches) -> Operation {
    match matches.subcommand() {
        None => Operation::Launch,
        Some(("terminfo", nested)) => {
            let (name, args) = nested.subcommand().expect("required subcommand");
            let database = || args.get_one::<OsString>("database").cloned();
            Operation::Terminfo(match name {
                "print" => TerminfoOperation::Print,
                "check" => TerminfoOperation::Check {
                    database: database(),
                },
                "install" => TerminfoOperation::Install {
                    database: database(),
                },
                "uninstall" => TerminfoOperation::Uninstall {
                    database: database(),
                },
                _ => unreachable!("clap validates terminfo subcommands"),
            })
        }
        Some(("doctor", nested)) => {
            let (name, args) = nested.subcommand().expect("required subcommand");
            Operation::Doctor(match name {
                "terminfo" => DoctorOperation::Terminfo,
                "ssh" => DoctorOperation::Ssh {
                    host: args
                        .get_one::<OsString>("host")
                        .expect("required host")
                        .clone(),
                    json: args.get_flag("json"),
                },
                _ => unreachable!("clap validates doctor subcommands"),
            })
        }
        _ => unreachable!("clap validates top-level subcommands"),
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
        assert_eq!(cli.operation, Operation::Launch);
    }

    #[test]
    fn parses_identity_and_management_commands_without_consuming_execute_argv() {
        let cli = run(&["leyline", "--term", "xterm-256color", "terminfo", "check"]);
        assert_eq!(cli.terminal_identity, Some(TerminalIdentity::Xterm256Color));
        assert_eq!(
            cli.operation,
            Operation::Terminfo(TerminfoOperation::Check { database: None })
        );
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
