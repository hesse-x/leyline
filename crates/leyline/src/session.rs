use std::sync::Arc;

use leyline_pty::{PtyProcess, PtySinks, SpawnSpec};

use crate::{
    app::{
        event::{BulkEvent, ByteBatch, ChildExit, ControlEvent, PtyEvent, PtyFailure},
        runtime::AppRuntime,
    },
    cli::LaunchRequest,
    config::EffectiveConfig,
    terminal::{FrameSnapshot, GridSize, TerminalAction, TerminalCoreAdapter, TerminalError},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Running,
    ExitObserved,
    ReadClosed,
    Completed,
    Held,
    Closing,
    Closed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionAction {
    Continue,
    Completed,
    Held,
    Failed,
}

pub struct TerminalSession {
    core: TerminalCoreAdapter,
    process: Option<PtyProcess>,
    state: SessionState,
    exited: Option<ChildExit>,
    read_closed: bool,
    hold_after_exit: bool,
    dirty: bool,
    latest_snapshot: Option<FrameSnapshot>,
    pending_title: Option<Arc<str>>,
}

#[allow(clippy::missing_errors_doc)]
impl TerminalSession {
    pub fn start(
        launch: &LaunchRequest,
        config: &EffectiveConfig,
        initial_size: GridSize,
        runtime: &AppRuntime,
    ) -> Result<Self, SessionStartError> {
        let pty_size =
            leyline_pty::PtySize::new(initial_size.columns.get(), initial_size.lines.get(), 0, 0)?;
        let spec = match launch {
            LaunchRequest::DefaultShell => SpawnSpec::default_shell(pty_size)?,
            LaunchRequest::Command(command) => {
                SpawnSpec::command(command.program.clone(), command.args.clone(), pty_size)?
            }
        };
        let bulk = runtime.bulk_sink();
        let reliable_exit = runtime.reliable_control_sink();
        let reliable_failure = runtime.reliable_control_sink();
        let bulk_close = bulk.clone();
        let sinks = PtySinks {
            output: Arc::new(move |bytes| {
                ByteBatch::new(bytes)
                    .is_ok_and(|batch| bulk.send_or_cancel(BulkEvent::PtyOutput(batch)).is_ok())
            }),
            read_closed: Arc::new(move || {
                bulk_close.send_or_cancel(BulkEvent::PtyReadClosed).is_ok()
            }),
            exited: Arc::new(move |exit| {
                let exit = match exit {
                    leyline_pty::ChildExit::Code(code) => ChildExit::Code(code),
                    leyline_pty::ChildExit::Signaled {
                        signal,
                        core_dumped,
                    } => ChildExit::Signaled {
                        signal,
                        core_dumped,
                    },
                    leyline_pty::ChildExit::Other(raw_status) => ChildExit::Other { raw_status },
                };
                let _ = reliable_exit.send_or_cancel(ControlEvent::PtyExited(exit));
            }),
            failed: Arc::new(move |message| {
                let _ = reliable_failure
                    .send_or_cancel(ControlEvent::PtyFailed(PtyFailure { message }));
            }),
        };
        let process = PtyProcess::spawn(spec, sinks)?;
        Ok(Self {
            core: TerminalCoreAdapter::new(initial_size, config.scrolling.history_lines as usize)?,
            process: Some(process),
            state: SessionState::Running,
            exited: None,
            read_closed: false,
            hold_after_exit: config.behavior.hold_after_exit,
            dirty: true,
            latest_snapshot: None,
            pending_title: None,
        })
    }

    pub fn handle_pty_event(&mut self, event: PtyEvent) -> Result<SessionAction, SessionError> {
        match event {
            PtyEvent::Output(batch) => {
                if self.read_closed {
                    return Err(SessionError::OutputAfterClose);
                }
                self.core.advance(batch.as_slice())?;
                self.dirty = true;
                self.flush_actions()?;
            }
            PtyEvent::Exited(exit) => {
                if self.exited.replace(exit).is_some() {
                    return Err(SessionError::DuplicateExit);
                }
            }
            PtyEvent::ReadClosed => {
                if self.read_closed {
                    return Err(SessionError::DuplicateReadClose);
                }
                self.read_closed = true;
            }
            PtyEvent::Failed(failure) => {
                self.state = SessionState::Failed;
                return Err(SessionError::Pty(failure.message));
            }
        }
        self.update_state()
    }

    pub fn resize(&mut self, size: GridSize) -> Result<(), SessionError> {
        self.core.resize(size)?;
        self.dirty = true;
        if matches!(
            self.state,
            SessionState::Running | SessionState::ExitObserved | SessionState::ReadClosed
        ) && let Some(process) = &self.process
        {
            process
                .resize(leyline_pty::PtySize::new(
                    size.columns.get(),
                    size.lines.get(),
                    0,
                    0,
                )?)
                .map_err(SessionError::Command)?;
        }
        Ok(())
    }

    pub fn end_drain_round(&mut self) -> Result<Option<FrameSnapshot>, SessionError> {
        if !self.dirty {
            return Ok(None);
        }
        let snapshot = self.core.snapshot()?;
        self.latest_snapshot = Some(snapshot.clone());
        self.dirty = false;
        Ok(Some(snapshot))
    }

    pub fn latest_snapshot(&self) -> Option<&FrameSnapshot> {
        self.latest_snapshot.as_ref()
    }
    pub const fn state(&self) -> SessionState {
        self.state
    }
    pub fn take_title(&mut self) -> Option<Arc<str>> {
        self.pending_title.take()
    }
    pub fn begin_shutdown(&mut self) {
        self.state = SessionState::Closing;
        if let Some(process) = &self.process {
            let _ = process.request_shutdown();
        }
    }

    fn flush_actions(&mut self) -> Result<(), SessionError> {
        let mut actions = Vec::new();
        self.core.drain_actions(&mut actions);
        for action in actions {
            match action {
                TerminalAction::WriteToPty(bytes) => {
                    if let Some(process) = &self.process {
                        process.write(bytes).map_err(SessionError::Command)?;
                    }
                }
                TerminalAction::SetTitle(title) => self.pending_title = Some(title),
                TerminalAction::Bell
                | TerminalAction::ClipboardRequestRejected
                | TerminalAction::UnsupportedSequence => {}
            }
        }
        Ok(())
    }

    fn update_state(&mut self) -> Result<SessionAction, SessionError> {
        self.state = match (self.exited.is_some(), self.read_closed) {
            (false, false) => SessionState::Running,
            (true, false) => SessionState::ExitObserved,
            (false, true) => SessionState::ReadClosed,
            (true, true) => SessionState::Completed,
        };
        if self.state != SessionState::Completed {
            return Ok(SessionAction::Continue);
        }
        if let Some(process) = self.process.take() {
            process.join().map_err(SessionError::Join)?;
        }
        if self.hold_after_exit {
            self.state = SessionState::Held;
            Ok(SessionAction::Held)
        } else {
            Ok(SessionAction::Completed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::runtime::{AppRuntimeBuilder, CountingWake, InboxPolicy};
    use std::{
        ffi::OsString,
        time::{Duration, Instant},
    };

    #[test]
    fn full_bulk_queue_shutdown_cancels_producer_and_joins() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .policy(InboxPolicy {
                bulk_capacity: 1,
                ..InboxPolicy::default()
            })
            .build()
            .unwrap();
        let config = EffectiveConfig::default();
        let launch = LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/sh"),
            args: vec![
                OsString::from("-c"),
                OsString::from("while :; do printf 0123456789; done"),
            ],
        });
        let mut session =
            TerminalSession::start(&launch, &config, GridSize::new(10, 2).unwrap(), &runtime)
                .unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        runtime.fast_cancel();
        session.begin_shutdown();
        drop(session);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn hold_waits_for_exit_and_eof_then_retains_final_snapshot() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .build()
            .unwrap();
        let mut config = EffectiveConfig::default();
        config.behavior.hold_after_exit = true;
        let launch = LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/printf"),
            args: vec![OsString::from("final-output")],
        });
        let mut session =
            TerminalSession::start(&launch, &config, GridSize::new(20, 2).unwrap(), &runtime)
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while session.state() != SessionState::Held {
            let mut events = Vec::new();
            runtime.inbox().drain_round(|event| events.push(event));
            for event in events {
                if let crate::app::event::AppEvent::Pty(event) = event {
                    session.handle_pty_event(event).unwrap();
                }
            }
            assert!(Instant::now() < deadline, "session did not reach Held");
            std::thread::yield_now();
        }
        let snapshot = session.end_drain_round().unwrap().unwrap();
        let visible: String = snapshot.cells.iter().map(|cell| cell.ch).collect();
        assert!(visible.contains("final-output"), "{visible:?}");
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionStartError {
    #[error(transparent)]
    Spawn(#[from] leyline_pty::SpawnError),
    #[error(transparent)]
    Terminal(#[from] TerminalError),
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Terminal(#[from] TerminalError),
    #[error(transparent)]
    Size(#[from] leyline_pty::SpawnError),
    #[error("PTY command failed: {0}")]
    Command(leyline_pty::PtyCommandError),
    #[error("PTY join failed: {0}")]
    Join(leyline_pty::JoinError),
    #[error("PTY failed: {0}")]
    Pty(String),
    #[error("received PTY output after read closure")]
    OutputAfterClose,
    #[error("received duplicate child exit event")]
    DuplicateExit,
    #[error("received duplicate PTY read closure")]
    DuplicateReadClose,
}
