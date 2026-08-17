use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    Cli,
    Configuration,
    Environment,
    Platform,
    Pty,
    Terminal,
    Text,
    Renderer,
    Internal,
    Remote,
}

impl ErrorCategory {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Platform | Self::Renderer => 3,
            Self::Internal => 4,
            Self::Remote => 5,
            Self::Cli
            | Self::Configuration
            | Self::Environment
            | Self::Pty
            | Self::Terminal
            | Self::Text => 2,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Configuration => "configuration",
            Self::Environment => "environment",
            Self::Platform => "platform",
            Self::Pty => "pty",
            Self::Terminal => "terminal",
            Self::Text => "text",
            Self::Renderer => "renderer",
            Self::Internal => "internal",
            Self::Remote => "ssh",
        }
    }
}

pub trait ClassifiedError: std::error::Error {
    fn category(&self) -> ErrorCategory;
}

#[must_use]
pub fn render_error(error: &dyn ClassifiedError, verbose: bool) -> String {
    let mut report = format!("leyline: {} error: {}", error.category().label(), error);
    if verbose {
        let mut source = error.source();
        while let Some(cause) = source {
            let _ = write!(report, "\n  caused by: {cause}");
            source = cause.source();
        }
    }
    report.push('\n');
    report
}

#[must_use]
pub fn escape_diagnostic(value: &str) -> String {
    value.chars().flat_map(char::escape_default).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_terminal_controls() {
        assert_eq!(escape_diagnostic("bad\n\x1b[31m"), "bad\\n\\u{1b}[31m");
    }
}
