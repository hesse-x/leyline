use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded, select_biased};

use crate::tab::SessionId;

const PLAY_CAPACITY: usize = 32;
const CONTROL_CAPACITY: usize = crate::config::MAX_TABS as usize;
const SESSION_COOLDOWN: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
enum Control {
    Forget(SessionId),
    Shutdown,
}

pub struct SoundWorker {
    play: Sender<SessionId>,
    control: Sender<Control>,
    cancelled: Arc<AtomicBool>,
}

impl SoundWorker {
    /// Starts the bounded sound worker.
    ///
    /// # Panics
    /// Panics if the operating system cannot create the worker thread.
    #[must_use]
    pub fn new() -> Self {
        let (play_tx, play_rx) = bounded(PLAY_CAPACITY);
        let (control_tx, control_rx) = bounded(CONTROL_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        std::thread::Builder::new()
            .name("leyline-sound".into())
            .spawn(move || run_worker(&play_rx, &control_rx, &worker_cancelled))
            .expect("sound worker thread creation must succeed");
        Self {
            play: play_tx,
            control: control_tx,
            cancelled,
        }
    }

    /// Queues one coalescible sound request without blocking the UI thread.
    #[must_use]
    pub fn play(&self, session_id: SessionId) -> bool {
        self.play.try_send(session_id).is_ok()
    }

    pub fn forget(&self, session_id: SessionId) {
        if self.control.try_send(Control::Forget(session_id)).is_err() {
            tracing::debug!(
                category = "bell_backend",
                backend = "sound",
                session_id = session_id.get(),
                "sound forget control coalesced"
            );
        }
    }
}

impl Default for SoundWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SoundWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.control.try_send(Control::Shutdown);
    }
}

fn run_worker(play: &Receiver<SessionId>, control: &Receiver<Control>, cancelled: &AtomicBool) {
    let mut backend: Option<leyline_sound::Canberra> = None;
    let mut unavailable = false;
    let mut last_played =
        HashMap::<SessionId, Instant>::with_capacity(crate::config::MAX_TABS as usize);
    while !cancelled.load(Ordering::Acquire) {
        select_biased! {
            recv(control) -> command => match command {
                Ok(Control::Forget(id)) => { last_played.remove(&id); }
                Ok(Control::Shutdown) | Err(_) => break,
            },
            recv(play) -> command => {
                let Ok(id) = command else { break; };
                let now = Instant::now();
                last_played.retain(|_, at| now.saturating_duration_since(*at) < SESSION_COOLDOWN);
                if unavailable || last_played.get(&id).is_some_and(|at| now.saturating_duration_since(*at) < SESSION_COOLDOWN) { continue; }
                if backend.is_none() {
                    match leyline_sound::Canberra::load() {
                        Ok(value) => backend = Some(value),
                        Err(error) => {
                            unavailable = true;
                            tracing::warn!(category = "bell_backend", backend = "sound", %error, "sound backend unavailable");
                            continue;
                        }
                    }
                }
                if let Err(error) = backend.as_mut().expect("initialized").play_terminal_bell() {
                    unavailable = true;
                    backend = None;
                    tracing::warn!(category = "bell_backend", backend = "sound", %error, "sound backend failed");
                } else {
                    last_played.insert(id, now);
                }
            }
        }
    }
}
