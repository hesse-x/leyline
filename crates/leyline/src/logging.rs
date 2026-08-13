use tracing::level_filters::LevelFilter;
use tracing_subscriber::fmt;

use crate::cli::Verbosity;

#[derive(Debug, thiserror::Error)]
pub enum LoggingError {
    #[error("the global logging subscriber is already initialized")]
    AlreadyInitialized,
}

/// Installs the process-global tracing subscriber.
///
/// # Errors
/// Returns [`LoggingError::AlreadyInitialized`] when another subscriber is installed.
pub fn initialize(verbosity: Verbosity) -> Result<(), LoggingError> {
    let level = match verbosity {
        Verbosity::Warn => LevelFilter::WARN,
        Verbosity::Info => LevelFilter::INFO,
        Verbosity::Debug => LevelFilter::DEBUG,
        Verbosity::Trace => LevelFilter::TRACE,
    };
    fmt()
        .with_max_level(level)
        .with_target(true)
        .try_init()
        .map_err(|_| LoggingError::AlreadyInitialized)
}
