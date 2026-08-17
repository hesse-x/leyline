#![allow(clippy::missing_errors_doc)]

use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{Read as _, Write as _},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::diagnostics::{ClassifiedError, ErrorCategory};

pub const TERM_NAME: &str = "leyline-256color";
pub const SOURCE: &str = include_str!("../../../terminfo/leyline.terminfo");
pub const MANIFEST: &str = include_str!("../../../terminfo/capabilities.toml");
const MAX_TOOL_OUTPUT: usize = 1024 * 1024;
const SSH_PROBE_SCRIPT: &str = "set -eu\nprintf 'term=%s\\n' \"${TERM-unknown}\"\nif infocmp -x -1 leyline-256color >/dev/null 2>&1; then printf 'entry=present\\n'; else printf 'entry=missing\\n'; fi\nprintf 'ncurses='; infocmp -V 2>/dev/null || printf 'unavailable\\n'\nif command -v tmux >/dev/null 2>&1; then printf 'tmux='; tmux -V; else printf 'tmux=unavailable\\n'; fi\n";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalIdentity {
    #[default]
    Leyline,
    Xterm256Color,
}

impl TerminalIdentity {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "leyline" => Some(Self::Leyline),
            "xterm-256color" => Some(Self::Xterm256Color),
            _ => None,
        }
    }

    #[must_use]
    pub const fn term(self) -> &'static str {
        match self {
            Self::Leyline => TERM_NAME,
            Self::Xterm256Color => "xterm-256color",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TerminfoError {
    #[error("required tool {0} is unavailable; install ncurses-bin (>= 6.4)")]
    ToolMissing(&'static str),
    #[error("terminfo entry {term} is missing from the effective database; {hint}")]
    EntryMissing {
        term: &'static str,
        hint: &'static str,
    },
    #[error("terminfo entry {term} does not match Leyline epoch 1: {reason}")]
    EntryMismatch { term: &'static str, reason: String },
    #[error("invalid terminfo database path")]
    InvalidDatabase,
    #[error("terminfo database operation failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0} failed: {1}")]
    ToolFailed(&'static str, String),
    #[error("terminfo tool output exceeded the 1 MiB limit")]
    OutputTooLarge,
    #[error("{0} exceeded the 10 second timeout")]
    ToolTimeout(&'static str),
    #[error("another Leyline terminfo install or uninstall is in progress")]
    Busy,
    #[error("refusing to overwrite or remove an entry not owned by this Leyline installation")]
    NotOwned,
    #[error("compiled terminfo did not contain exactly one regular entry")]
    InvalidCompiledOutput,
    #[error("SSH probe failed: {0}")]
    Ssh(String),
}

impl ClassifiedError for TerminfoError {
    fn category(&self) -> ErrorCategory {
        match self {
            Self::EntryMismatch { .. } | Self::NotOwned => ErrorCategory::Platform,
            Self::ToolMissing(_) | Self::EntryMissing { .. } | Self::InvalidDatabase => {
                ErrorCategory::Environment
            }
            Self::Io(_)
            | Self::ToolFailed(_, _)
            | Self::OutputTooLarge
            | Self::ToolTimeout(_)
            | Self::Busy
            | Self::InvalidCompiledOutput => ErrorCategory::Internal,
            Self::Ssh(_) => ErrorCategory::Remote,
        }
    }
}

pub fn doctor_ssh(host: &OsStr, json: bool) -> Result<String, TerminfoError> {
    if host.as_bytes().contains(&0) {
        return Err(TerminfoError::InvalidDatabase);
    }
    let mut command = Command::new("ssh");
    command
        .args(["-o", "ConnectTimeout=10", "--"])
        .arg(host)
        .args(["sh", "-s"]);
    let output = run_tool_with_input(command, "ssh", Some(SSH_PROBE_SCRIPT.as_bytes())).map_err(
        |error| match error {
            other @ (TerminfoError::ToolTimeout(_)
            | TerminfoError::OutputTooLarge
            | TerminfoError::ToolFailed(_, _)) => TerminfoError::Ssh(other.to_string()),
            other => other,
        },
    )?;
    if !output.status.success() {
        return Err(TerminfoError::Ssh(bounded_text(&output.stderr)));
    }
    let observed = String::from_utf8_lossy(&output.stdout);
    let host_id = anonymous_host_id(host);
    if json {
        let entry = if observed.lines().any(|line| line == "entry=present") {
            "present"
        } else {
            "missing"
        };
        Ok(format!(
            "{{\"schema_version\":1,\"host_id\":\"{host_id}\",\"term\":\"{TERM_NAME}\",\"entry\":\"{entry}\"}}\n"
        ))
    } else {
        Ok(format!("schema_version=1\nhost_id={host_id}\n{observed}"))
    }
}

fn anonymous_host_id(host: &OsStr) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    host.as_bytes().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn preflight(identity: TerminalIdentity) -> Result<(), TerminfoError> {
    let output = infocmp(identity.term(), None)?;
    if identity == TerminalIdentity::Leyline {
        validate_leyline(&output)?;
    }
    Ok(())
}

pub fn check(database: Option<&OsStr>) -> Result<String, TerminfoError> {
    let output = infocmp(TERM_NAME, database)?;
    validate_leyline(&output)?;
    Ok(format!("status=ok term={TERM_NAME} epoch=1 rgb=declared\n"))
}

pub fn doctor() -> Result<String, TerminfoError> {
    let output = infocmp(TERM_NAME, None)?;
    validate_leyline(&output)?;
    Ok(format!(
        "schema_version=1\nstatus=ok\nterm={TERM_NAME}\nepoch=1\nrgb=declared\n"
    ))
}

pub fn install(database: Option<&OsStr>) -> Result<String, TerminfoError> {
    let database = user_database(database)?;
    ensure_safe_database(&database)?;
    fs::create_dir_all(&database)?;
    let _lock = InstallLock::acquire(&database)?;
    let staging = database.join(format!(".leyline-staging-{}", std::process::id()));
    if staging.exists() {
        return Err(TerminfoError::Busy);
    }
    fs::create_dir(&staging)?;
    let source = staging.join("leyline.terminfo");
    fs::write(&source, SOURCE.as_bytes())?;
    let result = (|| {
        run_tic(&source, &staging)?;
        let entry = find_compiled_entry(&staging, &source)?;
        let relative = entry
            .strip_prefix(&staging)
            .map_err(|_| TerminfoError::InvalidCompiledOutput)?;
        let destination = database.join(relative);
        let ownership = ownership_path(&database);
        ensure_safe_child_directory(
            &database,
            destination.parent().ok_or(TerminfoError::InvalidDatabase)?,
        )?;
        ensure_safe_child_directory(
            &database,
            ownership.parent().ok_or(TerminfoError::InvalidDatabase)?,
        )?;
        if destination.exists() && !ownership.exists() {
            return Err(TerminfoError::NotOwned);
        }
        if destination.exists() {
            let expected = fs::read(&ownership).map_err(|_| TerminfoError::NotOwned)?;
            let current = infocmp(TERM_NAME, Some(database.as_os_str()))?;
            if expected != current {
                return Err(TerminfoError::NotOwned);
            }
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&entry, &destination)?;
        if let Some(parent) = ownership.parent() {
            fs::create_dir_all(parent)?;
        }
        let normalized = infocmp(TERM_NAME, Some(database.as_os_str()))?;
        validate_leyline(&normalized)?;
        let ownership_staging = staging.join("ownership");
        fs::write(&ownership_staging, &normalized)?;
        fs::rename(&ownership_staging, &ownership)?;
        Ok(format!(
            "status=installed term={TERM_NAME} path={}\n",
            destination.display()
        ))
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

pub fn uninstall(database: Option<&OsStr>) -> Result<String, TerminfoError> {
    let database = user_database(database)?;
    ensure_safe_database(&database)?;
    let _lock = InstallLock::acquire(&database)?;
    let ownership = ownership_path(&database);
    let expected = fs::read(&ownership).map_err(|_| TerminfoError::NotOwned)?;
    let current = infocmp(TERM_NAME, Some(database.as_os_str()))?;
    if expected != current {
        return Err(TerminfoError::NotOwned);
    }
    let entry = find_named_entry(&database)?.ok_or(TerminfoError::NotOwned)?;
    fs::remove_file(&entry)?;
    fs::remove_file(&ownership)?;
    Ok(format!(
        "status=uninstalled term={TERM_NAME} path={}\n",
        entry.display()
    ))
}

fn validate_leyline(output: &[u8]) -> Result<(), TerminfoError> {
    let text = String::from_utf8_lossy(output);
    if !text.lines().any(|line| line.trim() == "RGB,") {
        return Err(TerminfoError::EntryMismatch {
            term: TERM_NAME,
            reason: "RGB is not declared".into(),
        });
    }
    for rejected in [
        "Ms=", "Cs=", "Cr=", "Ss=", "Se=", "initc=", "mc0=", "mc4=", "mc5=",
    ] {
        if text
            .lines()
            .any(|line| line.trim_start().starts_with(rejected))
        {
            return Err(TerminfoError::EntryMismatch {
                term: TERM_NAME,
                reason: format!("rejected capability {rejected} is present"),
            });
        }
    }
    Ok(())
}

fn infocmp(term: &'static str, database: Option<&OsStr>) -> Result<Vec<u8>, TerminfoError> {
    let mut command = Command::new("infocmp");
    command.args(["-x", "-1"]);
    if let Some(database) = database {
        command.arg("-A").arg(database);
    }
    command.arg(term);
    let output = run_tool(command, "infocmp")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("couldn't open terminfo file") || stderr.contains("unknown terminal") {
            return Err(TerminfoError::EntryMissing {
                term,
                hint: if term == TERM_NAME {
                    "run `leyline terminfo install --user` or select `--term xterm-256color`"
                } else {
                    "install the system xterm-256color entry"
                },
            });
        }
        return Err(TerminfoError::ToolFailed(
            "infocmp",
            bounded_text(&output.stderr),
        ));
    }
    Ok(output.stdout)
}

fn run_tic(source: &Path, output_dir: &Path) -> Result<(), TerminfoError> {
    let mut command = Command::new("tic");
    command.args(["-x", "-o"]).arg(output_dir).arg(source);
    let output = run_tool(command, "tic")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(TerminfoError::ToolFailed(
            "tic",
            bounded_text(&output.stderr),
        ))
    }
}

fn run_tool(command: Command, name: &'static str) -> Result<Output, TerminfoError> {
    run_tool_with_input(command, name, None)
}

fn run_tool_with_input(
    mut command: Command,
    name: &'static str,
    input: Option<&[u8]>,
) -> Result<Output, TerminfoError> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            TerminfoError::ToolMissing(name)
        } else {
            TerminfoError::Io(error)
        }
    })?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .ok_or_else(|| TerminfoError::ToolFailed(name, "stdin unavailable".into()))?
            .write_all(input)?;
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| TerminfoError::ToolFailed(name, "stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| TerminfoError::ToolFailed(name, "stderr unavailable".into()))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take((MAX_TOOL_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .take((MAX_TOOL_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(TerminfoError::ToolTimeout(name));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| TerminfoError::ToolFailed(name, "stdout reader panicked".into()))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| TerminfoError::ToolFailed(name, "stderr reader panicked".into()))??;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    if output.stdout.len().saturating_add(output.stderr.len()) > MAX_TOOL_OUTPUT {
        return Err(TerminfoError::OutputTooLarge);
    }
    Ok(output)
}

fn bounded_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(4096)
        .collect()
}

fn user_database(database: Option<&OsStr>) -> Result<PathBuf, TerminfoError> {
    let path = database
        .map_or_else(
            || std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".terminfo")),
            |path| Some(PathBuf::from(path)),
        )
        .ok_or(TerminfoError::InvalidDatabase)?;
    if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
        return Err(TerminfoError::InvalidDatabase);
    }
    Ok(path)
}

fn ensure_safe_database(database: &Path) -> Result<(), TerminfoError> {
    if let Ok(metadata) = fs::symlink_metadata(database)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(TerminfoError::InvalidDatabase);
    }
    Ok(())
}

fn ensure_safe_child_directory(database: &Path, child: &Path) -> Result<(), TerminfoError> {
    let relative = child
        .strip_prefix(database)
        .map_err(|_| TerminfoError::InvalidDatabase)?;
    let mut current = database.to_owned();
    for component in relative.components() {
        current.push(component);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(TerminfoError::InvalidDatabase);
        }
    }
    Ok(())
}

fn ownership_path(database: &Path) -> PathBuf {
    database.join(".leyline/leyline-256color.owner")
}

fn find_compiled_entry(root: &Path, source: &Path) -> Result<PathBuf, TerminfoError> {
    let mut entries = Vec::new();
    collect_files(root, &mut entries)?;
    entries.retain(|entry| entry != source);
    if entries.len() == 1 {
        Ok(entries.remove(0))
    } else {
        Err(TerminfoError::InvalidCompiledOutput)
    }
}

fn find_named_entry(root: &Path) -> Result<Option<PathBuf>, TerminfoError> {
    let mut entries = Vec::new();
    collect_files(root, &mut entries)?;
    Ok(entries.into_iter().find(|path| {
        path.file_name() == Some(OsStr::new(TERM_NAME)) && !path.starts_with(root.join(".leyline"))
    }))
}

fn collect_files(root: &Path, output: &mut Vec<PathBuf>) -> Result<(), TerminfoError> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(TerminfoError::InvalidDatabase);
        }
        if file_type.is_dir() {
            collect_files(&entry.path(), output)?;
        } else if file_type.is_file() {
            output.push(entry.path());
        }
    }
    Ok(())
}

struct InstallLock(PathBuf);

impl InstallLock {
    fn acquire(database: &Path) -> Result<Self, TerminfoError> {
        let state = database.join(".leyline");
        ensure_safe_child_directory(database, &state)?;
        fs::create_dir_all(&state)?;
        let path = database.join(".leyline/install.lock");
        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    TerminfoError::Busy
                } else {
                    TerminfoError::Io(error)
                }
            })?;
        writeln!(lock, "pid={}", std::process::id())?;
        Ok(Self(path))
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn canonical_source_is_standalone_and_omits_rejected_capabilities() {
        assert!(
            !SOURCE
                .lines()
                .any(|line| line.trim_start().starts_with("use="))
        );
        assert!(SOURCE.contains("\tRGB,"));
        for rejected in ["Ms=", "Cs=", "Cr=", "Ss=", "Se=", "initc=", "mc0="] {
            assert!(!SOURCE.contains(rejected), "source contains {rejected}");
        }
        assert!(MANIFEST.contains("schema = 1"));
    }

    #[test]
    fn manifest_is_a_bidirectional_allowlist_for_the_source() {
        let manifest = toml::from_str::<toml::Value>(MANIFEST).expect("manifest TOML");
        let declared = manifest["capability_group"]
            .as_array()
            .expect("capability groups")
            .iter()
            .flat_map(|group| group["names"].as_array().expect("names"))
            .map(|name| name.as_str().expect("capability name").to_owned())
            .collect::<BTreeSet<_>>();
        let source = SOURCE
            .lines()
            .skip(1)
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }
                Some(
                    line.split(['#', '='])
                        .next()
                        .expect("capability")
                        .trim_end_matches(',')
                        .to_owned(),
                )
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(source, declared);
    }

    #[test]
    fn user_install_upgrade_and_uninstall_are_reversible() {
        let temp = tempfile::tempdir().expect("temporary database");
        let database = temp.path().join("terminfo");
        install(Some(database.as_os_str())).expect("install");
        check(Some(database.as_os_str())).expect("check");
        install(Some(database.as_os_str())).expect("owned upgrade");
        uninstall(Some(database.as_os_str())).expect("uninstall");
        assert!(matches!(
            check(Some(database.as_os_str())),
            Err(TerminfoError::EntryMissing { .. })
        ));
    }

    #[test]
    fn uninstall_refuses_a_changed_ownership_record() {
        let temp = tempfile::tempdir().expect("temporary database");
        let database = temp.path().join("terminfo");
        install(Some(database.as_os_str())).expect("install");
        fs::write(ownership_path(&database), b"changed").expect("tamper ownership");
        assert!(matches!(
            uninstall(Some(database.as_os_str())),
            Err(TerminfoError::NotOwned)
        ));
    }

    #[test]
    fn install_rejects_a_symlinked_state_directory() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("temporary database");
        let database = temp.path().join("terminfo");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&database).expect("database");
        fs::create_dir(&outside).expect("outside");
        symlink(&outside, database.join(".leyline")).expect("state symlink");
        assert!(matches!(
            install(Some(database.as_os_str())),
            Err(TerminfoError::InvalidDatabase)
        ));
    }
}
