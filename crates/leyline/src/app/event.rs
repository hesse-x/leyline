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
}
