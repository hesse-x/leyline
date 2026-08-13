use std::process::{Command, Stdio};

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
pub fn open_uri(uri: &str) -> Result<(), DesktopError> {
    let uri = validate_uri(uri)?;
    Command::new("xdg-open")
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(DesktopError::Spawn)
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("URI is not a bounded absolute URI")]
    InvalidUri,
    #[error("URI scheme is not allowed")]
    DisallowedScheme,
    #[error("cannot start xdg-open: {0}")]
    Spawn(std::io::Error),
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
