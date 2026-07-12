//! End-to-end daemon lifecycle: `daemon start` → `/health` ok → `daemon stop`
//! → pidfile gone → second `stop` is a no-op. This is the Phase-2 gate.
//!
//! Isolated with `LOOPD_DIR` (a temp dir) + a custom port in `config.yaml`, so
//! it never touches the developer's real `~/.loopd` or a daemon on 7777. The
//! detached daemon inherits `LOOPD_DIR` from the `start` process's environment.
//!
//! The round-trip test runs the child commands with **null stdio**; the
//! pipe-capture test (Part F regression) runs `daemon start` with its output
//! captured through a pipe and asserts it still returns promptly — which only
//! holds because `spawn_detached` no longer lets the daemon inherit the caller's
//! std handles (it used to, so a captured pipe stayed open until the daemon
//! exited and blocked the caller).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Path to the built `loopd` binary (Cargo sets this for integration tests).
const BIN: &str = env!("CARGO_BIN_EXE_loopd");

/// A port unlikely to collide with anything on a dev box.
const TEST_PORT: u16 = 47_811;

/// A distinct port for the pipe-capture test (tests in a binary run in parallel).
const TEST_PORT_PIPED: u16 = 47_813;

fn run(dir: &Path, args: &[&str]) -> ExitStatus {
    Command::new(BIN)
        .args(args)
        .env("LOOPD_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn loopd")
}

fn read_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("daemon.log")).unwrap_or_else(|_| "<no daemon.log>".into())
}

fn health_ok(port: u16, within: Duration) -> bool {
    let client = reqwest::blocking::Client::new();
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + within;
    loop {
        let ok = client
            .get(&url)
            .timeout(Duration::from_millis(500))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn daemon_start_health_stop_round_trip() {
    let dir = std::env::temp_dir().join(format!("loopd_it_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("daemon:\n  port: {TEST_PORT}\n"),
    )
    .unwrap();
    let pidfile = dir.join("daemon.pid");

    // Best-effort pre-clean in case a prior run left something behind.
    let _ = run(&dir, &["daemon", "stop"]);

    // start → exits 0 only after /health is green, and writes the pidfile.
    let st = run(&dir, &["daemon", "start"]);
    assert!(
        st.success(),
        "daemon start failed; log:\n{}",
        read_log(&dir)
    );
    assert!(pidfile.exists(), "pidfile must exist after start");
    assert!(
        health_ok(TEST_PORT, Duration::from_secs(10)),
        "daemon /health never became ok; log:\n{}",
        read_log(&dir)
    );

    // stop → exits 0 and removes the pidfile.
    let st = run(&dir, &["daemon", "stop"]);
    assert!(st.success(), "daemon stop failed");
    assert!(!pidfile.exists(), "pidfile must be gone after stop");

    // stop again → clean no-op (idempotent).
    let st = run(&dir, &["daemon", "stop"]);
    assert!(st.success(), "second daemon stop should be a no-op");

    let _ = std::fs::remove_dir_all(&dir);
}

/// PART F regression: `loop daemon start` must return promptly even when its
/// stdout/stderr are captured through a **pipe** (`Command::output()` reads both
/// to EOF). Before Part F the detached daemon inherited the caller's pipe handle,
/// so EOF never arrived and this blocked until the daemon exited. The fix clears
/// `HANDLE_FLAG_INHERIT` on the caller's std handles around the spawn.
///
/// The capture runs on a worker thread so a regression *fails* (timeout) instead
/// of hanging the whole test binary forever.
#[test]
fn daemon_start_returns_promptly_under_a_captured_pipe() {
    let dir = std::env::temp_dir().join(format!("loopd_it_pipe_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.yaml"),
        format!("daemon:\n  port: {TEST_PORT_PIPED}\n"),
    )
    .unwrap();
    let pidfile = dir.join("daemon.pid");

    // Best-effort pre-clean in case a prior run left a daemon behind.
    let _ = run(&dir, &["daemon", "stop"]);

    let (tx, rx) = mpsc::channel();
    let thread_dir: PathBuf = dir.clone();
    let start = Instant::now();
    std::thread::spawn(move || {
        // `output()` opens pipes for stdout+stderr and reads them to EOF — the
        // exact capture pattern that used to deadlock against the daemon.
        let out = Command::new(BIN)
            .args(["daemon", "start"])
            .env("LOOPD_DIR", &thread_dir)
            .stdin(Stdio::null())
            .output();
        let _ = tx.send(out);
    });

    let output = match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => panic!("spawning `daemon start` failed: {e}"),
        Err(_) => panic!(
            "`daemon start` under a captured pipe did not return within 8s — the daemon is \
             holding the caller's pipe open (Part F regression). log:\n{}",
            read_log(&dir)
        ),
    };
    let elapsed = start.elapsed();

    assert!(
        output.status.success(),
        "daemon start failed (stderr: {}); log:\n{}",
        String::from_utf8_lossy(&output.stderr),
        read_log(&dir)
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "daemon start under a captured pipe took {elapsed:?} (>5s) — pipe-inheritance regression"
    );
    assert!(pidfile.exists(), "pidfile must exist after start");
    assert!(
        health_ok(TEST_PORT_PIPED, Duration::from_secs(5)),
        "daemon /health never became ok; log:\n{}",
        read_log(&dir)
    );

    let _ = run(&dir, &["daemon", "stop"]);
    let _ = std::fs::remove_dir_all(&dir);
}
