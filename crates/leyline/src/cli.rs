use std::ffi::{OsStr, OsString};

use clap::{Arg, ArgAction, Command, builder::OsStringValueParser, error::ErrorKind};

use crate::terminfo::TerminalIdentity;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cli {
    pub command: Option<CommandSpec>,
    pub verbosity: Verbosity,
    pub terminal_identity: Option<TerminalIdentity>,
    pub operation: Operation,
    pub window: WindowOverrides,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowOverrides {
    pub geometry: Option<WindowGeometry>,
    pub startup_state: Option<crate::config::StartupWindowState>,
    pub new_window: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowGeometry {
    pub columns: u16,
    pub lines: u16,
}

fn parse_geometry(value: &str) -> Result<WindowGeometry, String> {
    if value.is_empty() || !value.is_ascii() || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err("geometry must use ASCII COLUMNSxLINES without whitespace".into());
    }
    let (columns, lines) = value
        .split_once('x')
        .ok_or_else(|| "geometry must use COLUMNSxLINES".to_string())?;
    if columns.is_empty() || lines.is_empty() || lines.contains('x') {
        return Err("geometry must use COLUMNSxLINES".into());
    }
    let columns = columns
        .parse::<u32>()
        .map_err(|_| "columns must be an unsigned decimal integer".to_string())?;
    let lines = lines
        .parse::<u32>()
        .map_err(|_| "lines must be an unsigned decimal integer".to_string())?;
    if !(u32::from(crate::config::MIN_COLUMNS)..=u32::from(crate::config::MAX_COLUMNS))
        .contains(&columns)
    {
        return Err(format!(
            "columns must be in {}..={}",
            crate::config::MIN_COLUMNS,
            crate::config::MAX_COLUMNS
        ));
    }
    if !(u32::from(crate::config::MIN_LINES)..=u32::from(crate::config::MAX_LINES)).contains(&lines)
    {
        return Err(format!(
            "lines must be in {}..={}",
            crate::config::MIN_LINES,
            crate::config::MAX_LINES
        ));
    }
    Ok(WindowGeometry {
        columns: u16::try_from(columns).expect("validated columns fit u16"),
        lines: u16::try_from(lines).expect("validated lines fit u16"),
    })
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
            Arg::new("geometry")
                .long("geometry")
                .value_name("COLUMNSxLINES")
                .value_parser(parse_geometry)
                .help("Request the initial terminal grid (20..500 columns, 5..300 lines)"),
        )
        .arg(
            Arg::new("maximized")
                .long("maximized")
                .conflicts_with("fullscreen")
                .action(ArgAction::SetTrue)
                .help("Request an initially maximized window"),
        )
        .arg(
            Arg::new("fullscreen")
                .long("fullscreen")
                .conflicts_with("maximized")
                .action(ArgAction::SetTrue)
                .help("Request an initially fullscreen window"),
        )
        .arg(
            Arg::new("new-window")
                .long("new-window")
                .action(ArgAction::SetTrue)
                .help("Launch this process with a new terminal window"),
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
            let has_launch_window_option = matches.contains_id("geometry")
                || matches.get_flag("maximized")
                || matches.get_flag("fullscreen")
                || matches.get_flag("new-window");
            if operation != Operation::Launch && (command.is_some() || has_launch_window_option) {
                return ParseOutcome::Print {
                    success: false,
                    text: "error: launch options cannot be combined with a management subcommand\n"
                        .into(),
                };
            }
            let startup_state = if matches.get_flag("fullscreen") {
                Some(crate::config::StartupWindowState::Fullscreen)
            } else if matches.get_flag("maximized") {
                Some(crate::config::StartupWindowState::Maximized)
            } else {
                None
            };
            ParseOutcome::Run(Cli {
                command,
                verbosity,
                terminal_identity,
                operation,
                window: WindowOverrides {
                    geometry: matches.get_one::<WindowGeometry>("geometry").copied(),
                    startup_state,
                    new_window: matches.get_flag("new-window"),
                },
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

    #[test]
    fn parses_strict_geometry_and_window_state_overrides() {
        let cli = run(&[
            "leyline",
            "--geometry",
            "120x36",
            "--fullscreen",
            "--new-window",
        ]);
        assert_eq!(
            cli.window,
            WindowOverrides {
                geometry: Some(WindowGeometry {
                    columns: 120,
                    lines: 36,
                }),
                startup_state: Some(crate::config::StartupWindowState::Fullscreen),
                new_window: true,
            }
        );
        for geometry in ["0x24", "80x0", "80X24", " 80x24", "80x24+0+0", "501x24"] {
            assert!(matches!(
                parse(["leyline", "--geometry", geometry].map(OsString::from)),
                ParseOutcome::Print { success: false, .. }
            ));
        }
        assert!(matches!(
            parse(["leyline", "--maximized", "--fullscreen"].map(OsString::from)),
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
