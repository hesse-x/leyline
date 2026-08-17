#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

#[cfg(not(target_os = "linux"))]
compile_error!("leyline-pty currently supports Linux only");

mod spawn;

pub use spawn::{
    ChildExit, JoinError, MAX_OUTSTANDING_WRITE_BYTES, PtyCommandError, PtyProcess, PtySinks,
    PtySize, SpawnDirectory, SpawnDirectoryError, SpawnError, SpawnSpec, WriteStatus,
};
