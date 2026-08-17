use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

pub const MAX_OSC7_URI_BYTES: usize = 4096;
const TRACKER_CAPACITY: usize = MAX_OSC7_URI_BYTES + 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CwdReport {
    Set(Vec<u8>),
    Invalid(CwdRejectReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CwdRejectReason {
    TooLong,
    InvalidEncoding,
    InvalidUri,
    UnsupportedScheme,
    RemoteAuthority,
    InvalidPath,
}

impl CwdRejectReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "too_long",
            Self::InvalidEncoding => "invalid_encoding",
            Self::InvalidUri => "invalid_uri",
            Self::UnsupportedScheme => "unsupported_scheme",
            Self::RemoteAuthority => "remote_authority",
            Self::InvalidPath => "invalid_path",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCwdHint {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalIdentity {
    pub hostname: Option<String>,
}

impl LocalIdentity {
    #[must_use]
    pub fn capture() -> Self {
        let hostname = rustix::system::uname().nodename().to_bytes().to_owned();
        Self {
            hostname: normalize_hostname(&hostname),
        }
    }

    #[must_use]
    pub fn new(hostname: Option<String>) -> Self {
        Self {
            hostname: hostname.and_then(|value| normalize_hostname(value.as_bytes())),
        }
    }
}

fn normalize_hostname(raw: &[u8]) -> Option<String> {
    let raw = raw.strip_suffix(b".").unwrap_or(raw);
    (!raw.is_empty() && raw.is_ascii())
        .then(|| String::from_utf8(raw.to_ascii_lowercase()).expect("ASCII hostname"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackerState {
    Ground,
    Escape,
    OscCommand,
    OscSeven,
    IgnoreOsc,
    IgnoreOscEscape,
    Uri,
    UriEscape,
    Discard,
    DiscardEscape,
}

pub struct CwdTracker {
    state: TrackerState,
    uri: Vec<u8>,
    pending: Option<CwdReport>,
    overwritten: u64,
}

impl Default for CwdTracker {
    fn default() -> Self {
        Self {
            state: TrackerState::Ground,
            uri: Vec::with_capacity(TRACKER_CAPACITY),
            pending: None,
            overwritten: 0,
        }
    }
}

impl CwdTracker {
    pub fn advance(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.advance_byte(byte);
        }
    }

    pub fn take_report(&mut self) -> Option<CwdReport> {
        self.pending.take()
    }

    #[must_use]
    pub fn storage_len(&self) -> usize {
        self.uri.len()
    }

    #[must_use]
    pub const fn overwritten_reports(&self) -> u64 {
        self.overwritten
    }

    fn publish(&mut self, report: CwdReport) {
        if self.pending.replace(report).is_some() {
            self.overwritten = self.overwritten.saturating_add(1);
        }
    }

    #[allow(clippy::match_same_arms, clippy::unnested_or_patterns)]
    fn advance_byte(&mut self, byte: u8) {
        use TrackerState::{
            Discard, DiscardEscape, Escape, Ground, IgnoreOsc, IgnoreOscEscape, OscCommand,
            OscSeven, Uri, UriEscape,
        };
        self.state = match (self.state, byte) {
            (Ground, 0x1b) => Escape,
            (Ground, _) => Ground,
            (Escape, b']') => OscCommand,
            (Escape, 0x1b) => Escape,
            (Escape, _) => Ground,
            (OscCommand, b'7') => OscSeven,
            (OscCommand, 0x07) | (OscCommand, 0x18 | 0x1a) => Ground,
            (OscCommand, 0x1b) => IgnoreOscEscape,
            (OscCommand, _) => IgnoreOsc,
            (OscSeven, b';') => {
                self.uri.clear();
                Uri
            }
            (OscSeven, 0x07) | (OscSeven, 0x18 | 0x1a) => Ground,
            (OscSeven, 0x1b) => IgnoreOscEscape,
            (OscSeven, _) => IgnoreOsc,
            (IgnoreOsc, 0x07) | (IgnoreOsc, 0x18 | 0x1a) => Ground,
            (IgnoreOsc, 0x1b) => IgnoreOscEscape,
            (IgnoreOsc, _) => IgnoreOsc,
            (IgnoreOscEscape, b'\\') => Ground,
            (IgnoreOscEscape, _) => Ground,
            (Uri, 0x07) => {
                let uri = std::mem::take(&mut self.uri);
                self.publish(CwdReport::Set(uri));
                self.uri = Vec::with_capacity(TRACKER_CAPACITY);
                Ground
            }
            (Uri, 0x18 | 0x1a) => {
                self.uri.clear();
                Ground
            }
            (Uri, 0x1b) => UriEscape,
            (Uri, value) => {
                self.uri.push(value);
                if self.uri.len() > MAX_OSC7_URI_BYTES {
                    Discard
                } else {
                    Uri
                }
            }
            (UriEscape, b'\\') => {
                let uri = std::mem::take(&mut self.uri);
                self.publish(CwdReport::Set(uri));
                self.uri = Vec::with_capacity(TRACKER_CAPACITY);
                Ground
            }
            (UriEscape, _) => {
                self.uri.clear();
                Ground
            }
            (Discard, 0x07) => {
                self.uri.clear();
                self.publish(CwdReport::Invalid(CwdRejectReason::TooLong));
                Ground
            }
            (Discard, 0x18 | 0x1a) => {
                self.uri.clear();
                Ground
            }
            (Discard, 0x1b) => DiscardEscape,
            (Discard, _) => Discard,
            (DiscardEscape, b'\\') => {
                self.uri.clear();
                self.publish(CwdReport::Invalid(CwdRejectReason::TooLong));
                Ground
            }
            (DiscardEscape, _) => {
                self.uri.clear();
                Ground
            }
        };
    }
}

/// Validates a completed tracker report against the frozen local identity.
///
/// # Errors
/// Returns the stable rejection reason when the report is invalid or non-local.
pub fn validate_report(
    report: CwdReport,
    identity: &LocalIdentity,
) -> Result<LocalCwdHint, CwdRejectReason> {
    let CwdReport::Set(uri) = report else {
        let CwdReport::Invalid(reason) = report else {
            unreachable!()
        };
        return Err(reason);
    };
    validate_uri(&uri, identity)
}

fn validate_uri(uri: &[u8], identity: &LocalIdentity) -> Result<LocalCwdHint, CwdRejectReason> {
    if !uri.is_ascii() && std::str::from_utf8(uri).is_err() {
        return Err(CwdRejectReason::InvalidEncoding);
    }
    let colon = uri
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(CwdRejectReason::InvalidUri)?;
    if !uri[..colon].eq_ignore_ascii_case(b"file") {
        return Err(CwdRejectReason::UnsupportedScheme);
    }
    let remainder = &uri[colon + 1..];
    if !remainder.starts_with(b"//") {
        return Err(CwdRejectReason::InvalidUri);
    }
    let authority_and_path = &remainder[2..];
    let slash = authority_and_path
        .iter()
        .position(|byte| *byte == b'/')
        .ok_or(CwdRejectReason::InvalidPath)?;
    let authority = &authority_and_path[..slash];
    let raw_path = &authority_and_path[slash..];
    validate_authority(authority, identity)?;
    let decoded = decode_path(raw_path)?;
    let path = PathBuf::from(OsString::from_vec(decoded));
    if !path.is_absolute() {
        return Err(CwdRejectReason::InvalidPath);
    }
    Ok(LocalCwdHint { path })
}

fn validate_authority(authority: &[u8], identity: &LocalIdentity) -> Result<(), CwdRejectReason> {
    if !authority.is_ascii()
        || authority.iter().any(|byte| {
            *byte <= 0x20
                || *byte == 0x7f
                || matches!(byte, b'%' | b'@' | b':' | b'[' | b']' | b'?' | b'#' | b'\\')
        })
    {
        return Err(CwdRejectReason::InvalidUri);
    }
    let normalized = authority.strip_suffix(b".").unwrap_or(authority);
    let local = normalized.is_empty()
        || normalized.eq_ignore_ascii_case(b"localhost")
        || identity
            .hostname
            .as_ref()
            .is_some_and(|hostname| normalized.eq_ignore_ascii_case(hostname.as_bytes()));
    local.then_some(()).ok_or(CwdRejectReason::RemoteAuthority)
}

fn decode_path(raw: &[u8]) -> Result<Vec<u8>, CwdRejectReason> {
    if raw.is_empty() || (!raw.is_ascii() && std::str::from_utf8(raw).is_err()) {
        return Err(CwdRejectReason::InvalidEncoding);
    }
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if byte == b'%' {
            let high = raw.get(index + 1).and_then(|byte| hex(*byte));
            let low = raw.get(index + 2).and_then(|byte| hex(*byte));
            let value = high
                .zip(low)
                .map(|(high, low)| high << 4 | low)
                .ok_or(CwdRejectReason::InvalidPath)?;
            if value == 0 {
                return Err(CwdRejectReason::InvalidPath);
            }
            decoded.push(value);
            index += 3;
            continue;
        }
        if byte <= 0x20 || byte == 0x7f || matches!(byte, b'?' | b'#') {
            return Err(CwdRejectReason::InvalidPath);
        }
        decoded.push(byte);
        index += 1;
    }
    Ok(decoded)
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[must_use]
pub fn valid_absolute_env_path(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    (!path.as_os_str().is_empty()
        && path.is_absolute()
        && !path.as_os_str().as_bytes().contains(&0))
    .then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_handles_terminators_splits_and_literal_semicolon() {
        let mut tracker = CwdTracker::default();
        tracker.advance(b"noise\x1b]7;file:///tmp/a;b\x1b");
        assert_eq!(tracker.take_report(), None);
        tracker.advance(b"\\");
        assert_eq!(
            tracker.take_report(),
            Some(CwdReport::Set(b"file:///tmp/a;b".to_vec()))
        );
        tracker.advance(b"\x1b]7;file:///tmp/c\x07");
        assert_eq!(
            tracker.take_report(),
            Some(CwdReport::Set(b"file:///tmp/c".to_vec()))
        );
    }

    #[test]
    fn tracker_is_bounded_and_recovers_after_overflow() {
        let mut tracker = CwdTracker::default();
        let mut input = b"\x1b]7;".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_OSC7_URI_BYTES + 1));
        input.push(0x07);
        tracker.advance(&input);
        assert!(tracker.storage_len() <= TRACKER_CAPACITY);
        assert_eq!(
            tracker.take_report(),
            Some(CwdReport::Invalid(CwdRejectReason::TooLong))
        );
        tracker.advance(b"\x1b]7;file:///ok\x07");
        assert!(matches!(tracker.take_report(), Some(CwdReport::Set(_))));
    }

    #[test]
    fn tracker_accepts_exact_limit_and_keeps_only_the_latest_report() {
        let mut tracker = CwdTracker::default();
        let mut input = b"\x1b]7;".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_OSC7_URI_BYTES));
        input.push(0x07);
        input.extend_from_slice(b"\x1b]7;file:///latest\x07");
        tracker.advance(&input);
        assert_eq!(
            tracker.take_report(),
            Some(CwdReport::Set(b"file:///latest".to_vec()))
        );
        assert_eq!(tracker.overwritten_reports(), 1);
        assert!(tracker.storage_len() <= TRACKER_CAPACITY);
    }

    #[test]
    fn cancellation_preserves_the_previous_pending_report() {
        let mut tracker = CwdTracker::default();
        tracker.advance(b"\x1b]7;file:///old\x07\x1b]7;partial\x18");
        assert_eq!(
            tracker.take_report(),
            Some(CwdReport::Set(b"file:///old".to_vec()))
        );
    }

    #[test]
    fn validator_enforces_local_file_uri_and_preserves_bytes() {
        let identity = LocalIdentity::new(Some("Workstation".into()));
        let hint = validate_report(
            CwdReport::Set(b"FiLe://workstation./tmp/a%20b/%FF;x".to_vec()),
            &identity,
        )
        .unwrap();
        assert_eq!(hint.path.as_os_str().as_bytes(), b"/tmp/a b/\xff;x");
        assert_eq!(
            validate_report(CwdReport::Set(b"file://remote/tmp".to_vec()), &identity),
            Err(CwdRejectReason::RemoteAuthority)
        );
        assert_eq!(
            validate_report(CwdReport::Set(b"file:///tmp?q".to_vec()), &identity),
            Err(CwdRejectReason::InvalidPath)
        );
    }

    #[test]
    fn any_chunking_produces_the_same_report() {
        let input = b"prefix\x1b]7;file:///tmp/a%20b\x1b\\suffix";
        for split in 0..=input.len() {
            let mut tracker = CwdTracker::default();
            tracker.advance(&input[..split]);
            tracker.advance(&input[split..]);
            assert_eq!(
                tracker.take_report(),
                Some(CwdReport::Set(b"file:///tmp/a%20b".to_vec()))
            );
        }
    }
}
