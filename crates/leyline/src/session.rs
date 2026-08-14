use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use leyline_pty::{PtyProcess, PtySinks, SpawnSpec};

use crate::{
    app::{
        event::{BulkEvent, ByteBatch, ChildExit, ControlEvent, PtyEvent, PtyFailure},
        runtime::AppRuntime,
    },
    cli::LaunchRequest,
    config::EffectiveConfig,
    security::{AuditLogDecision, MetadataRateLimiter},
    terminal::{
        FrameSnapshot, GridSize, ParseAuditDelta, TerminalAction, TerminalCoreAdapter,
        TerminalError,
    },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPoll {
    Pending,
    Complete,
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueClass {
    Interactive,
    Bulk,
    ParserReply,
}

const fn queue_class_limit(class: QueueClass) -> usize {
    match class {
        QueueClass::Interactive => leyline_pty::MAX_OUTSTANDING_WRITE_BYTES,
        QueueClass::Bulk | QueueClass::ParserReply => {
            leyline_pty::MAX_OUTSTANDING_WRITE_BYTES - crate::security::INTERACTIVE_INPUT_RESERVE
        }
    }
}

struct InputTransaction {
    bytes: Vec<u8>,
}

const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

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
    pending_input: VecDeque<InputTransaction>,
    pending_input_bytes: usize,
    shutdown_deadline: Option<Instant>,
    security_audit: ParseAuditDelta,
    audit_log_limiter: MetadataRateLimiter,
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
        let writable = runtime.control_sink();
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
            writable: Arc::new(move || {
                // A full control queue is already wake-visible; end-of-round draining retries input.
                let _ = writable.try_send(ControlEvent::PtyWritable);
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
            pending_input: VecDeque::new(),
            pending_input_bytes: 0,
            shutdown_deadline: None,
            security_audit: ParseAuditDelta::default(),
            audit_log_limiter: MetadataRateLimiter::default(),
        })
    }

    pub fn handle_pty_event(&mut self, event: PtyEvent) -> Result<SessionAction, SessionError> {
        match event {
            PtyEvent::Output(batch) => {
                if self.read_closed {
                    return Err(SessionError::OutputAfterClose);
                }
                let delta = self.core.advance(batch.as_slice())?;
                self.security_audit.unknown_sequences = self
                    .security_audit
                    .unknown_sequences
                    .saturating_add(delta.audit.unknown_sequences);
                self.security_audit.rejected_actions = self
                    .security_audit
                    .rejected_actions
                    .saturating_add(delta.audit.rejected_actions);
                self.security_audit.truncated_sequences = self
                    .security_audit
                    .truncated_sequences
                    .saturating_add(delta.audit.truncated_sequences);
                self.security_audit.reply_bytes = self
                    .security_audit
                    .reply_bytes
                    .saturating_add(delta.audit.reply_bytes);
                if delta.audit.unknown_sequences != 0
                    || delta.audit.rejected_actions != 0
                    || delta.audit.truncated_sequences != 0
                {
                    self.log_security_audit(delta.audit);
                }
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
            PtyEvent::Writable => self.flush_input(64 * 1024)?,
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
        self.flush_input(64 * 1024)?;
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
    pub fn input_modes(&self) -> crate::terminal::TerminalModes {
        self.core.input_modes()
    }
    pub fn input_key(
        &mut self,
        key: crate::terminal::TerminalKey,
        modifiers: crate::terminal::Modifiers,
    ) -> Result<(), SessionError> {
        let bytes = crate::terminal::encode_key(key, modifiers, self.core.input_modes())
            .map_err(SessionError::Input)?;
        self.restore_viewport_after_input()?;
        self.queue_transaction(QueueClass::Interactive, bytes)
    }
    pub fn commit_text(&mut self, text: &str) -> Result<(), SessionError> {
        let bytes = crate::terminal::commit_text(text).map_err(SessionError::Input)?;
        self.restore_viewport_after_input()?;
        self.queue_transaction(QueueClass::Interactive, bytes)
    }
    pub fn paste(&mut self, text: &str) -> Result<(), SessionError> {
        let bytes =
            crate::terminal::paste_transaction(text, self.core.input_modes().bracketed_paste)
                .map_err(SessionError::Input)?;
        self.restore_viewport_after_input()?;
        self.queue_transaction(QueueClass::Bulk, bytes)
    }
    pub fn focus_changed(&mut self, focused: bool) -> Result<(), SessionError> {
        if let Some(bytes) = crate::terminal::encode_focus(focused, self.core.input_modes()) {
            self.queue_transaction(QueueClass::Interactive, bytes)?;
        }
        Ok(())
    }
    pub fn start_selection(
        &mut self,
        point: crate::terminal::SelectionPoint,
    ) -> Result<(), SessionError> {
        self.start_selection_kind(crate::terminal::SelectionKind::Simple, point)
    }
    pub fn start_selection_kind(
        &mut self,
        kind: crate::terminal::SelectionKind,
        point: crate::terminal::SelectionPoint,
    ) -> Result<(), SessionError> {
        self.core
            .start_selection(kind, point, crate::terminal::SelectionSide::Left)?;
        self.dirty = true;
        Ok(())
    }
    pub fn update_selection(
        &mut self,
        point: crate::terminal::SelectionPoint,
    ) -> Result<(), SessionError> {
        self.core
            .update_selection(point, crate::terminal::SelectionSide::Right)?;
        self.dirty = true;
        Ok(())
    }
    pub fn selection_overlay(&self, generation: u64) -> crate::frame_composer::SelectionOverlay {
        let ranges = self.core.projected_selection().map_or_else(
            || Arc::from([]),
            |range| {
                Arc::from([crate::frame_composer::CellRange {
                    start: range.start,
                    end: range.end,
                }])
            },
        );
        crate::frame_composer::SelectionOverlay {
            snapshot_generation: generation,
            revision: self.core.selection_revision(),
            ranges,
        }
    }
    pub fn selected_text(&self) -> Option<String> {
        self.core.selected_text()
    }
    pub fn hyperlink_at(
        &self,
        point: crate::terminal::SelectionPoint,
    ) -> Option<(u64, u16, Arc<str>)> {
        let snapshot = self.latest_snapshot.as_ref()?;
        let index = usize::from(point.line)
            .checked_mul(snapshot.grid.columns())?
            .checked_add(usize::from(point.column))?;
        let hyperlink = snapshot.cells.get(index)?.hyperlink?;
        let uri = Arc::clone(&snapshot.hyperlinks.get(usize::from(hyperlink))?.uri);
        Some((snapshot.generation, hyperlink, uri))
    }
    pub fn pointer_report(
        &mut self,
        button: crate::terminal::MouseButton,
        state: crate::terminal::ButtonState,
        point: crate::terminal::SelectionPoint,
        modifiers: crate::terminal::Modifiers,
    ) -> Result<bool, SessionError> {
        let bytes = crate::terminal::encode_mouse(
            button,
            state,
            point.column,
            point.line,
            modifiers,
            self.core.input_modes(),
        )
        .map_err(SessionError::Input)?;
        if let Some(bytes) = bytes {
            self.queue_transaction(QueueClass::Interactive, bytes)?;
            return Ok(true);
        }
        Ok(false)
    }
    pub fn scroll(&mut self, lines: i32) -> Result<(), SessionError> {
        self.core.scroll_display(lines)?;
        self.dirty = true;
        Ok(())
    }
    pub fn scroll_to_display_offset(&mut self, offset: usize) -> Result<(), SessionError> {
        self.core.scroll_to_display_offset(offset)?;
        self.dirty = true;
        Ok(())
    }
    pub fn alternate_scroll(&mut self, lines: i32) -> Result<bool, SessionError> {
        let Some(bytes) = crate::terminal::encode_alternate_scroll(lines, self.core.input_modes())
        else {
            return Ok(false);
        };
        self.queue_transaction(QueueClass::Interactive, bytes)?;
        Ok(true)
    }

    fn restore_viewport_after_input(&mut self) -> Result<(), SessionError> {
        self.core.scroll_to_bottom()?;
        self.dirty = true;
        Ok(())
    }
    pub const fn state(&self) -> SessionState {
        self.state
    }
    #[must_use]
    pub const fn security_audit(&self) -> ParseAuditDelta {
        self.security_audit
    }

    fn log_security_audit(&mut self, delta: ParseAuditDelta) {
        if let AuditLogDecision::Emit {
            previously_suppressed,
        } = self.audit_log_limiter.record(Instant::now())
        {
            tracing::debug!(
                category = "rejected_input",
                operation = "terminal_sequence",
                unknown = delta.unknown_sequences,
                rejected = delta.rejected_actions,
                truncated = delta.truncated_sequences,
                previously_suppressed,
                "terminal sequence audit event"
            );
        }
    }
    pub fn take_title(&mut self) -> Option<Arc<str>> {
        self.pending_title.take()
    }
    pub fn begin_shutdown(&mut self) {
        if self.shutdown_deadline.is_some() || self.state == SessionState::Closed {
            return;
        }
        self.state = SessionState::Closing;
        self.pending_input.clear();
        self.pending_input_bytes = 0;
        self.shutdown_deadline = Some(Instant::now() + SHUTDOWN_DEADLINE);
        if let Some(process) = &self.process {
            let _ = process.request_shutdown();
        }
    }

    #[must_use]
    pub const fn shutdown_deadline(&self) -> Option<Instant> {
        self.shutdown_deadline
    }

    pub fn poll_shutdown(&mut self, now: Instant) -> Result<ShutdownPoll, SessionError> {
        let Some(deadline) = self.shutdown_deadline else {
            return Ok(ShutdownPoll::Pending);
        };
        if let Some(process) = self.process.as_mut()
            && process.try_join().map_err(SessionError::Join)?
        {
            self.process.take();
            self.state = SessionState::Closed;
            self.shutdown_deadline = None;
            return Ok(ShutdownPoll::Complete);
        }
        if self.process.is_none() {
            self.state = SessionState::Closed;
            self.shutdown_deadline = None;
            return Ok(ShutdownPoll::Complete);
        }
        if now >= deadline {
            // PtyProcess::drop detaches handles whose workers own all remaining state.
            self.process.take();
            self.state = SessionState::Closed;
            self.shutdown_deadline = None;
            return Ok(ShutdownPoll::TimedOut);
        }
        Ok(ShutdownPoll::Pending)
    }

    fn flush_actions(&mut self) -> Result<(), SessionError> {
        let mut actions = Vec::new();
        self.core.drain_actions(&mut actions);
        for action in actions {
            match action {
                TerminalAction::WriteToPty(bytes) => {
                    self.queue_transaction(QueueClass::ParserReply, bytes)?;
                }
                TerminalAction::SetTitle(title) => self.pending_title = Some(title),
                TerminalAction::Bell
                | TerminalAction::ClipboardRequestRejected
                | TerminalAction::UnsupportedSequence => {}
            }
        }
        Ok(())
    }

    pub fn queue_input(&mut self, bytes: Vec<u8>) -> Result<(), SessionError> {
        self.queue_transaction(QueueClass::Interactive, bytes)
    }

    pub fn queue_transaction(
        &mut self,
        class: QueueClass,
        bytes: Vec<u8>,
    ) -> Result<(), SessionError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let next = self
            .pending_input_bytes
            .checked_add(bytes.len())
            .ok_or(SessionError::InputCapacityExceeded)?;
        let worker_bytes = self
            .process
            .as_ref()
            .map_or(0, PtyProcess::outstanding_write_bytes);
        let hard_limit = queue_class_limit(class);
        if next
            .checked_add(worker_bytes)
            .is_none_or(|total| total > hard_limit)
        {
            return Err(SessionError::InputCapacityExceeded);
        }
        self.pending_input_bytes = next;
        self.pending_input.push_back(InputTransaction { bytes });
        // Start every newly queued transaction immediately. Writable notifications only report
        // recovery from backpressure; an idle PTY has no transition that would trigger one.
        self.flush_input(64 * 1024)
    }

    pub fn flush_input(&mut self, mut budget: usize) -> Result<(), SessionError> {
        let Some(process) = &self.process else {
            return Ok(());
        };
        while budget != 0 {
            let Some(transaction) = self.pending_input.front() else {
                break;
            };
            let chunk_len = transaction.bytes.len().min(64 * 1024).min(budget);
            let chunk = transaction.bytes[..chunk_len].to_vec();
            match process.try_write(chunk).map_err(SessionError::Command)? {
                leyline_pty::WriteStatus::Accepted => {
                    let Some(transaction) = self.pending_input.front_mut() else {
                        break;
                    };
                    transaction.bytes.drain(..chunk_len);
                    self.pending_input_bytes -= chunk_len;
                    budget -= chunk_len;
                    if transaction.bytes.is_empty() {
                        self.pending_input.pop_front();
                    }
                }
                leyline_pty::WriteStatus::WouldBlock => break,
                leyline_pty::WriteStatus::Closed => return Err(SessionError::InputClosed),
            }
        }
        Ok(())
    }

    fn update_state(&mut self) -> Result<SessionAction, SessionError> {
        if self.shutdown_deadline.is_some() {
            return Ok(SessionAction::Continue);
        }
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
    fn bulk_and_parser_replies_preserve_interactive_credit() {
        assert_eq!(
            queue_class_limit(QueueClass::Interactive),
            leyline_pty::MAX_OUTSTANDING_WRITE_BYTES
        );
        assert_eq!(
            queue_class_limit(QueueClass::Bulk),
            leyline_pty::MAX_OUTSTANDING_WRITE_BYTES - crate::security::INTERACTIVE_INPUT_RESERVE
        );
        assert_eq!(
            queue_class_limit(QueueClass::ParserReply),
            queue_class_limit(QueueClass::Bulk)
        );
    }

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
    fn shutdown_completion_is_polled_without_blocking_ui() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .build()
            .unwrap();
        let config = EffectiveConfig::default();
        let launch = LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from("exec sleep 30")],
        });
        let mut session =
            TerminalSession::start(&launch, &config, GridSize::new(10, 2).unwrap(), &runtime)
                .unwrap();

        session.begin_shutdown();
        let first_poll = Instant::now();
        assert_eq!(
            session.poll_shutdown(Instant::now()).unwrap(),
            ShutdownPoll::Pending
        );
        assert!(first_poll.elapsed() < Duration::from_millis(100));

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut events = Vec::new();
            runtime.inbox().drain_round(|event| events.push(event));
            for event in events {
                if let crate::app::event::AppEvent::Pty(event) = event {
                    session.handle_pty_event(event).unwrap();
                }
            }
            if session.poll_shutdown(Instant::now()).unwrap() == ShutdownPoll::Complete {
                break;
            }
            assert!(Instant::now() < deadline, "shutdown did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(session.state(), SessionState::Closed);
    }

    #[test]
    fn user_input_is_submitted_without_waiting_for_a_writable_notification() {
        let runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .build()
            .unwrap();
        let config = EffectiveConfig::default();
        let launch = LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from("exec sleep 30")],
        });
        let mut session =
            TerminalSession::start(&launch, &config, GridSize::new(10, 2).unwrap(), &runtime)
                .unwrap();

        session.commit_text("x").unwrap();

        assert!(session.pending_input.is_empty());
        assert_eq!(session.pending_input_bytes, 0);
        session.begin_shutdown();
    }

    #[test]
    fn user_input_reaches_the_pty_without_prior_output() {
        let mut runtime = AppRuntimeBuilder::new(Arc::new(CountingWake::default()))
            .build()
            .unwrap();
        let config = EffectiveConfig::default();
        let launch = LaunchRequest::Command(crate::cli::CommandSpec {
            program: OsString::from("/bin/cat"),
            args: Vec::new(),
        });
        let mut session =
            TerminalSession::start(&launch, &config, GridSize::new(10, 2).unwrap(), &runtime)
                .unwrap();

        session.commit_text("x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut echoed = false;
        while Instant::now() < deadline && !echoed {
            let mut events = Vec::new();
            runtime.inbox().drain_round(|event| events.push(event));
            for event in events {
                if let crate::app::event::AppEvent::Pty(PtyEvent::Output(batch)) = event {
                    echoed |= batch.as_slice().contains(&b'x');
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(echoed, "queued keyboard text never reached the PTY");
        session.begin_shutdown();
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
        if self.security_audit.unknown_sequences != 0
            || self.security_audit.rejected_actions != 0
            || self.security_audit.truncated_sequences != 0
        {
            tracing::debug!(
                category = "rejected_input",
                operation = "terminal_sequence_summary",
                unknown = self.security_audit.unknown_sequences,
                rejected = self.security_audit.rejected_actions,
                truncated = self.security_audit.truncated_sequences,
                reply_bytes = self.security_audit.reply_bytes,
                "terminal security audit summary"
            );
        }
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
    #[error("PTY input capacity exceeded")]
    InputCapacityExceeded,
    #[error("PTY input endpoint is closed")]
    InputClosed,
    #[error(transparent)]
    Input(#[from] crate::terminal::InputError),
}
