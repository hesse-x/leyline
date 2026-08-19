#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppEvent {
    Pty(PtyEvent),
    ShutdownRequested(ShutdownReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlEvent {
    Shutdown(ShutdownReason),
    PtyExited(ChildExit),
    PtyFailed(PtyFailure),
    PtyWritable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BulkEvent {
    PtyOutput(ByteBatch),
    PtyReadClosed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PtyEvent {
    Output(ByteBatch),
    Exited(ChildExit),
    Failed(PtyFailure),
    ReadClosed,
    Writable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildExit {
    Code(i32),
    Signaled { signal: i32, core_dumped: bool },
    Other { raw_status: i32 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtyFailure {
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ByteBatch(Vec<u8>);

impl ByteBatch {
    pub const MAX_LEN: usize = 64 * 1024;

    /// Creates a bounded, non-empty PTY output batch.
    ///
    /// # Errors
    /// Returns [`BatchError`] when `bytes` is empty or exceeds [`Self::MAX_LEN`].
    pub fn new(bytes: Vec<u8>) -> Result<Self, BatchError> {
        if bytes.is_empty() {
            return Err(BatchError::Empty);
        }
        if bytes.len() > Self::MAX_LEN {
            return Err(BatchError::TooLarge(bytes.len()));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BatchError {
    #[error("PTY output batches cannot be empty")]
    Empty,
    #[error("PTY output batch is {0} bytes; the limit is 65536")]
    TooLarge(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShutdownReason {
    SkeletonComplete,
    UserRequested,
    ChildExited,
    PlatformFailure,
    StartupFailure,
    EventIngressDisconnected,
    Signal(ProcessSignal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Terminate,
}

impl ProcessSignal {
    #[must_use]
    pub const fn number(self) -> i32 {
        match self {
            Self::Hangup => signal_hook::consts::signal::SIGHUP,
            Self::Interrupt => signal_hook::consts::signal::SIGINT,
            Self::Terminate => signal_hook::consts::signal::SIGTERM,
        }
    }

    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Hangup => 129,
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}

impl TryFrom<i32> for ProcessSignal {
    type Error = UnsupportedProcessSignal;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            signal_hook::consts::signal::SIGHUP => Ok(Self::Hangup),
            signal_hook::consts::signal::SIGINT => Ok(Self::Interrupt),
            signal_hook::consts::signal::SIGTERM => Ok(Self::Terminate),
            signal => Err(UnsupportedProcessSignal(signal)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unsupported process signal {0}")]
pub struct UnsupportedProcessSignal(pub i32);

#[cfg(test)]
mod tests {
    use super::ProcessSignal;

    #[test]
    fn process_signals_have_stable_numbers_and_exit_codes() {
        for (number, signal, exit_code) in [
            (1, ProcessSignal::Hangup, 129),
            (2, ProcessSignal::Interrupt, 130),
            (15, ProcessSignal::Terminate, 143),
        ] {
            assert_eq!(ProcessSignal::try_from(number), Ok(signal));
            assert_eq!(signal.number(), number);
            assert_eq!(signal.exit_code(), exit_code);
        }
        assert!(ProcessSignal::try_from(9).is_err());
    }
}
