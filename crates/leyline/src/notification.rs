use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select_biased};

use crate::{
    config::BellConfig,
    tab::{BellGeneration, SessionId},
};

const DATA_CAPACITY: usize = 32;
const CONTROL_CAPACITY: usize = crate::config::MAX_TABS as usize + DATA_CAPACITY;
const BACKOFF: Duration = Duration::from_mins(1);

#[derive(Clone, Copy, Debug)]
enum DataCommand {
    Show {
        session_id: SessionId,
        generation: BellGeneration,
        sequence: u64,
        ordinal: u8,
    },
}

#[derive(Clone, Copy, Debug)]
enum ControlCommand {
    Acknowledge {
        session_id: SessionId,
        generation: BellGeneration,
        barrier: u64,
    },
    Forget {
        session_id: SessionId,
        barrier: u64,
    },
    Shutdown,
}

#[derive(Clone, Copy)]
struct NotificationEntry {
    generation: BellGeneration,
    notification_id: u32,
}

pub struct NotificationWorker {
    data: Sender<DataCommand>,
    control: Sender<ControlCommand>,
    cancelled: Arc<AtomicBool>,
    sequence: u64,
    limiter: NotificationRateLimiter,
    pending_controls: VecDeque<ControlCommand>,
}

struct NotificationRateLimiter {
    last_by_session: HashMap<SessionId, Instant>,
    global_window: VecDeque<Instant>,
}

impl NotificationRateLimiter {
    fn new() -> Self {
        Self {
            last_by_session: HashMap::with_capacity(crate::config::MAX_TABS as usize),
            global_window: VecDeque::with_capacity(30),
        }
    }

    fn allows(&mut self, session_id: SessionId, now: Instant, config: &BellConfig) -> bool {
        if self
            .last_by_session
            .get(&session_id)
            .is_some_and(|last| now.saturating_duration_since(*last) < config.notification_cooldown)
        {
            return false;
        }
        while self
            .global_window
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= Duration::from_mins(1))
        {
            self.global_window.pop_front();
        }
        self.global_window.len() < usize::from(config.notification_burst_per_minute.get())
    }

    fn record(&mut self, session_id: SessionId, now: Instant) {
        self.last_by_session.insert(session_id, now);
        self.global_window.push_back(now);
    }

    fn forget(&mut self, session_id: SessionId) {
        self.last_by_session.remove(&session_id);
    }
}

impl NotificationWorker {
    /// Starts the bounded notification worker.
    ///
    /// # Panics
    /// Panics if the operating system cannot create the worker thread.
    #[must_use]
    pub fn new() -> Self {
        let (data_tx, data_rx) = bounded(DATA_CAPACITY);
        let (control_tx, control_rx) = bounded(CONTROL_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("leyline-notification".into())
            .spawn(move || run_worker(&data_rx, &control_rx, &worker_cancelled))
            .expect("notification worker thread creation must succeed");
        Self {
            data: data_tx,
            control: control_tx,
            cancelled,
            sequence: 0,
            limiter: NotificationRateLimiter::new(),
            pending_controls: VecDeque::with_capacity(crate::config::MAX_TABS as usize),
        }
    }

    pub fn show(
        &mut self,
        session_id: SessionId,
        generation: BellGeneration,
        ordinal: u8,
        now: Instant,
        config: &BellConfig,
    ) -> bool {
        self.retry_controls();
        if self
            .pending_controls
            .iter()
            .any(|command| command.session_id() == session_id)
        {
            return false;
        }
        if !self.limiter.allows(session_id, now, config) {
            return false;
        }
        let Some(sequence) = self.next_sequence() else {
            return false;
        };
        let command = DataCommand::Show {
            session_id,
            generation,
            sequence,
            ordinal,
        };
        if self.data.try_send(command).is_err() {
            return false;
        }
        self.limiter.record(session_id, now);
        true
    }

    pub fn acknowledge(&mut self, session_id: SessionId, generation: BellGeneration) {
        let Some(barrier) = self.next_sequence() else {
            return;
        };
        self.send_control(ControlCommand::Acknowledge {
            session_id,
            generation,
            barrier,
        });
    }

    pub fn forget(&mut self, session_id: SessionId) {
        self.limiter.forget(session_id);
        let Some(barrier) = self.next_sequence() else {
            return;
        };
        self.send_control(ControlCommand::Forget {
            session_id,
            barrier,
        });
    }

    fn next_sequence(&mut self) -> Option<u64> {
        self.sequence = self.sequence.checked_add(1)?;
        Some(self.sequence)
    }

    fn send_control(&mut self, command: ControlCommand) {
        self.retry_controls();
        match self.control.try_send(command) {
            Ok(()) => {}
            Err(TrySendError::Full(command)) => {
                let session_id = command.session_id();
                self.pending_controls
                    .retain(|pending| pending.session_id() != session_id);
                self.pending_controls.push_back(command);
            }
            Err(TrySendError::Disconnected(_)) => tracing::warn!(
                category = "bell_backend",
                backend = "notification",
                reason = "worker_stopped",
                "notification lifecycle control could not be queued"
            ),
        }
    }

    pub fn retry_controls(&mut self) {
        while let Some(command) = self.pending_controls.pop_front() {
            match self.control.try_send(command) {
                Ok(()) => {}
                Err(TrySendError::Full(command)) => {
                    self.pending_controls.push_front(command);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.pending_controls.clear();
                    break;
                }
            }
        }
    }
}

impl ControlCommand {
    fn session_id(self) -> SessionId {
        match self {
            Self::Acknowledge { session_id, .. } | Self::Forget { session_id, .. } => session_id,
            Self::Shutdown => unreachable!("shutdown is never retained"),
        }
    }
}

impl Default for NotificationWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for NotificationWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.control.try_send(ControlCommand::Shutdown);
    }
}

fn run_worker(
    data: &Receiver<DataCommand>,
    control: &Receiver<ControlCommand>,
    cancelled: &AtomicBool,
) {
    let mut connection: Option<zbus::blocking::Connection> = None;
    let mut retry_at: Option<Instant> = None;
    let mut entries =
        HashMap::<SessionId, NotificationEntry>::with_capacity(crate::config::MAX_TABS as usize);
    let mut barriers = HashMap::<SessionId, u64>::with_capacity(CONTROL_CAPACITY);
    while !cancelled.load(Ordering::Acquire) {
        select_biased! {
            recv(control) -> command => match command {
                Ok(ControlCommand::Acknowledge { session_id, generation, barrier }) => {
                    if !data.is_empty() { barriers.insert(session_id, barrier); }
                    if entries.get(&session_id).is_some_and(|entry| entry.generation == generation)
                        && let Some(entry) = entries.remove(&session_id) {
                        close(&mut connection, entry.notification_id);
                    }
                }
                Ok(ControlCommand::Forget { session_id, barrier }) => {
                    if !data.is_empty() { barriers.insert(session_id, barrier); }
                    if let Some(entry) = entries.remove(&session_id) { close(&mut connection, entry.notification_id); }
                }
                Ok(ControlCommand::Shutdown) | Err(_) => break,
            },
            recv(data) -> command => {
                let Ok(DataCommand::Show { session_id, generation, sequence, ordinal }) = command else { break; };
                let stale = barriers.get(&session_id).is_some_and(|barrier| sequence < *barrier);
                if data.is_empty() { barriers.clear(); } else { barriers.retain(|_, barrier| *barrier >= sequence); }
                if stale { continue; }
                if retry_at.is_some_and(|deadline| deadline > Instant::now()) { continue; }
                if connection.is_none() {
                    match zbus::blocking::Connection::session() {
                        Ok(value) => connection = Some(value),
                        Err(error) => {
                            retry_at = Instant::now().checked_add(BACKOFF);
                            tracing::warn!(category = "bell_backend", backend = "notification", error = %error, "notification service unavailable");
                            continue;
                        }
                    }
                }
                let replaces = entries.get(&session_id).map_or(0, |entry| entry.notification_id);
                match notify(connection.as_ref().expect("initialized"), replaces, ordinal) {
                    Ok(id) if id != 0 => { entries.insert(session_id, NotificationEntry { generation, notification_id: id }); }
                    Ok(_) => {}
                    Err(error) => {
                        connection = None;
                        retry_at = Instant::now().checked_add(BACKOFF);
                        tracing::warn!(category = "bell_backend", backend = "notification", error = %error, "notification call failed");
                    }
                }
            }
        }
    }
    if let Some(ref connection) = connection {
        for entry in entries.values() {
            let _ = close_call(connection, entry.notification_id);
        }
    }
}

fn notify(
    connection: &zbus::blocking::Connection,
    replaces_id: u32,
    ordinal: u8,
) -> zbus::Result<u32> {
    use zbus::zvariant::Value;
    let proxy = zbus::blocking::Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    let actions: Vec<&str> = Vec::new();
    let mut hints = HashMap::<&str, Value<'_>>::new();
    hints.insert("transient", Value::from(true));
    let body = format!("Terminal bell (tab {ordinal})");
    proxy.call(
        "Notify",
        &(
            "Leyline",
            replaces_id,
            "",
            "Leyline",
            body.as_str(),
            actions,
            hints,
            5_000_i32,
        ),
    )
}

fn close(connection: &mut Option<zbus::blocking::Connection>, id: u32) {
    if let Some(value) = connection.as_ref()
        && close_call(value, id).is_err()
    {
        *connection = None;
    }
}

fn close_call(connection: &zbus::blocking::Connection, id: u32) -> zbus::Result<()> {
    let proxy = zbus::blocking::Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )?;
    proxy.call("CloseNotification", &(id,))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_and_global_window_are_consumed_only_by_admitted_shows() {
        let mut limiter = NotificationRateLimiter::new();
        let config = BellConfig::default();
        let now = Instant::now();
        assert!(limiter.allows(SessionId::from_raw(1), now, &config));
        limiter.record(SessionId::from_raw(1), now);
        assert!(!limiter.allows(SessionId::from_raw(1), now, &config));
        for id in 2_u8..=6 {
            let session = SessionId::from_raw(u64::from(id));
            assert!(limiter.allows(session, now, &config));
            limiter.record(session, now);
        }
        assert!(!limiter.allows(SessionId::from_raw(7), now, &config));
    }
}
