use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
};

use leyline_gfx::EventWake;
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    iterator::{Handle, Signals},
};

use crate::app::event::ProcessSignal;

#[derive(Default)]
pub struct ProcessSignalState {
    first: AtomicI32,
    count: AtomicU32,
    wake_failed: AtomicBool,
}

impl ProcessSignalState {
    #[must_use]
    pub fn first_signal(&self) -> Option<ProcessSignal> {
        ProcessSignal::try_from(self.first.load(Ordering::Acquire)).ok()
    }

    #[must_use]
    pub fn received_count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn wake_failed(&self) -> bool {
        self.wake_failed.load(Ordering::Relaxed)
    }

    fn publish(&self, signal: ProcessSignal, wake: &EventWake) {
        let _ =
            self.first
                .compare_exchange(0, signal.number(), Ordering::AcqRel, Ordering::Acquire);
        let _ = self
            .count
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
        if wake.signal().is_err() {
            self.wake_failed.store(true, Ordering::Relaxed);
        }
    }
}

pub struct SignalRelay {
    state: Arc<ProcessSignalState>,
    close: Handle,
    thread: Option<JoinHandle<()>>,
}

impl SignalRelay {
    /// Installs the process-wide relay for signals that support graceful shutdown.
    ///
    /// # Errors
    /// Returns an error when handlers or the relay thread cannot be created.
    pub fn install(wake: EventWake) -> Result<Self, SignalRelayError> {
        let mut signals =
            Signals::new([SIGHUP, SIGINT, SIGTERM]).map_err(SignalRelayError::Install)?;
        let close = signals.handle();
        let state = Arc::new(ProcessSignalState::default());
        let thread_state = Arc::clone(&state);
        let thread = match thread::Builder::new()
            .name("leyline-signal-relay".into())
            .spawn(move || {
                for raw in signals.forever() {
                    if let Ok(signal) = ProcessSignal::try_from(raw) {
                        thread_state.publish(signal, &wake);
                    }
                }
            }) {
            Ok(thread) => thread,
            Err(error) => {
                close.close();
                return Err(SignalRelayError::Spawn(error));
            }
        };
        Ok(Self {
            state,
            close,
            thread: Some(thread),
        })
    }

    #[must_use]
    pub fn state(&self) -> Arc<ProcessSignalState> {
        Arc::clone(&self.state)
    }

    /// Stops and joins the relay thread.
    ///
    /// # Errors
    /// Returns an error if the relay thread panicked.
    pub fn close_and_join(&mut self) -> Result<(), SignalRelayError> {
        self.close.close();
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| SignalRelayError::Panicked)?;
        }
        Ok(())
    }
}

impl Drop for SignalRelay {
    fn drop(&mut self) {
        self.close.close();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SignalRelayError {
    #[error("cannot install process signal relay: {0}")]
    Install(#[source] io::Error),
    #[error("cannot start process signal relay: {0}")]
    Spawn(#[source] io::Error),
    #[error("process signal relay thread panicked")]
    Panicked,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_preserves_first_signal_and_counts_publications() {
        let state = ProcessSignalState::default();
        let wake = EventWake::new().expect("wake");
        state.publish(ProcessSignal::Interrupt, &wake);
        state.publish(ProcessSignal::Terminate, &wake);
        assert_eq!(state.first_signal(), Some(ProcessSignal::Interrupt));
        assert_eq!(state.received_count(), 2);
        assert!(!state.wake_failed());
    }

    #[test]
    fn relay_closes_without_receiving_a_signal() {
        let wake = EventWake::new().expect("wake");
        let mut relay = SignalRelay::install(wake).expect("relay");
        relay.close_and_join().expect("join");
        relay.close_and_join().expect("idempotent join");
    }
}
