//! Lifecycle — start/stop/status of the detached daemon process.
//!
//! Two sides, deliberately kept distinct:
//! - **client side** (`start`/`stop`/`status`): what `loop daemon …` runs. It
//!   manages a pidfile and spawns/kills the daemon process; it holds no business
//!   logic and no long-lived state.
//! - **server side**: the detached child re-execs this same binary with the
//!   hidden `daemon serve` command, which calls [`super::server::serve`] and
//!   blocks. `start` never calls `serve` in-process — it spawns a fresh,
//!   terminal-detached copy so the daemon survives the CLI exiting.
//!
//! Detach recipe (claude-squad pattern, ARCHITECTURE.md §8 Q6): re-exec
//! `current_exe()` → set stdio to a log file / null → on Windows set
//! `CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS` → spawn, **never wait** → write
//! `~/.loopd/daemon.pid`. `stop` is idempotent: a missing pidfile is success, not
//! an error, so `loop daemon stop` is safe to call twice.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::config::{ensure_loopd_dir, loopd_dir, Config};

/// `~/.loopd/daemon.pid` — holds the running daemon's OS pid.
pub fn pidfile_path() -> PathBuf {
    loopd_dir().join("daemon.pid")
}

/// `~/.loopd/daemon.log` — the detached daemon's stdout+stderr.
pub fn log_path() -> PathBuf {
    loopd_dir().join("daemon.log")
}

/// Daemon liveness as the pidfile reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Pidfile present and the process is alive.
    Running { pid: u32 },
    /// No pidfile — the daemon is not running.
    Stopped,
    /// Pidfile present but the process is gone (left behind by a crash). The
    /// pid is reported so callers can clean it up.
    Stale { pid: u32 },
}

/// Outcome of [`start`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    /// A fresh daemon was spawned with this pid.
    Started { pid: u32 },
    /// A healthy daemon was already running with this pid; nothing spawned.
    AlreadyRunning { pid: u32 },
}

/// Outcome of [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// The daemon (this pid) was signalled and the pidfile removed.
    Stopped { pid: u32 },
    /// No pidfile — nothing to stop (idempotent no-op).
    NotRunning,
}

/// Current daemon status from the default pidfile.
pub fn status() -> Status {
    status_at(&pidfile_path())
}

/// Spawn the daemon detached if it isn't already running, recording its pid.
/// Idempotent: returns [`StartOutcome::AlreadyRunning`] when a live daemon is
/// found. Waiting for `/health` is the caller's job (the `DaemonClient`).
pub fn start(config: &Config) -> Result<StartOutcome> {
    if let Status::Running { pid } = status() {
        return Ok(StartOutcome::AlreadyRunning { pid });
    }
    // A stale pidfile (process gone) would otherwise block the spawn — clear it.
    remove_pidfile(&pidfile_path())?;
    let pid = spawn_detached(config)?;
    Ok(StartOutcome::Started { pid })
}

/// Stop the daemon and remove the pidfile. Idempotent — a missing pidfile is
/// success, not an error.
pub fn stop() -> Result<StopOutcome> {
    stop_at(&pidfile_path())
}

// --- internals (path-injected for tests) -------------------------------------

fn status_at(pidfile: &Path) -> Status {
    match read_pidfile(pidfile) {
        None => Status::Stopped,
        Some(pid) if process_alive(pid) => Status::Running { pid },
        Some(pid) => Status::Stale { pid },
    }
}

fn stop_at(pidfile: &Path) -> Result<StopOutcome> {
    let pid = match read_pidfile(pidfile) {
        Some(pid) => pid,
        None => return Ok(StopOutcome::NotRunning),
    };
    // Best-effort kill: the process may already be gone (stale pidfile), which
    // is fine — we still remove the pidfile so the state is clean.
    kill_pid(pid);
    remove_pidfile(pidfile)?;
    Ok(StopOutcome::Stopped { pid })
}

/// Re-exec this binary as `daemon serve`, detached from the terminal, with stdio
/// pointed at the log file. Returns the new pid. Never waits on the child.
fn spawn_detached(_config: &Config) -> Result<u32> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    ensure_loopd_dir()?;
    let log = log_path();
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening daemon log {}", log.display()))?;
    let err = out.try_clone().context("cloning daemon log handle")?;

    let mut cmd = Command::new(&exe);
    cmd.arg("daemon")
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Survive the parent exiting (new process group) and detach from the
        // console (no inherited window). claude-squad's Windows recipe.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    // On unix, redirected stdio + the CLI exiting leaves the daemon orphaned and
    // reparented to init — sufficient for v1. Full `setsid` daemonization is a
    // later refinement (needs a libc/nix dep we don't carry yet).

    let child = cmd
        .spawn()
        .with_context(|| format!("spawning detached daemon: {}", exe.display()))?;
    let pid = child.id();
    // Drop the child handle without waiting — std never kills on drop, so the
    // daemon keeps running after this process exits.
    write_pidfile(&pidfile_path(), pid)?;
    Ok(pid)
}

fn write_pidfile(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, pid.to_string())
        .with_context(|| format!("writing pidfile {}", path.display()))
}

fn read_pidfile(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn remove_pidfile(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing pidfile {}", path.display())),
    }
}

/// Is a process with `pid` alive? Shells out — no liveness syscall is in `std`,
/// and we avoid a libc/winapi dependency. Best-effort: any failure reads as dead.
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Terminate `pid` (and its child tree, so the supervised npx→node→agent chain
/// dies with the daemon later). Best-effort — ignore "no such process".
#[cfg(windows)]
fn kill_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F", "/T"])
        .output();
}

#[cfg(not(windows))]
fn kill_pid(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::new_run_id;

    fn temp_pidfile() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("loopd_life_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("daemon.pid")
    }

    #[test]
    fn pidfile_round_trips() {
        let path = temp_pidfile();
        assert_eq!(read_pidfile(&path), None);
        write_pidfile(&path, 4242).unwrap();
        assert_eq!(read_pidfile(&path), Some(4242));
        remove_pidfile(&path).unwrap();
        assert_eq!(read_pidfile(&path), None);
    }

    #[test]
    fn stop_is_idempotent_when_not_running() {
        let path = temp_pidfile();
        // No pidfile → NotRunning, no error.
        assert_eq!(stop_at(&path).unwrap(), StopOutcome::NotRunning);
        // Calling again is still a clean no-op.
        assert_eq!(stop_at(&path).unwrap(), StopOutcome::NotRunning);
    }

    #[test]
    fn stop_removes_pidfile_for_a_dead_pid() {
        let path = temp_pidfile();
        // A pid that is overwhelmingly unlikely to exist.
        write_pidfile(&path, 4_294_967_290).unwrap();
        let outcome = stop_at(&path).unwrap();
        assert!(matches!(outcome, StopOutcome::Stopped { .. }));
        assert!(!path.exists(), "pidfile must be gone after stop");
    }

    #[test]
    fn status_reflects_pidfile_and_liveness() {
        let path = temp_pidfile();
        assert_eq!(status_at(&path), Status::Stopped);

        // This test process is certainly alive → Running.
        write_pidfile(&path, std::process::id()).unwrap();
        assert_eq!(status_at(&path), Status::Running { pid: std::process::id() });

        // A dead pid → Stale (pidfile present, process gone).
        write_pidfile(&path, 4_294_967_290).unwrap();
        assert_eq!(status_at(&path), Status::Stale { pid: 4_294_967_290 });
    }
}
