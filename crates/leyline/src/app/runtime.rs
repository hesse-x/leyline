use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded, select};

use super::event::{AppEvent, BulkEvent, ControlEvent, PtyEvent};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PtyCompletion {
    exited: bool,
    read_closed: bool,
}

impl PtyCompletion {
    pub fn observe_control(&mut self, event: &ControlEvent) {
        self.exited |= matches!(event, ControlEvent::PtyExited(_));
    }

    pub fn observe_bulk(&mut self, event: &BulkEvent) {
        self.read_closed |= matches!(event, BulkEvent::PtyReadClosed);
    }

    #[must_use]
    pub const fn final_output_complete(self) -> bool {
        self.exited && self.read_closed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InboxPolicy {
    pub control_capacity: usize,
    pub bulk_capacity: usize,
    pub control_budget: usize,
    pub bulk_event_budget: usize,
    pub bulk_byte_budget: usize,
}

impl Default for InboxPolicy {
    fn default() -> Self {
        Self {
            control_capacity: 64,
            bulk_capacity: 32,
            control_budget: 8,
            bulk_event_budget: 4,
            bulk_byte_budget: 256 * 1024,
        }
    }
}

impl InboxPolicy {
    const MAX_CAPACITY: usize = 65_536;
    fn validate(self) -> Result<Self, RuntimeBuildError> {
        if self.control_capacity == 0
            || self.bulk_capacity == 0
            || self.control_capacity > Self::MAX_CAPACITY
            || self.bulk_capacity > Self::MAX_CAPACITY
        {
            return Err(RuntimeBuildError::InvalidCapacity);
        }
        if self.control_budget == 0 || self.bulk_event_budget == 0 || self.bulk_byte_budget == 0 {
            return Err(RuntimeBuildError::InvalidBudget);
        }
        Ok(self)
    }
}

pub trait WakeBackend: Send + Sync + 'static {
    fn wake(&self);
}

#[derive(Default)]
pub struct CountingWake {
    count: AtomicUsize,
}
impl CountingWake {
    #[must_use]
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}
impl WakeBackend for CountingWake {
    fn wake(&self) {
        self.count.fetch_add(1, Ordering::Release);
    }
}

#[derive(Clone)]
struct WakeHandle {
    pending: Arc<AtomicBool>,
    backend: Arc<dyn WakeBackend>,
}
impl WakeHandle {
    fn notify(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.backend.wake();
        }
    }
}

pub struct AppRuntimeBuilder {
    policy: InboxPolicy,
    wake_backend: Arc<dyn WakeBackend>,
}
impl AppRuntimeBuilder {
    #[must_use]
    pub fn new(wake_backend: Arc<dyn WakeBackend>) -> Self {
        Self {
            policy: InboxPolicy::default(),
            wake_backend,
        }
    }
    #[must_use]
    pub const fn policy(mut self, policy: InboxPolicy) -> Self {
        self.policy = policy;
        self
    }
    /// Builds bounded control and bulk ingress channels.
    ///
    /// # Errors
    /// Returns [`RuntimeBuildError`] for zero, excessive, or invalid policy values.
    pub fn build(self) -> Result<AppRuntime, RuntimeBuildError> {
        let policy = self.policy.validate()?;
        let (control_tx, control_rx) = bounded(policy.control_capacity);
        let (bulk_tx, bulk_rx) = bounded(policy.bulk_capacity);
        let (cancel_tx, cancel_rx) = bounded(0);
        let wake = WakeHandle {
            pending: Arc::new(AtomicBool::new(false)),
            backend: self.wake_backend,
        };
        Ok(AppRuntime {
            inbox: AppInbox {
                control_rx,
                bulk_rx,
                policy,
                wake: wake.clone(),
            },
            control: ControlSink {
                sender: control_tx.clone(),
                wake: wake.clone(),
            },
            reliable_control: ReliableControlSink {
                sender: control_tx,
                cancel: cancel_rx.clone(),
                wake: wake.clone(),
            },
            bulk: BulkSink {
                sender: bulk_tx,
                cancel: cancel_rx,
                wake,
            },
            cancel_tx: Some(cancel_tx),
        })
    }
}

pub struct AppRuntime {
    inbox: AppInbox,
    control: ControlSink,
    reliable_control: ReliableControlSink,
    bulk: BulkSink,
    cancel_tx: Option<Sender<()>>,
}
impl AppRuntime {
    #[must_use]
    pub fn control_sink(&self) -> ControlSink {
        self.control.clone()
    }
    #[must_use]
    pub fn reliable_control_sink(&self) -> ReliableControlSink {
        self.reliable_control.clone()
    }
    #[must_use]
    pub fn bulk_sink(&self) -> BulkSink {
        self.bulk.clone()
    }
    pub fn inbox(&mut self) -> &mut AppInbox {
        &mut self.inbox
    }
    pub fn fast_cancel(&mut self) {
        self.cancel_tx.take();
    }
}
impl Drop for AppRuntime {
    fn drop(&mut self) {
        self.cancel_tx.take();
    }
}

#[derive(Clone)]
pub struct ControlSink {
    sender: Sender<ControlEvent>,
    wake: WakeHandle,
}
impl ControlSink {
    /// Attempts a non-blocking control send.
    ///
    /// # Errors
    /// Returns the original event when the bounded inbox is full or disconnected.
    pub fn try_send(&self, event: ControlEvent) -> Result<(), TrySendError<ControlEvent>> {
        self.sender.try_send(event).inspect(|()| self.wake.notify())
    }
}

#[derive(Clone)]
pub struct ReliableControlSink {
    sender: Sender<ControlEvent>,
    cancel: Receiver<()>,
    wake: WakeHandle,
}
impl ReliableControlSink {
    /// Waits until a control event is sent or runtime cancellation wins.
    ///
    /// # Errors
    /// Returns the original event on cancellation or disconnection.
    pub fn send_or_cancel(&self, event: ControlEvent) -> Result<(), ControlSendError> {
        select! {
            send(self.sender, event) -> result => result.map(|()| self.wake.notify()).map_err(|error| ControlSendError::Disconnected(error.0)),
            recv(self.cancel) -> _ => Err(ControlSendError::Cancelled(event)),
        }
    }
}

#[derive(Clone)]
pub struct BulkSink {
    sender: Sender<BulkEvent>,
    cancel: Receiver<()>,
    wake: WakeHandle,
}
impl BulkSink {
    /// Applies cancellable backpressure while sending a bulk event.
    ///
    /// # Errors
    /// Returns the original event on cancellation or disconnection.
    pub fn send_or_cancel(&self, event: BulkEvent) -> Result<(), BulkSendError> {
        select! {
            send(self.sender, event) -> result => result.map(|()| self.wake.notify()).map_err(|error| BulkSendError::Disconnected(error.0)),
            recv(self.cancel) -> _ => Err(BulkSendError::Cancelled(event)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ControlSendError {
    #[error("control send was cancelled")]
    Cancelled(ControlEvent),
    #[error("control inbox is disconnected")]
    Disconnected(ControlEvent),
}
#[derive(Debug, thiserror::Error)]
pub enum BulkSendError {
    #[error("bulk send was cancelled")]
    Cancelled(BulkEvent),
    #[error("bulk inbox is disconnected")]
    Disconnected(BulkEvent),
}

pub struct AppInbox {
    control_rx: Receiver<ControlEvent>,
    bulk_rx: Receiver<BulkEvent>,
    policy: InboxPolicy,
    wake: WakeHandle,
}
impl AppInbox {
    pub fn drain_round(&mut self, mut consume: impl FnMut(AppEvent)) -> DrainResult {
        let mut result = DrainResult::default();
        for _ in 0..self.policy.control_budget {
            match self.control_rx.try_recv() {
                Ok(event) => {
                    result.control += 1;
                    consume(control_to_app(event));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    result.control_disconnected = true;
                    break;
                }
            }
        }
        for _ in 0..self.policy.bulk_event_budget {
            match self.bulk_rx.try_recv() {
                Ok(event) => {
                    let bytes = match &event {
                        BulkEvent::PtyOutput(batch) => batch.len(),
                        BulkEvent::PtyReadClosed => 0,
                    };
                    result.bulk += 1;
                    result.bulk_bytes += bytes;
                    consume(bulk_to_app(event));
                    if result.bulk_bytes >= self.policy.bulk_byte_budget {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    result.bulk_disconnected = true;
                    break;
                }
            }
        }
        result
    }

    #[must_use]
    pub fn prepare_to_wait(&self) -> bool {
        self.wake.pending.store(false, Ordering::Release);
        if self.control_rx.is_empty() && self.bulk_rx.is_empty() {
            true
        } else {
            self.wake.pending.store(true, Ordering::Release);
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DrainResult {
    pub control: usize,
    pub bulk: usize,
    pub bulk_bytes: usize,
    pub control_disconnected: bool,
    pub bulk_disconnected: bool,
}

fn control_to_app(event: ControlEvent) -> AppEvent {
    match event {
        ControlEvent::Shutdown(reason) => AppEvent::ShutdownRequested(reason),
        ControlEvent::PtyExited(exit) => AppEvent::Pty(PtyEvent::Exited(exit)),
        ControlEvent::PtyFailed(failure) => AppEvent::Pty(PtyEvent::Failed(failure)),
        ControlEvent::PtyWritable => AppEvent::Pty(PtyEvent::Writable),
    }
}
fn bulk_to_app(event: BulkEvent) -> AppEvent {
    match event {
        BulkEvent::PtyOutput(batch) => AppEvent::Pty(PtyEvent::Output(batch)),
        BulkEvent::PtyReadClosed => AppEvent::Pty(PtyEvent::ReadClosed),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeBuildError {
    #[error("inbox capacity must be in 1..=65536")]
    InvalidCapacity,
    #[error("inbox processing budgets must be nonzero")]
    InvalidBudget,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::event::{ByteBatch, ShutdownReason};
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn enforces_capacity_and_control_precedes_bulk_with_budgets() {
        let wake = Arc::new(CountingWake::default());
        let mut runtime = AppRuntimeBuilder::new(wake.clone())
            .policy(InboxPolicy {
                control_capacity: 1,
                bulk_capacity: 2,
                control_budget: 1,
                bulk_event_budget: 1,
                bulk_byte_budget: 10,
            })
            .build()
            .expect("runtime");
        let control = runtime.control_sink();
        control
            .try_send(ControlEvent::Shutdown(ShutdownReason::UserRequested))
            .expect("send");
        assert!(matches!(
            control.try_send(ControlEvent::Shutdown(ShutdownReason::UserRequested)),
            Err(TrySendError::Full(_))
        ));
        runtime
            .bulk_sink()
            .send_or_cancel(BulkEvent::PtyOutput(
                ByteBatch::new(vec![1]).expect("batch"),
            ))
            .expect("send");
        let mut events = Vec::new();
        assert_eq!(
            runtime.inbox().drain_round(|event| events.push(event)),
            DrainResult {
                control: 1,
                bulk: 1,
                bulk_bytes: 1,
                control_disconnected: false,
                bulk_disconnected: false
            }
        );
        assert!(matches!(events[0], AppEvent::ShutdownRequested(_)));
        assert_eq!(wake.count(), 1);
    }

    #[test]
    fn full_bulk_send_is_cancelled_without_deadlock() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .policy(InboxPolicy {
                bulk_capacity: 1,
                ..InboxPolicy::default()
            })
            .build()
            .expect("runtime");
        let bulk = runtime.bulk_sink();
        bulk.send_or_cancel(BulkEvent::PtyReadClosed).expect("fill");
        let (done_tx, done_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            done_tx
                .send(bulk.send_or_cancel(BulkEvent::PtyReadClosed))
                .expect("result");
        });
        runtime.fast_cancel();
        assert!(matches!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("completion"),
            Err(BulkSendError::Cancelled(_))
        ));
        thread.join().expect("join");
    }

    #[test]
    fn clearing_wake_rechecks_queues() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .build()
            .expect("runtime");
        runtime
            .control_sink()
            .try_send(ControlEvent::Shutdown(ShutdownReason::UserRequested))
            .expect("send");
        assert!(!runtime.inbox().prepare_to_wait());
        runtime.inbox().drain_round(drop);
        assert!(runtime.inbox().prepare_to_wait());
    }

    #[test]
    fn pty_completion_requires_exit_and_read_close_in_any_order() {
        use crate::app::event::ChildExit;
        let mut state = PtyCompletion::default();
        state.observe_control(&ControlEvent::PtyExited(ChildExit::Code(0)));
        assert!(!state.final_output_complete());
        state.observe_bulk(&BulkEvent::PtyReadClosed);
        assert!(state.final_output_complete());

        let mut reverse = PtyCompletion::default();
        reverse.observe_bulk(&BulkEvent::PtyReadClosed);
        assert!(!reverse.final_output_complete());
        reverse.observe_control(&ControlEvent::PtyExited(ChildExit::Code(0)));
        assert!(reverse.final_output_complete());
    }
}
