//! End-to-end daemon lifecycle: `daemon start` → `/health` ok → `daemon stop`
//! → pidfile gone → second `stop` is a no-op. This is the Phase-2 gate.
//!
//! Isolated with `LOOPD_DIR` (a temp dir) + a custom port in `config.yaml`, so
//! it never touches the developer's real `~/.loopd` or a daemon on 7777. The
//! detached daemon inherits `LOOPD_DIR` from the `start` process's environment.
//!
//! The child commands run with **null stdio** (not piped): the detached daemon
//! inherits its parent's handles, so a captured pipe would stay open until the
//! daemon exits and block the test. Null stdio sidesteps that. Making
//! `loop daemon start` safe to capture programmatically (non-inheritable handles)
//! is the Phase-2 "Part F" robustness follow-up; it does not affect interactive
//! use (a console handle, unlike a pipe, never blocks on EOF).

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Path to the built `loopd` binary (Cargo sets this for integration tests).
const BIN: &str = env!("CARGO_BIN_EXE_loopd");

/// A port unlikely to collide with anything on a dev box.
const TEST_PORT: u16 = 47_811;

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
    assert!(st.success(), "daemon start failed; log:\n{}", read_log(&dir));
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
