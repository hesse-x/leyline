mod environment;
mod report;
mod terminal;
mod text;
mod vulkan;
mod wayland;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use report::{ProbeError, ProbeResult, Reporter};

#[derive(Clone, Copy)]
enum Command {
    Environment,
    Terminal,
    Text,
    Wayland,
    Vulkan,
    All,
}

struct Options {
    command: Command,
    json: bool,
    verbose: bool,
    font: Option<String>,
    terminal_fixture: Option<PathBuf>,
    wayland_interactive_seconds: Option<u64>,
}

fn usage() -> &'static str {
    "Usage: fastterm-probe <environment|terminal|text|wayland|vulkan|all> \
     [--verbose] [--json] [--font PATTERN] [--terminal-fixture PATH] \
     [--wayland-interactive-seconds SECONDS]"
}

fn parse_args() -> Result<Options, ProbeError> {
    let mut args = env::args_os().skip(1);
    let command = match args
        .next()
        .and_then(|value| value.into_string().ok())
        .as_deref()
    {
        Some("environment") => Command::Environment,
        Some("terminal") => Command::Terminal,
        Some("text") => Command::Text,
        Some("wayland") => Command::Wayland,
        Some("vulkan") => Command::Vulkan,
        Some("all") => Command::All,
        _ => return Err(ProbeError::internal("cli", usage())),
    };
    let mut options = Options {
        command,
        json: false,
        verbose: false,
        font: None,
        terminal_fixture: None,
        wayland_interactive_seconds: None,
    };
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--json") => options.json = true,
            Some("--verbose") => options.verbose = true,
            Some("--font") => {
                options.font = Some(next_utf8(&mut args, "--font")?);
            }
            Some("--terminal-fixture") => {
                options.terminal_fixture =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        ProbeError::internal("cli", "missing fixture path")
                    })?));
            }
            Some("--wayland-interactive-seconds") => {
                let value = next_utf8(&mut args, "--wayland-interactive-seconds")?;
                options.wayland_interactive_seconds = Some(value.parse().map_err(|_| {
                    ProbeError::internal(
                        "cli",
                        "--wayland-interactive-seconds requires a positive integer",
                    )
                })?);
            }
            _ => return Err(ProbeError::internal("cli", usage())),
        }
    }
    Ok(options)
}

fn next_utf8(
    args: &mut impl Iterator<Item = std::ffi::OsString>,
    flag: &str,
) -> ProbeResult<String> {
    args.next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| ProbeError::internal("cli", format!("{flag} requires UTF-8 text")))
}

fn run(options: &Options, reporter: &mut Reporter) -> ProbeResult<()> {
    match options.command {
        Command::Environment => environment::run(reporter),
        Command::Terminal => terminal::run(reporter, options.terminal_fixture.as_deref()),
        Command::Text => text::run(reporter, options.font.as_deref()),
        Command::Wayland => wayland::run(reporter, options.wayland_interactive_seconds),
        Command::Vulkan => vulkan::run(reporter),
        Command::All => {
            environment::run(reporter)?;
            terminal::run(reporter, options.terminal_fixture.as_deref())?;
            text::run(reporter, options.font.as_deref())?;
            wayland::run(reporter, options.wayland_interactive_seconds)?;
            vulkan::run(reporter)
        }
    }
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(error.exit_code());
        }
    };
    let mut reporter = Reporter::new(options.json, options.verbose);
    match run(&options, &mut reporter) {
        Ok(()) => {
            reporter.finish();
            ExitCode::SUCCESS
        }
        Err(error) => {
            reporter.failure(&error);
            ExitCode::from(error.exit_code())
        }
    }
}
