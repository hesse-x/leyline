use std::{
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use rustix::{
    event::{EventfdFlags, eventfd},
    io::{Errno, read, write},
};

#[derive(Debug, thiserror::Error)]
pub enum WakeError {
    #[error("cannot create eventfd: {0}")]
    Create(#[source] io::Error),
    #[error("cannot signal eventfd: {0}")]
    Signal(#[source] io::Error),
    #[error("cannot drain eventfd: {0}")]
    Drain(#[source] io::Error),
}

#[derive(Clone)]
pub struct EventWake {
    fd: std::sync::Arc<OwnedFd>,
}

impl EventWake {
    /// Creates a nonblocking, close-on-exec event counter.
    ///
    /// # Errors
    /// Returns [`WakeError`] when the host cannot create an eventfd.
    pub fn new() -> Result<Self, WakeError> {
        eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)
            .map(|fd| Self {
                fd: std::sync::Arc::new(fd),
            })
            .map_err(|error| WakeError::Create(error.into()))
    }

    /// Signals the UI poll loop.
    ///
    /// # Errors
    /// Returns [`WakeError`] for errors other than a saturated nonblocking counter.
    pub fn signal(&self) -> Result<(), WakeError> {
        match write(&*self.fd, &1_u64.to_ne_bytes()) {
            Ok(_) | Err(Errno::AGAIN) => Ok(()),
            Err(error) => Err(WakeError::Signal(error.into())),
        }
    }

    /// Drains all pending wake counts.
    ///
    /// # Errors
    /// Returns [`WakeError`] for errors other than reaching an empty counter.
    pub fn drain(&self) -> Result<(), WakeError> {
        let mut bytes = [0_u8; 8];
        loop {
            match read(&*self.fd, &mut bytes) {
                Ok(_) => {}
                Err(Errno::AGAIN) => return Ok(()),
                Err(error) => return Err(WakeError::Drain(error.into())),
            }
        }
    }

    #[must_use]
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_eventfd_coalesces_and_drains_without_blocking() {
        let wake = EventWake::new().expect("eventfd");
        wake.signal().expect("signal");
        wake.signal().expect("signal");
        wake.drain().expect("drain");
        wake.drain().expect("empty drain");
    }
}
