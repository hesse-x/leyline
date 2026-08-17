use std::{
    collections::HashSet,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{
    Receiver, RecvTimeoutError, SendTimeoutError, Sender, TrySendError, bounded,
};
use rustix::{
    event::{PollFd, PollFlags, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    time::Timespec,
};

use crate::security::MAX_CLIPBOARD_BYTES;
const TRANSFER_QUEUE_CAPACITY: usize = 8;
const TRANSFER_WORKERS: usize = 2;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(5);
const ABSOLUTE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferTarget {
    Clipboard,
    Primary,
}

#[derive(Debug)]
enum TransferJob {
    Read {
        request: u64,
        target: TransferTarget,
        fd: OwnedFd,
    },
    Write {
        target: TransferTarget,
        source: u64,
        fd: OwnedFd,
        bytes: Arc<[u8]>,
    },
}

#[derive(Debug)]
pub enum TransferResult {
    Received {
        request: u64,
        target: TransferTarget,
        result: Result<String, TransferError>,
    },
    WriteFailed {
        target: TransferTarget,
        source: u64,
        error: TransferError,
    },
}

pub struct TransferWorkers {
    jobs: Sender<TransferJob>,
    results: Receiver<TransferResult>,
    control: Arc<TransferControl>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct TransferControl {
    cancelled: Mutex<HashSet<u64>>,
    shutdown: AtomicBool,
}

impl TransferWorkers {
    /// Starts the fixed transfer worker pool.
    ///
    /// # Panics
    /// Panics if the operating system cannot create either worker thread.
    #[must_use]
    pub fn new(wake: &leyline_gfx::EventWake) -> Self {
        let (jobs, job_rx) = bounded(TRANSFER_QUEUE_CAPACITY);
        let (result_tx, results) = bounded(TRANSFER_QUEUE_CAPACITY);
        let control = Arc::new(TransferControl::default());
        let mut handles = Vec::with_capacity(TRANSFER_WORKERS);
        for index in 0..TRANSFER_WORKERS {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let wake = wake.clone();
            let control = Arc::clone(&control);
            handles.push(
                std::thread::Builder::new()
                    .name(format!("leyline-clipboard-{index}"))
                    .spawn(move || transfer_worker(&job_rx, &result_tx, &wake, &control))
                    .expect("clipboard worker thread creation must succeed"),
            );
        }
        Self {
            jobs,
            results,
            control,
            handles,
        }
    }

    /// Queues a bounded clipboard read without blocking the UI thread.
    ///
    /// # Errors
    /// Returns [`TransferQueueError`] if the fixed worker queue cannot accept the transfer.
    pub fn receive(
        &self,
        request: u64,
        target: TransferTarget,
        fd: OwnedFd,
    ) -> Result<(), TransferQueueError> {
        self.jobs
            .try_send(TransferJob::Read {
                request,
                target,
                fd,
            })
            .map_err(TransferQueueError::from)
    }

    /// Queues a bounded clipboard source write without blocking the UI thread.
    ///
    /// # Errors
    /// Returns [`TransferQueueError`] if the fixed worker queue cannot accept the transfer.
    pub fn send(
        &self,
        target: TransferTarget,
        source: u64,
        fd: OwnedFd,
        bytes: Arc<[u8]>,
    ) -> Result<(), TransferQueueError> {
        self.jobs
            .try_send(TransferJob::Write {
                target,
                source,
                fd,
                bytes,
            })
            .map_err(TransferQueueError::from)
    }

    pub fn cancel(&self, request: u64) {
        self.control
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request);
    }

    pub fn finish_request(&self, request: u64) {
        clear_cancel(&self.control, request);
    }

    pub fn drain(&self, mut consume: impl FnMut(TransferResult)) {
        while let Ok(result) = self.results.try_recv() {
            consume(result);
        }
    }

    /// Drains at most one UI-round budget and reports whether retained results remain.
    pub fn drain_round(&self, budget: usize, mut consume: impl FnMut(TransferResult)) -> bool {
        for _ in 0..budget {
            let Ok(result) = self.results.try_recv() else {
                break;
            };
            consume(result);
        }
        !self.results.is_empty()
    }
}

impl Drop for TransferWorkers {
    fn drop(&mut self) {
        self.control.shutdown.store(true, Ordering::Release);
        for handle in self.handles.drain(..) {
            if handle.join().is_err() {
                tracing::warn!("clipboard transfer worker panicked during shutdown");
            }
        }
    }
}

fn transfer_worker(
    jobs: &Receiver<TransferJob>,
    results: &Sender<TransferResult>,
    wake: &leyline_gfx::EventWake,
    control: &TransferControl,
) {
    loop {
        if control.shutdown.load(Ordering::Acquire) {
            return;
        }
        let job = match jobs.recv_timeout(POLL_INTERVAL) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let result = match job {
            TransferJob::Read {
                request,
                target,
                fd,
            } => TransferResult::Received {
                request,
                target,
                result: read_text_cancellable(fd, request, control),
            },
            TransferJob::Write {
                target,
                source,
                fd,
                bytes,
            } => match write_bytes_cancellable(fd, &bytes, control) {
                Ok(()) => continue,
                Err(error) => TransferResult::WriteFailed {
                    target,
                    source,
                    error,
                },
            },
        };
        if !send_result(results, result, control) {
            return;
        }
        let _ = wake.signal();
    }
}

fn send_result(
    results: &Sender<TransferResult>,
    mut result: TransferResult,
    control: &TransferControl,
) -> bool {
    loop {
        match results.send_timeout(result, POLL_INTERVAL) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if control.shutdown.load(Ordering::Acquire) {
                    return false;
                }
                result = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

fn read_text_cancellable(
    fd: OwnedFd,
    request: u64,
    control: &TransferControl,
) -> Result<String, TransferError> {
    let mut file = std::fs::File::from(fd);
    set_nonblocking(&file)?;
    let mut bytes = Vec::new();
    let started = Instant::now();
    let mut progressed = started;
    loop {
        check_transfer_state(control, Some(request), started, progressed)?;
        wait_fd(&file, PollFlags::IN)?;
        let mut chunk = [0; 16 * 1024];
        match file.read(&mut chunk) {
            Ok(0) => {
                clear_cancel(control, request);
                return String::from_utf8(bytes).map_err(|_| TransferError::InvalidUtf8);
            }
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    clear_cancel(control, request);
                    return Err(TransferError::TooLarge);
                }
                progressed = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                clear_cancel(control, request);
                return Err(TransferError::Io(error.to_string()));
            }
        }
    }
}

fn write_bytes_cancellable(
    fd: OwnedFd,
    bytes: &[u8],
    control: &TransferControl,
) -> Result<(), TransferError> {
    let mut file = std::fs::File::from(fd);
    set_nonblocking(&file)?;
    let started = Instant::now();
    let mut progressed = started;
    let mut offset = 0;
    while offset < bytes.len() {
        check_transfer_state(control, None, started, progressed)?;
        wait_fd(&file, PollFlags::OUT)?;
        match file.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(TransferError::Io(
                    "selection peer closed without progress".into(),
                ));
            }
            Ok(written) => {
                offset += written;
                progressed = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(TransferError::Io(error.to_string())),
        }
    }
    Ok(())
}

fn set_nonblocking(fd: &impl AsFd) -> Result<(), TransferError> {
    let flags = fcntl_getfl(fd).map_err(|error| TransferError::Io(error.to_string()))?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(|error| TransferError::Io(error.to_string()))
}

fn wait_fd(fd: &impl AsFd, interest: PollFlags) -> Result<(), TransferError> {
    let mut descriptors = [PollFd::new(fd, interest | PollFlags::ERR | PollFlags::HUP)];
    let timeout = Timespec {
        tv_sec: 0,
        tv_nsec: i64::from(POLL_INTERVAL.subsec_nanos()),
    };
    loop {
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(TransferError::Io(error.to_string())),
        }
    }
}

fn check_transfer_state(
    control: &TransferControl,
    request: Option<u64>,
    started: Instant,
    progressed: Instant,
) -> Result<(), TransferError> {
    if control.shutdown.load(Ordering::Acquire) {
        return Err(TransferError::Cancelled);
    }
    if let Some(request) = request
        && control
            .cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&request)
    {
        return Err(TransferError::Cancelled);
    }
    let now = Instant::now();
    if now.duration_since(progressed) >= NO_PROGRESS_TIMEOUT
        || now.duration_since(started) >= ABSOLUTE_TIMEOUT
    {
        if let Some(request) = request {
            clear_cancel(control, request);
        }
        return Err(TransferError::Timeout);
    }
    Ok(())
}

fn clear_cancel(control: &TransferControl, request: u64) {
    control
        .cancelled
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request);
}

#[cfg(test)]
fn read_text(fd: OwnedFd) -> Result<String, TransferError> {
    let mut file = std::fs::File::from(fd);
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_CLIPBOARD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| TransferError::Io(error.to_string()))?;
    if bytes.len() > MAX_CLIPBOARD_BYTES {
        return Err(TransferError::TooLarge);
    }
    String::from_utf8(bytes).map_err(|_| TransferError::InvalidUtf8)
}

#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum TransferError {
    #[error("clipboard transfer exceeds 8 MiB")]
    TooLarge,
    #[error("clipboard transfer is not UTF-8")]
    InvalidUtf8,
    #[error("clipboard transfer was cancelled")]
    Cancelled,
    #[error("clipboard transfer timed out")]
    Timeout,
    #[error("clipboard I/O failed: {0}")]
    Io(String),
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum TransferQueueError {
    #[error("clipboard transfer queue is full")]
    Full,
    #[error("clipboard transfer workers have stopped")]
    Closed,
}

impl From<TrySendError<TransferJob>> for TransferQueueError {
    fn from(value: TrySendError<TransferJob>) -> Self {
        match value {
            TrySendError::Full(_) => Self::Full,
            TrySendError::Disconnected(_) => Self::Closed,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteRisk {
    Multiline,
    ControlCharacters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasteConfirmationOverlay {
    pub revision: u64,
    pub source: TransferTarget,
    pub bytes: usize,
    pub lines: usize,
    pub risk: PasteRisk,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteConfirmationDecision {
    Accept,
    Reject,
    Consume,
}

#[must_use]
pub const fn confirmation_key(key: leyline_gfx::LogicalKey) -> PasteConfirmationDecision {
    match key {
        leyline_gfx::LogicalKey::Enter | leyline_gfx::LogicalKey::Character('y' | 'Y') => {
            PasteConfirmationDecision::Accept
        }
        leyline_gfx::LogicalKey::Escape | leyline_gfx::LogicalKey::Character('n' | 'N') => {
            PasteConfirmationDecision::Reject
        }
        _ => PasteConfirmationDecision::Consume,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PastePolicy {
    Allowed(String),
    NeedsConfirmation {
        text: String,
        bytes: usize,
        lines: usize,
        risk: PasteRisk,
    },
    Rejected,
}

#[must_use]
pub fn evaluate_paste(text: &str, confirm_multiline: bool) -> PastePolicy {
    if text.len() > MAX_CLIPBOARD_BYTES || text.contains('\0') {
        return PastePolicy::Rejected;
    }
    let normalized = text.replace("\r\n", "\n");
    if normalized.len() > crate::terminal::MAX_PASTE_BODY_BYTES {
        return PastePolicy::Rejected;
    }
    // A terminal selection commonly carries one line-ending delimiter. It is not
    // a multiline paste unless content remains on both sides of another newline.
    let inspected = normalized.strip_suffix('\n').unwrap_or(&normalized);
    let lines = inspected.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let controls = normalized
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
    if controls || (confirm_multiline && lines > 1) {
        let risk = if controls {
            PasteRisk::ControlCharacters
        } else {
            PasteRisk::Multiline
        };
        PastePolicy::NeedsConfirmation {
            bytes: normalized.len(),
            lines,
            risk,
            text: normalized,
        }
    } else {
        PastePolicy::Allowed(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Seek, SeekFrom},
        os::fd::OwnedFd,
        os::unix::net::UnixStream,
        time::Duration,
    };
    #[test]
    fn multiline_and_controls_do_not_bypass_confirmation() {
        assert!(matches!(
            evaluate_paste("one\ntwo", true),
            PastePolicy::NeedsConfirmation {
                risk: PasteRisk::Multiline,
                ..
            }
        ));
        assert!(matches!(
            evaluate_paste("echo\u{1b}[31m", false),
            PastePolicy::NeedsConfirmation {
                risk: PasteRisk::ControlCharacters,
                ..
            }
        ));
        assert_eq!(
            evaluate_paste("safe", true),
            PastePolicy::Allowed("safe".into())
        );
        assert_eq!(evaluate_paste("bad\0", false), PastePolicy::Rejected);
    }

    #[test]
    fn confirmation_keys_accept_reject_and_consume_everything_else() {
        assert_eq!(
            confirmation_key(leyline_gfx::LogicalKey::Enter),
            PasteConfirmationDecision::Accept
        );
        assert_eq!(
            confirmation_key(leyline_gfx::LogicalKey::Character('Y')),
            PasteConfirmationDecision::Accept
        );
        assert_eq!(
            confirmation_key(leyline_gfx::LogicalKey::Escape),
            PasteConfirmationDecision::Reject
        );
        assert_eq!(
            confirmation_key(leyline_gfx::LogicalKey::Character('n')),
            PasteConfirmationDecision::Reject
        );
        assert_eq!(
            confirmation_key(leyline_gfx::LogicalKey::Function(1)),
            PasteConfirmationDecision::Consume
        );
    }

    #[test]
    fn one_trailing_line_ending_does_not_require_confirmation() {
        assert_eq!(
            evaluate_paste("single line\n", true),
            PastePolicy::Allowed("single line\n".into())
        );
        assert!(matches!(
            evaluate_paste("one\ntwo\n", true),
            PastePolicy::NeedsConfirmation {
                lines: 2,
                risk: PasteRisk::Multiline,
                ..
            }
        ));
    }

    #[test]
    fn paste_body_limit_reserves_the_bracketed_wrapper_before_modal() {
        let at_limit = "a".repeat(crate::terminal::MAX_PASTE_BODY_BYTES);
        assert!(matches!(
            evaluate_paste(&at_limit, true),
            PastePolicy::Allowed(_)
        ));
        let over_limit = "a".repeat(crate::terminal::MAX_PASTE_BODY_BYTES + 1);
        assert_eq!(evaluate_paste(&over_limit, true), PastePolicy::Rejected);
    }

    #[test]
    fn transfer_reader_enforces_utf8_and_size() {
        let mut file = tempfile::tempfile().expect("file");
        file.write_all("terminal 中文".as_bytes()).expect("write");
        file.seek(SeekFrom::Start(0)).expect("seek");
        assert_eq!(read_text(file.into()).expect("read"), "terminal 中文");

        let mut file = tempfile::tempfile().expect("file");
        file.write_all(&[0xff]).expect("write");
        file.seek(SeekFrom::Start(0)).expect("seek");
        assert_eq!(read_text(file.into()), Err(TransferError::InvalidUtf8));
    }

    #[test]
    fn active_transfers_are_cancelled_and_release_worker_slots() {
        let wake = leyline_gfx::EventWake::new().expect("wake");
        let workers = TransferWorkers::new(&wake);
        let (reader_a, _writer_a) = UnixStream::pair().expect("pair");
        let (reader_b, _writer_b) = UnixStream::pair().expect("pair");
        workers
            .receive(1, TransferTarget::Primary, OwnedFd::from(reader_a))
            .expect("queue a");
        workers
            .receive(2, TransferTarget::Primary, OwnedFd::from(reader_b))
            .expect("queue b");
        workers.cancel(1);
        workers.cancel(2);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut cancelled = HashSet::new();
        while cancelled.len() < 2 && Instant::now() < deadline {
            workers.drain(|result| {
                if let TransferResult::Received {
                    request,
                    result: Err(TransferError::Cancelled),
                    ..
                } = result
                {
                    cancelled.insert(request);
                }
            });
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cancelled, HashSet::from([1, 2]));

        let mut file = tempfile::tempfile().expect("file");
        file.write_all(b"slot reused").expect("write");
        file.seek(SeekFrom::Start(0)).expect("seek");
        workers
            .receive(3, TransferTarget::Primary, file.into())
            .expect("reused queue");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut received = None;
        while received.is_none() && Instant::now() < deadline {
            workers.drain(|result| {
                if let TransferResult::Received {
                    request: 3, result, ..
                } = result
                {
                    received = Some(result);
                }
            });
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(received, Some(Ok("slot reused".into())));
    }

    #[test]
    fn result_drain_is_bounded_per_ui_round() {
        let (jobs, _job_rx) = bounded::<TransferJob>(1);
        let (result_tx, results) = bounded(TRANSFER_QUEUE_CAPACITY);
        let workers = TransferWorkers {
            jobs,
            results,
            control: Arc::new(TransferControl::default()),
            handles: Vec::new(),
        };
        for _ in 0..3 {
            result_tx
                .send(TransferResult::WriteFailed {
                    target: TransferTarget::Clipboard,
                    source: 1,
                    error: TransferError::Timeout,
                })
                .unwrap();
        }

        let mut drained = 0;
        assert!(workers.drain_round(2, |_| drained += 1));
        assert_eq!(drained, 2);
        assert!(!workers.drain_round(2, |_| drained += 1));
        assert_eq!(drained, 3);
    }
}
