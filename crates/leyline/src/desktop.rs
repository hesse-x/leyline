use std::{
    process::{Command, Stdio},
    sync::mpsc::{SyncSender, TrySendError, sync_channel},
};

const MAX_URI_BYTES: usize = 4096;

#[allow(clippy::missing_errors_doc)]
pub fn validate_uri(uri: &str) -> Result<&str, DesktopError> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES || uri.chars().any(char::is_control) {
        return Err(DesktopError::InvalidUri);
    }
    let (scheme, rest) = uri.split_once(':').ok_or(DesktopError::InvalidUri)?;
    if rest.is_empty()
        || !scheme.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphabetic()
                || (index != 0 && (byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')))
        })
    {
        return Err(DesktopError::InvalidUri);
    }
    if !matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto"
    ) {
        return Err(DesktopError::DisallowedScheme);
    }
    Ok(uri)
}

#[allow(clippy::missing_errors_doc)]
pub struct DesktopLauncher {
    sender: SyncSender<String>,
}

impl DesktopLauncher {
    /// Starts one bounded desktop worker so process creation and reaping never block the UI.
    ///
    /// # Panics
    /// Panics if the operating system cannot create the worker thread.
    #[must_use]
    pub fn new() -> Self {
        let (sender, receiver) = sync_channel::<String>(4);
        std::thread::Builder::new()
            .name("leyline-desktop".into())
            .spawn(move || {
                while let Ok(uri) = receiver.recv() {
                    let result = Command::new("xdg-open")
                        .arg(uri)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .and_then(|mut child| child.wait());
                    if let Err(error) = result {
                        tracing::warn!(%error, "desktop URI opener failed");
                    }
                }
            })
            .expect("desktop worker thread creation must succeed");
        Self { sender }
    }

    /// Queues one validated URI without blocking the UI thread.
    ///
    /// # Errors
    /// Returns a typed error for an invalid URI, a full queue, or a stopped worker.
    pub fn open(&self, uri: &str) -> Result<(), DesktopError> {
        let uri = validate_uri(uri)?.to_owned();
        self.sender.try_send(uri).map_err(|error| match error {
            TrySendError::Full(_) => DesktopError::QueueFull,
            TrySendError::Disconnected(_) => DesktopError::WorkerStopped,
        })
    }
}

impl Default for DesktopLauncher {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("URI is not a bounded absolute URI")]
    InvalidUri,
    #[error("URI scheme is not allowed")]
    DisallowedScheme,
    #[error("desktop opener queue is full")]
    QueueFull,
    #[error("desktop opener worker has stopped")]
    WorkerStopped,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_safe_schemes_are_accepted() {
        assert_eq!(
            validate_uri("HTTPS://example.com/a?b=c").unwrap(),
            "HTTPS://example.com/a?b=c"
        );
        assert!(matches!(
            validate_uri("file:///etc/passwd"),
            Err(DesktopError::DisallowedScheme)
        ));
        assert!(matches!(
            validate_uri("https://ok\n--bad"),
            Err(DesktopError::InvalidUri)
        ));
        assert!(validate_uri("example.com").is_err());
    }
}
