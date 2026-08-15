use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    os::{
        fd::{FromRawFd, RawFd},
        unix::ffi::OsStrExt,
        unix::process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, bounded};

// Raw Unix boundary functions intentionally expose small safe wrappers; their error contracts are
// represented by the typed errors below.
#[allow(clippy::missing_errors_doc)]
const _: () = ();

const READ_CHUNK: usize = 16 * 1024;
const MAX_BATCH: usize = 64 * 1024;
pub const MAX_OUTSTANDING_WRITE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtySize {
    columns: u16,
    rows: u16,
    pixel_width: u16,
    pixel_height: u16,
}

impl PtySize {
    /// Creates a validated kernel window size.
    ///
    /// # Errors
    /// Returns [`SpawnError::InvalidSize`] when rows or columns are zero.
    pub fn new(
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, SpawnError> {
        if columns == 0 || rows == 0 {
            return Err(SpawnError::InvalidSize);
        }
        Ok(Self {
            columns,
            rows,
            pixel_width,
            pixel_height,
        })
    }

    fn winsize(self) -> libc::winsize {
        libc::winsize {
            ws_row: self.rows,
            ws_col: self.columns,
            ws_xpixel: self.pixel_width,
            ws_ypixel: self.pixel_height,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub initial_size: PtySize,
}

impl SpawnSpec {
    /// Freezes a direct command, environment, and working directory before spawning.
    ///
    /// # Errors
    /// Returns a typed validation or environment error when the specification cannot be frozen.
    pub fn command(
        program: OsString,
        args: Vec<OsString>,
        initial_size: PtySize,
    ) -> Result<Self, SpawnError> {
        validate_word("program", &program)?;
        for arg in &args {
            validate_word("argument", arg)?;
        }
        let cwd = std::env::current_dir().map_err(SpawnError::CurrentDirectory)?;
        let mut environment: Vec<_> = std::env::vars_os().collect();
        environment.retain(|(key, _)| key != "TERM" && key != "COLORTERM");
        environment.push(("TERM".into(), "xterm-256color".into()));
        environment.push(("COLORTERM".into(), "truecolor".into()));
        Ok(Self {
            program,
            args,
            cwd,
            environment,
            initial_size,
        })
    }

    /// Resolves the effective user's account shell as an interactive non-login command.
    ///
    /// # Errors
    /// Returns a typed account lookup or specification error.
    pub fn default_shell(initial_size: PtySize) -> Result<Self, SpawnError> {
        let shell = system_shell()?;
        Self::command(shell, vec![OsString::from("-i")], initial_size)
    }
}

fn validate_word(kind: &'static str, value: &OsStr) -> Result<(), SpawnError> {
    if value.is_empty() || value.as_bytes().contains(&0) {
        return Err(SpawnError::InvalidWord(kind));
    }
    Ok(())
}

fn system_shell() -> Result<OsString, SpawnError> {
    let mut buffer = vec![0_u8; 16 * 1024];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut found = std::ptr::null_mut();
    // SAFETY: all pointers refer to writable storage valid for the duration of getpwuid_r.
    let result = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &raw mut found,
        )
    };
    if result != 0 {
        return Err(SpawnError::ShellLookup(io::Error::from_raw_os_error(
            result,
        )));
    }
    if found.is_null() {
        return Err(SpawnError::ShellMissing);
    }
    // SAFETY: a successful getpwuid_r record points into `buffer` and is NUL terminated.
    let bytes = unsafe { std::ffi::CStr::from_ptr((*found).pw_shell) }.to_bytes();
    let shell = OsString::from(OsStr::from_bytes(bytes));
    if !PathBuf::from(&shell).is_absolute() || shell.is_empty() {
        return Err(SpawnError::ShellInvalid);
    }
    Ok(shell)
}

#[derive(Clone)]
pub struct PtySinks {
    pub output: Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>,
    pub read_closed: Arc<dyn Fn() -> bool + Send + Sync>,
    pub exited: Arc<dyn Fn(ChildExit) + Send + Sync>,
    pub failed: Arc<dyn Fn(String) + Send + Sync>,
    pub writable: Arc<dyn Fn() + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildExit {
    Code(i32),
    Signaled { signal: i32, core_dumped: bool },
    Other(i32),
}

enum CommandMessage {
    Write(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriteStatus {
    Accepted,
    WouldBlock,
    Closed,
}

pub struct PtyProcess {
    commands: Sender<CommandMessage>,
    shutdown: Arc<AtomicBool>,
    outstanding_write_bytes: Arc<AtomicUsize>,
    latest_resize: Arc<Mutex<Option<PtySize>>>,
    io_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

impl PtyProcess {
    /// Creates the PTY, executes the child, and starts its uniquely owning workers.
    ///
    /// # Errors
    /// Returns a typed resource, execution, or thread creation error.
    pub fn spawn(spec: SpawnSpec, sinks: PtySinks) -> Result<Self, SpawnError> {
        validate_spec(&spec)?;
        let (master, slave) = open_pty(spec.initial_size)?;
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.environment);
        let stdin = slave.try_clone().map_err(SpawnError::Pty)?;
        let stdout = slave.try_clone().map_err(SpawnError::Pty)?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave));
        // SAFETY: the closure only invokes async-signal-safe libc calls and reports errno through Command's exec handshake.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|source| SpawnError::Exec {
            program: spec.program,
            source,
        })?;
        let pidfd = match open_pidfd(child.id()) {
            Ok(pidfd) => pidfd,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SpawnError::PidFd(error));
            }
        };
        let (commands, command_rx) = bounded(16);
        let shutdown = Arc::new(AtomicBool::new(false));
        let outstanding_write_bytes = Arc::new(AtomicUsize::new(0));
        let latest_resize = Arc::new(Mutex::new(None));
        let io_sinks = sinks.clone();
        let io_shutdown = Arc::clone(&shutdown);
        let io_outstanding = Arc::clone(&outstanding_write_bytes);
        let io_resize = Arc::clone(&latest_resize);
        let io_thread = thread::Builder::new()
            .name("leyline-pty-io".into())
            .spawn(move || {
                io_worker(
                    master,
                    &command_rx,
                    &io_shutdown,
                    &io_outstanding,
                    &io_resize,
                    &io_sinks,
                );
            })
            .map_err(|error| {
                shutdown.store(true, Ordering::Release);
                let _ = signal_pidfd(pidfd.as_raw_fd(), libc::SIGKILL);
                let _ = child.wait();
                SpawnError::Thread(error)
            })?;
        let wait_shutdown = Arc::clone(&shutdown);
        let child_owner = ChildOwner::new(child, pidfd);
        let wait_thread = match thread::Builder::new()
            .name("leyline-pty-wait".into())
            .spawn(move || wait_worker(child_owner, &wait_shutdown, &sinks))
        {
            Ok(thread) => thread,
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = io_thread.join();
                return Err(SpawnError::Thread(error));
            }
        };
        Ok(Self {
            commands,
            shutdown,
            outstanding_write_bytes,
            latest_resize,
            io_thread: Some(io_thread),
            wait_thread: Some(wait_thread),
        })
    }

    /// Queues a terminal window resize.
    ///
    /// # Errors
    /// The control lane is latest-wins and independent of input backpressure.
    pub fn resize(&self, size: PtySize) -> Result<(), PtyCommandError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(PtyCommandError::Unavailable);
        }
        *self
            .latest_resize
            .lock()
            .map_err(|_| PtyCommandError::Unavailable)? = Some(size);
        Ok(())
    }
    /// Queues a bounded parser response for the PTY master.
    ///
    /// # Errors
    /// Returns an error for invalid bytes or an unavailable bounded endpoint.
    pub fn try_write(&self, bytes: Vec<u8>) -> Result<WriteStatus, PtyCommandError> {
        if bytes.is_empty() || bytes.len() > MAX_BATCH {
            return Err(PtyCommandError::InvalidWrite);
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Ok(WriteStatus::Closed);
        }
        let len = bytes.len();
        let reserved = self.outstanding_write_bytes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| {
                current
                    .checked_add(len)
                    .filter(|next| *next <= MAX_OUTSTANDING_WRITE_BYTES)
            },
        );
        if reserved.is_err() {
            return Ok(WriteStatus::WouldBlock);
        }
        match self.commands.try_send(CommandMessage::Write(bytes)) {
            Ok(()) => Ok(WriteStatus::Accepted),
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                self.outstanding_write_bytes
                    .fetch_sub(len, Ordering::AcqRel);
                Ok(WriteStatus::WouldBlock)
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {
                self.outstanding_write_bytes
                    .fetch_sub(len, Ordering::AcqRel);
                Ok(WriteStatus::Closed)
            }
        }
    }
    #[must_use]
    pub fn outstanding_write_bytes(&self) -> usize {
        self.outstanding_write_bytes.load(Ordering::Acquire)
    }
    /// Requests worker shutdown and master closure.
    ///
    /// # Errors
    /// Returns [`PtyCommandError::Unavailable`] when the endpoint is already unavailable.
    pub fn request_shutdown(&self) -> Result<(), PtyCommandError> {
        self.shutdown.store(true, Ordering::Release);
        Ok(())
    }
    /// Waits for both owning workers to finish and observes panics.
    ///
    /// # Errors
    /// Returns a typed error when either worker panicked.
    pub fn join(mut self) -> Result<(), JoinError> {
        self.join_inner(false)
    }
    /// Joins the workers only after both have finished.
    ///
    /// This is safe to call from an event loop because it never waits for a live worker.
    ///
    /// # Errors
    /// Returns a typed error when either completed worker panicked.
    pub fn try_join(&mut self) -> Result<bool, JoinError> {
        let io_finished = self
            .io_thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished);
        let wait_finished = self
            .wait_thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished);
        if !io_finished || !wait_finished {
            return Ok(false);
        }
        self.join_inner(false)?;
        Ok(true)
    }
    fn join_inner(&mut self, shutdown: bool) -> Result<(), JoinError> {
        if shutdown {
            self.shutdown.store(true, Ordering::Release);
        }
        if self
            .io_thread
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            return Err(JoinError::WorkerPanic);
        }
        if self
            .wait_thread
            .take()
            .is_some_and(|handle| handle.join().is_err())
        {
            return Err(JoinError::WaiterPanic);
        }
        Ok(())
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Dropping JoinHandle detaches; shutdown ownership stays in worker-owned Arcs.
        self.io_thread.take();
        self.wait_thread.take();
    }
}

fn validate_spec(spec: &SpawnSpec) -> Result<(), SpawnError> {
    validate_word("program", &spec.program)?;
    for arg in &spec.args {
        validate_word("argument", arg)?;
    }
    for (key, value) in &spec.environment {
        if key.is_empty()
            || key.as_bytes().contains(&b'=')
            || key.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err(SpawnError::InvalidEnvironment);
        }
    }
    Ok(())
}

fn open_pty(size: PtySize) -> Result<(File, File), SpawnError> {
    let mut master = -1;
    let mut slave = -1;
    let winsize = size.winsize();
    // SAFETY: openpty initializes both fd outputs; winsize points to a valid value.
    if unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &raw const winsize,
        )
    } == -1
    {
        return Err(SpawnError::Pty(io::Error::last_os_error()));
    }
    for fd in [master, slave] {
        // SAFETY: both descriptors are valid and F_GETFD/F_SETFD do not transfer ownership.
        let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if descriptor_flags == -1
            || unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } == -1
        {
            let error = io::Error::last_os_error();
            // SAFETY: both descriptors are still uniquely owned here.
            unsafe {
                libc::close(master);
                libc::close(slave);
            }
            return Err(SpawnError::Pty(error));
        }
    }
    // SAFETY: `master` is valid and F_GETFL/F_SETFL do not transfer ownership.
    let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        let error = io::Error::last_os_error();
        // SAFETY: both descriptors are still uniquely owned here.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(SpawnError::Pty(error));
    }
    // SAFETY: successful openpty returned uniquely owned descriptors.
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

#[allow(clippy::needless_pass_by_value)] // Moving File documents and enforces exclusive fd ownership.
#[allow(clippy::too_many_lines)]
fn io_worker(
    master: File,
    commands: &Receiver<CommandMessage>,
    shutdown: &AtomicBool,
    outstanding: &AtomicUsize,
    latest_resize: &Mutex<Option<PtySize>>,
    sinks: &PtySinks,
) {
    let fd = master.as_raw_fd();
    let mut pending_write = Vec::new();
    'outer: loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Ok(mut resize) = latest_resize.lock()
            && let Some(size) = resize.take()
        {
            let winsize = size.winsize();
            if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize) } == -1 {
                (sinks.failed)(format!("resize: {}", io::Error::last_os_error()));
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                CommandMessage::Write(bytes) => pending_write.extend(bytes),
            }
        }
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN
                | if pending_write.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                },
            revents: 0,
        }];
        let poll = unsafe { libc::poll(fds.as_mut_ptr(), 1, 25) };
        if poll < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            (sinks.failed)(format!("poll: {}", io::Error::last_os_error()));
            break;
        }
        if fds[0].revents & libc::POLLOUT != 0 && !pending_write.is_empty() {
            let count =
                unsafe { libc::write(fd, pending_write.as_ptr().cast(), pending_write.len()) };
            if count > 0 {
                let count = count.cast_unsigned();
                pending_write.drain(..count);
                outstanding.fetch_sub(count, Ordering::AcqRel);
                (sinks.writable)();
            }
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut batch = Vec::with_capacity(MAX_BATCH);
            loop {
                let mut chunk = [0_u8; READ_CHUNK];
                let count = unsafe {
                    libc::read(
                        fd,
                        chunk.as_mut_ptr().cast(),
                        chunk.len().min(MAX_BATCH - batch.len()),
                    )
                };
                if count > 0 {
                    batch.extend_from_slice(&chunk[..count.cast_unsigned()]);
                    if batch.len() == MAX_BATCH {
                        break;
                    }
                    continue;
                }
                if count == 0 {
                    if !batch.is_empty() && !(sinks.output)(batch) {
                        break 'outer;
                    }
                    let _ = (sinks.read_closed)();
                    break 'outer;
                }
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EIO) {
                    if !batch.is_empty() && !(sinks.output)(batch) {
                        break 'outer;
                    }
                    let _ = (sinks.read_closed)();
                    break 'outer;
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    break;
                }
                (sinks.failed)(format!("read: {error}"));
                break 'outer;
            }
            if !batch.is_empty() && !(sinks.output)(batch) {
                break;
            }
        }
    }
    let discarded = pending_write.len()
        + commands
            .try_iter()
            .map(|command| match command {
                CommandMessage::Write(bytes) => bytes.len(),
            })
            .sum::<usize>();
    if discarded != 0 {
        outstanding.fetch_sub(discarded, Ordering::AcqRel);
        (sinks.writable)();
    }
}

struct ChildOwner {
    child: Child,
    pidfd: File,
    reaped: bool,
}

impl ChildOwner {
    fn new(child: Child, pidfd: File) -> Self {
        Self {
            child,
            pidfd,
            reaped: false,
        }
    }
    fn finish(&mut self, status: std::process::ExitStatus) -> ChildExit {
        self.reaped = true;
        map_exit(status)
    }
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = signal_pidfd(self.pidfd.as_raw_fd(), libc::SIGKILL);
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

fn wait_worker(mut owner: ChildOwner, shutdown: &AtomicBool, sinks: &PtySinks) {
    const TERM_GRACE: Duration = Duration::from_secs(1);
    let mut term_sent: Option<Instant> = None;
    loop {
        match owner.child.try_wait() {
            Ok(Some(status)) => {
                (sinks.exited)(owner.finish(status));
                return;
            }
            Ok(None) => {}
            Err(error) => {
                (sinks.failed)(format!("wait: {error}"));
                return;
            }
        }
        if shutdown.load(Ordering::Acquire) {
            if let Some(sent) = term_sent {
                if sent.elapsed() >= TERM_GRACE {
                    if let Err(error) = signal_pidfd(owner.pidfd.as_raw_fd(), libc::SIGKILL) {
                        (sinks.failed)(format!("pidfd SIGKILL: {error}"));
                    }
                    match owner.child.wait() {
                        Ok(status) => (sinks.exited)(owner.finish(status)),
                        Err(error) => (sinks.failed)(format!("wait after SIGKILL: {error}")),
                    }
                    return;
                }
            } else {
                if let Err(error) = signal_pidfd(owner.pidfd.as_raw_fd(), libc::SIGTERM) {
                    (sinks.failed)(format!("pidfd SIGTERM: {error}"));
                }
                term_sent = Some(Instant::now());
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn map_exit(status: std::process::ExitStatus) -> ChildExit {
    use std::os::unix::process::ExitStatusExt;
    status.code().map_or_else(
        || {
            status
                .signal()
                .map_or(ChildExit::Other(status.into_raw()), |signal| {
                    ChildExit::Signaled {
                        signal,
                        core_dumped: status.core_dumped(),
                    }
                })
        },
        ChildExit::Code,
    )
}

use std::os::fd::AsRawFd;

fn open_pidfd(pid: u32) -> io::Result<File> {
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returned a uniquely owned descriptor.
    let fd = i32::try_from(fd).map_err(|_| io::Error::other("pidfd exceeds i32"))?;
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn signal_pidfd(pidfd: RawFd, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: pidfd_send_signal uses a valid owned pidfd, scalar signal, and null siginfo.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("PTY size must be nonzero")]
    InvalidSize,
    #[error("invalid {0}: empty or contains NUL")]
    InvalidWord(&'static str),
    #[error("invalid environment entry")]
    InvalidEnvironment,
    #[error("cannot determine current directory: {0}")]
    CurrentDirectory(io::Error),
    #[error("cannot query account shell: {0}")]
    ShellLookup(io::Error),
    #[error("current user has no account record")]
    ShellMissing,
    #[error("account shell must be a nonempty absolute path")]
    ShellInvalid,
    #[error("cannot create PTY: {0}")]
    Pty(io::Error),
    #[error("cannot open child pidfd: {0}")]
    PidFd(io::Error),
    #[error("cannot execute {program:?}: {source}")]
    Exec {
        program: OsString,
        source: io::Error,
    },
    #[error("cannot start PTY thread: {0}")]
    Thread(io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum PtyCommandError {
    #[error("PTY command endpoint is unavailable or full")]
    Unavailable,
    #[error("PTY write must contain 1..=65536 bytes")]
    InvalidWrite,
}

#[derive(Debug, thiserror::Error)]
pub enum JoinError {
    #[error("PTY I/O worker panicked")]
    WorkerPanic,
    #[error("PTY waiter panicked")]
    WaiterPanic,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::ffi::OsStringExt,
        sync::{Mutex, mpsc},
        time::Duration,
    };

    fn collect(
        spec: SpawnSpec,
    ) -> (
        PtyProcess,
        Arc<Mutex<Vec<u8>>>,
        mpsc::Receiver<&'static str>,
    ) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let output_sink = Arc::clone(&output);
        let (done_tx, done_rx) = mpsc::channel();
        let close_tx = done_tx.clone();
        let exit_tx = done_tx;
        let process = PtyProcess::spawn(
            spec,
            PtySinks {
                output: Arc::new(move |bytes| {
                    output_sink.lock().unwrap().extend(bytes);
                    true
                }),
                read_closed: Arc::new(move || close_tx.send("closed").is_ok()),
                exited: Arc::new(move |_| {
                    let _ = exit_tx.send("exited");
                }),
                failed: Arc::new(|message| panic!("PTY failure: {message}")),
                writable: Arc::new(|| {}),
            },
        )
        .unwrap();
        (process, output, done_rx)
    }

    #[test]
    fn real_pty_preserves_output_environment_and_exit_ordering() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let spec = SpawnSpec::command(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf '%s:%s' \"$TERM\" \"$COLORTERM\"".into(),
            ],
            size,
        )
        .unwrap();
        let (process, output, done_rx) = collect(spec);
        let first = done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let second = done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_ne!(first, second);
        process.join().unwrap();
        let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        assert!(text.contains("xterm-256color:truecolor"), "{text:?}");
    }

    #[test]
    fn default_shell_uses_absolute_account_shell_and_interactive_non_login_argv() {
        let spec = SpawnSpec::default_shell(PtySize::new(80, 24, 0, 0).unwrap()).unwrap();
        assert!(PathBuf::from(&spec.program).is_absolute());
        assert_eq!(spec.args, [OsString::from("-i")]);
    }

    #[test]
    fn slave_is_controlling_tty_and_argv_bytes_are_not_reparsed() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let non_utf8 = OsString::from_vec(vec![b'a', b' ', b'-', 0xff]);
        let script = "test -t 0 && test -t 1 && test -t 2 || exit 9; \
                      test \"$(ps -o pid= -p $$)\" = \"$(ps -o sid= -p $$)\" || exit 10; \
                      printf '%s' \"$1\" | od -An -tx1";
        let spec = SpawnSpec::command(
            "/bin/sh".into(),
            vec!["-c".into(), script.into(), "helper".into(), non_utf8],
            size,
        )
        .unwrap();
        let (process, output, done) = collect(spec);
        let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
        let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
        process.join().unwrap();
        let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        assert!(text.contains("61 20 2d ff"), "{text:?}");
    }

    #[test]
    fn resize_delivers_sigwinch_and_new_kernel_size() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let script = "trap 'stty size; exit 0' WINCH; printf ready; while :; do read x; done";
        let spec =
            SpawnSpec::command("/bin/sh".into(), vec!["-c".into(), script.into()], size).unwrap();
        let (process, output, done) = collect(spec);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !output
            .lock()
            .unwrap()
            .windows(5)
            .any(|bytes| bytes == b"ready")
        {
            assert!(Instant::now() < deadline, "helper did not become ready");
            thread::sleep(Duration::from_millis(5));
        }
        process.resize(PtySize::new(77, 23, 0, 0).unwrap()).unwrap();
        let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
        let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
        process.join().unwrap();
        let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
        assert!(text.contains("23 77"), "{text:?}");
    }

    #[test]
    fn latest_resize_bypasses_saturated_input_lane() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let script = "stty -echo; trap 'printf RESIZE:; stty size' WINCH; \
                      printf ready; while :; do :; done";
        let spec =
            SpawnSpec::command("/bin/sh".into(), vec!["-c".into(), script.into()], size).unwrap();
        let (process, output, _done) = collect(spec);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !output
            .lock()
            .unwrap()
            .windows(5)
            .any(|bytes| bytes == b"ready")
        {
            assert!(Instant::now() < deadline, "helper did not become ready");
            thread::sleep(Duration::from_millis(5));
        }

        let batch = vec![b'x'; MAX_BATCH];
        let mut saturated = false;
        for _ in 0..64 {
            if process.try_write(batch.clone()).unwrap() == WriteStatus::WouldBlock {
                saturated = true;
                break;
            }
        }
        assert!(saturated, "input lane did not reach bounded backpressure");

        for columns in 80..93 {
            process
                .resize(PtySize::new(columns, 37, 0, 0).unwrap())
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !String::from_utf8_lossy(&output.lock().unwrap()).contains("RESIZE:37 92") {
            assert!(
                Instant::now() < deadline,
                "final resize did not bypass input backpressure"
            );
            thread::sleep(Duration::from_millis(5));
        }
        process.request_shutdown().unwrap();
        process.join().unwrap();
    }

    #[test]
    fn shutdown_escalates_for_child_ignoring_sigterm() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let script = "trap '' TERM HUP; printf ready; while :; do :; done";
        let spec =
            SpawnSpec::command("/bin/sh".into(), vec!["-c".into(), script.into()], size).unwrap();
        let (process, output, _done) = collect(spec);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !output
            .lock()
            .unwrap()
            .windows(5)
            .any(|bytes| bytes == b"ready")
        {
            assert!(Instant::now() < deadline, "helper did not become ready");
            thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        process.request_shutdown().unwrap();
        process.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn spawn_reports_missing_program_and_working_directory() {
        let size = PtySize::new(40, 12, 0, 0).unwrap();
        let missing =
            SpawnSpec::command("/definitely/missing/leyline".into(), Vec::new(), size).unwrap();
        let sinks = PtySinks {
            output: Arc::new(|_| true),
            read_closed: Arc::new(|| true),
            exited: Arc::new(|_| {}),
            failed: Arc::new(|_| {}),
            writable: Arc::new(|| {}),
        };
        assert!(matches!(
            PtyProcess::spawn(missing, sinks.clone()),
            Err(SpawnError::Exec { .. })
        ));
        let mut bad_cwd = SpawnSpec::command("/bin/true".into(), Vec::new(), size).unwrap();
        bad_cwd.cwd = PathBuf::from("/definitely/missing/leyline-cwd");
        assert!(matches!(
            PtyProcess::spawn(bad_cwd, sinks),
            Err(SpawnError::Exec { .. })
        ));
    }

    #[test]
    fn repeated_sessions_do_not_leak_process_file_descriptors() {
        let count_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        let baseline = count_fds();
        for _ in 0..32 {
            let size = PtySize::new(10, 2, 0, 0).unwrap();
            let spec = SpawnSpec::command("/bin/true".into(), Vec::new(), size).unwrap();
            let (process, _output, done) = collect(spec);
            let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
            let _ = done.recv_timeout(Duration::from_secs(3)).unwrap();
            process.join().unwrap();
        }
        assert!(
            count_fds() <= baseline + 2,
            "process fd count grew unexpectedly"
        );
    }
}
