//! Supervisor — Mode A (owner).
//!
//! Spawns agent processes through a PTY (`portable-pty`; ConPTY on Windows),
//! feeds their streaming output into the agent's [`StreamParser`], persists the
//! resulting `LoopEvent`s, rolls them up onto the `Run`, and can stop them.
//! Stopping a process is the **worst action loopd ever takes** — it never edits
//! the user's codebase.
//!
//! **Design choices (Windows-first; dev machine is Windows):**
//! - *One merged stream.* A PTY is a single duplex channel: with ConPTY the
//!   child's stdout **and** stderr are merged, so there is no separate stderr to
//!   classify. The parser handles the merged stream leniently (JSON → events,
//!   everything else → `Output`), which captures stderr noise without dropping
//!   it. (This is the deliberate PTY trade-off vs. vibe-kanban's piped-stdio
//!   model that can split stderr into `Error` events — see ARCHITECTURE §6.)
//! - *Windows launch.* `claude` ships as an npm `.cmd` shim (no native `.exe`),
//!   which `CreateProcess` can't run directly, so on Windows we spawn
//!   `cmd.exe /c <program> …`. That also yields the `cmd → node → agent` process
//!   tree we must kill as a unit.
//! - *Tree kill.* Killing only the direct child orphans `node`/agent
//!   grandchildren, so we kill the whole tree by pid (`taskkill /T`, graceful →
//!   `/F` force; on unix the process group). A [`RunHandle`]'s `Drop` kills the
//!   tree if the run is still live (the `kill_on_drop` guarantee), so a dropped
//!   supervisor — or daemon shutdown — never leaks agents.
//! - *Long-line caveat.* ConPTY wraps output at the PTY width, which could split
//!   a very long JSON line and corrupt it. We set a wide PTY to make this rare;
//!   if it bites in practice, piped stdio is the proven fallback (flagged for the
//!   gap pass — ARCHITECTURE §6, do not pre-optimize).
//!
//! Pause/resume (ARCHITECTURE §4) is **not** a process suspend: pause = capture
//! the agent's `agent_session_id` + stop; resume = re-spawn via the agent's
//! native `--resume`. Those land with the route wiring (Phase 3.7).

use std::collections::HashMap;
use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};

use crate::agents::{Adapter, RunOpts, RunState};
use crate::core::events::{now_ms, RunStatus};
use crate::core::store::Store;

/// Tracks every owned (Mode-A) run so the daemon can stop/pause it. Cheap to
/// share (`Arc` in `AppState`); all per-run process state lives in [`RunHandle`].
#[derive(Default)]
pub struct SupervisorRegistry {
    runs: Mutex<HashMap<String, Arc<RunHandle>>>,
}

/// A control handle for one supervised process. The reader thread updates
/// `session_id`/`finished`; the registry sets `kill_requested`/`paused`. Kept in
/// the registry until the daemon stops; its `Drop` is the `kill_on_drop` guard.
pub struct RunHandle {
    /// The run this handle controls.
    pub run_id: String,
    /// OS pid of the spawned root (the `cmd.exe` wrapper on Windows). The whole
    /// tree under it is what we kill.
    pub pid: Option<u32>,
    /// The agent's own session id, captured once the stream reveals it — this is
    /// what makes pause→resume possible.
    session_id: Mutex<Option<String>>,
    /// Set when a stop was requested via the registry (vs. a natural exit).
    kill_requested: AtomicBool,
    /// Set when the stop was a *pause* (resumable) rather than a hard kill.
    paused: AtomicBool,
    /// Set once the process has exited and final state is written.
    finished: AtomicBool,
}

impl RunHandle {
    /// The agent session id, if discovered (for pause/resume + Mode-B correlation).
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }
}

impl Drop for RunHandle {
    fn drop(&mut self) {
        // kill_on_drop: if the run is still live when its handle goes away (e.g.
        // daemon shutdown drops the registry), take down the whole tree so we
        // never leak a node/agent process.
        if !self.finished.load(Ordering::SeqCst) {
            if let Some(pid) = self.pid {
                kill_tree(pid);
            }
        }
    }
}

impl SupervisorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn a fresh run of `task` with `adapter`, owning the process. The run
    /// row must already exist in the store (the handler inserts it); this fills
    /// in `pid` and drives it to a terminal state on a background thread.
    pub fn spawn(
        &self,
        adapter: &dyn Adapter,
        run_id: &str,
        task: &str,
        cwd: &str,
        opts: &RunOpts,
        store: Arc<Mutex<Store>>,
    ) -> Result<Arc<RunHandle>> {
        let args = adapter.build_args(task, opts);
        self.spawn_raw(adapter, run_id, &args, cwd, store)
    }

    /// Re-spawn `run_id` to resume the agent session `session_id`, continuing
    /// `task`. Used by pause→resume (the parser drops the replayed history).
    pub fn resume(
        &self,
        adapter: &dyn Adapter,
        run_id: &str,
        session_id: &str,
        task: &str,
        cwd: &str,
        opts: &RunOpts,
        store: Arc<Mutex<Store>>,
    ) -> Result<Arc<RunHandle>> {
        let args = adapter.resume_args(session_id, task, opts);
        self.spawn_raw(adapter, run_id, &args, cwd, store)
    }

    /// Lower-level spawn used by both `spawn` and `resume` (and tests, which pass
    /// a trivial program). Builds the PTY, launches the process, and starts the
    /// reader thread that streams output → parser → store.
    pub fn spawn_raw(
        &self,
        adapter: &dyn Adapter,
        run_id: &str,
        args: &[String],
        cwd: &str,
        store: Arc<Mutex<Store>>,
    ) -> Result<Arc<RunHandle>> {
        let pty = native_pty_system();
        // Wide PTY: minimize ConPTY wrapping splitting long JSON lines.
        let pair = pty
            .openpty(PtySize {
                rows: 64,
                cols: 4096,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;

        let mut cmd = build_command(adapter.program(), args);
        if !cwd.is_empty() {
            cmd.cwd(cwd);
        }
        for (k, v) in adapter.env() {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning agent `{}`", adapter.program()))?;
        let pid = child.process_id();
        // Drop the slave so the PTY signals EOF once the child exits.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().context("cloning pty reader")?;
        let parser = adapter.new_parser(run_id);

        let handle = Arc::new(RunHandle {
            run_id: run_id.to_string(),
            pid,
            session_id: Mutex::new(None),
            kill_requested: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        self.runs
            .lock()
            .expect("registry mutex")
            .insert(run_id.to_string(), handle.clone());

        // Record the pid immediately so `ps`/`dash` show it before any output.
        if let Some(pid) = pid {
            if let Ok(store) = store.lock() {
                if let Ok(Some(mut run)) = store.get_run(run_id) {
                    run.pid = Some(pid);
                    run.updated_at = now_ms();
                    let _ = store.upsert_run(&run);
                }
            }
        }

        // ConPTY (and any PTY) does not EOF the master read when the child exits
        // — a terminal stays "open" after its shell quits. So we split the work:
        //   - a *waiter* thread owns the child + master: it waits for the child to
        //     exit, then drops the master to tear down the PTY, which unblocks the
        //     reader, and forwards the exit code;
        //   - a *reader* thread streams output → parser → store until that EOF,
        //     then writes the terminal state with the exit code from the waiter.
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<Option<i32>>();
        let master = pair.master;
        std::thread::Builder::new()
            .name(format!("supervisor-wait:{run_id}"))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                // Tearing down the master closes the PTY pipe → reader gets EOF.
                drop(master);
                let _ = exit_tx.send(code);
            })
            .context("spawning supervisor waiter thread")?;

        let thread_handle = handle.clone();
        let run_id_owned = run_id.to_string();
        std::thread::Builder::new()
            .name(format!("supervisor:{run_id}"))
            .spawn(move || {
                reader_loop(
                    run_id_owned,
                    reader,
                    parser,
                    store,
                    thread_handle,
                    exit_rx,
                );
            })
            .context("spawning supervisor reader thread")?;

        Ok(handle)
    }

    /// Request a hard stop of `run_id` (the worst action loopd takes). Returns
    /// `false` if the run isn't owned/known here. The reader thread observes the
    /// flag and marks the run `Killed`.
    pub fn kill(&self, run_id: &str) -> bool {
        match self.handle(run_id) {
            Some(h) => {
                h.kill_requested.store(true, Ordering::SeqCst);
                if let Some(pid) = h.pid {
                    kill_tree(pid);
                }
                true
            }
            None => false,
        }
    }

    /// Pause `run_id`: capture the agent session id (already streamed) and stop
    /// the process. Resume re-spawns via [`resume`]. Returns the captured session
    /// id, or `None` if the run is unknown / never revealed one.
    pub fn pause(&self, run_id: &str) -> Option<String> {
        let h = self.handle(run_id)?;
        h.paused.store(true, Ordering::SeqCst);
        let sid = h.session_id();
        if let Some(pid) = h.pid {
            kill_tree(pid);
        }
        sid
    }

    /// Is this run owned (and currently tracked) here?
    pub fn owns(&self, run_id: &str) -> bool {
        self.runs
            .lock()
            .map(|m| m.contains_key(run_id))
            .unwrap_or(false)
    }

    fn handle(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.runs.lock().ok()?.get(run_id).cloned()
    }
}

/// The body of the per-run reader thread: stream PTY output into the parser,
/// persist events, roll them up onto the `Run`, then write the terminal status.
/// The exit code arrives from the waiter thread once the child has exited.
fn reader_loop(
    run_id: String,
    mut reader: Box<dyn Read + Send>,
    mut parser: Box<dyn crate::agents::StreamParser>,
    store: Arc<Mutex<Store>>,
    handle: Arc<RunHandle>,
    exit_rx: std::sync::mpsc::Receiver<Option<i32>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF — the waiter dropped the master after child exit.
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                let events = parser.push(&chunk);
                persist(&store, &run_id, &handle, &parser.run_state(), &events);
            }
            Err(_) => break, // read error (pty torn down) — end of stream.
        }
    }

    // Flush the tail and synthesize a RunEnd if the stream cut off early.
    let tail = parser.finish();
    let state = parser.run_state();
    persist(&store, &run_id, &handle, &state, &tail);

    // The waiter sends the exit code once `child.wait()` returns.
    let exit_code = exit_rx.recv().ok().flatten();
    finalize(&store, &run_id, &handle, &state, exit_code);
}

/// Insert this batch of events and merge the rolled-up `RunState` onto the row.
fn persist(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    handle: &RunHandle,
    state: &RunState,
    events: &[crate::core::events::LoopEvent],
) {
    // Capture the agent session id the first time the stream reveals it.
    if let Some(sid) = &state.session_id {
        if let Ok(mut g) = handle.session_id.lock() {
            if g.is_none() {
                *g = Some(sid.clone());
            }
        }
    }

    let store = match store.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    for ev in events {
        let _ = store.insert_event(ev);
    }
    if let Ok(Some(mut run)) = store.get_run(run_id) {
        apply_state(&mut run, state);
        let now = now_ms();
        if !events.is_empty() {
            run.last_event_at = now;
        }
        run.updated_at = now;
        let _ = store.upsert_run(&run);
    }
}

/// Merge the parser's rollup onto the run (live metrics; status is set later).
fn apply_state(run: &mut crate::core::events::Run, state: &RunState) {
    if let Some(model) = &state.model {
        run.model = Some(model.clone());
    }
    if let Some(sid) = &state.session_id {
        run.agent_session_id = Some(sid.clone());
    }
    run.iteration = state.iteration;
    run.tokens_in = state.tokens_in;
    run.tokens_out = state.tokens_out;
    run.context_tokens = state.context_tokens;
    if let Some(cost) = state.cost_usd {
        run.cost_usd = cost;
    }
    if let Some(cw) = state.context_window {
        run.context_window = cw;
    }
}

/// Write the terminal status once the process has exited.
fn finalize(
    store: &Arc<Mutex<Store>>,
    run_id: &str,
    handle: &RunHandle,
    state: &RunState,
    exit_code: Option<i32>,
) {
    let status = if handle.paused.load(Ordering::SeqCst) {
        RunStatus::Paused
    } else if handle.kill_requested.load(Ordering::SeqCst) {
        RunStatus::Killed
    } else if state.exit_ok == Some(false) || exit_code.map(|c| c != 0).unwrap_or(false) {
        RunStatus::Failed
    } else {
        RunStatus::Done
    };

    if let Ok(store) = store.lock() {
        if let Ok(Some(mut run)) = store.get_run(run_id) {
            apply_state(&mut run, state);
            run.status = status;
            run.exit_code = exit_code;
            // A paused run is resumable, not ended.
            if status != RunStatus::Paused {
                run.ended_at = Some(now_ms());
            }
            run.updated_at = now_ms();
            let _ = store.upsert_run(&run);
        }
    }
    // Mark finished *after* writing state so Drop won't re-kill an exited tree.
    handle.finished.store(true, Ordering::SeqCst);
}

/// Build the PTY command. On Windows, route through `cmd.exe /c` so npm `.cmd`
/// shims (`claude.cmd`) resolve via PATHEXT and we get a killable process tree.
fn build_command(program: &str, args: &[String]) -> CommandBuilder {
    #[cfg(windows)]
    {
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.arg("/c");
        cmd.arg(program);
        for a in args {
            cmd.arg(a);
        }
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = CommandBuilder::new(program);
        for a in args {
            cmd.arg(a);
        }
        cmd
    }
}

/// Kill the whole process tree rooted at `pid`, graceful then forced. Best
/// effort — a process that's already gone is success.
#[cfg(windows)]
fn kill_tree(pid: u32) {
    // `/T` includes the child tree (node, agent); try graceful, then force.
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .output();
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}

#[cfg(not(windows))]
fn kill_tree(pid: u32) {
    // portable-pty puts the child in its own session, so the negative pid
    // addresses the whole group. Graceful TERM, brief grace, then KILL.
    let group = format!("-{pid}");
    let _ = Command::new("kill").args(["-TERM", &group]).status();
    std::thread::sleep(Duration::from_millis(300));
    let _ = Command::new("kill").args(["-KILL", &group]).status();
    // Fallback for the bare pid in case it isn't a group leader.
    let _ = Command::new("kill").args(["-KILL", &pid.to_string()]).status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::claude::ClaudeAdapter;
    use crate::core::events::{new_run_id, Run};
    use std::time::Instant;

    fn test_store() -> Arc<Mutex<Store>> {
        let dir = std::env::temp_dir().join(format!("loopd_sup_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Mutex::new(Store::open(dir.join("test.db")).unwrap()))
    }

    fn seed_run(store: &Arc<Mutex<Store>>, run_id: &str) {
        let store = store.lock().unwrap();
        let mut run = Run::new(run_id);
        run.agent = "claude".to_string();
        run.owned = true;
        store.upsert_run(&run).unwrap();
    }

    fn wait_terminal(store: &Arc<Mutex<Store>>, run_id: &str, within: Duration) -> Option<Run> {
        let deadline = Instant::now() + within;
        loop {
            let run = store.lock().unwrap().get_run(run_id).unwrap();
            if let Some(ref r) = run {
                if r.status != RunStatus::Running {
                    return run;
                }
            }
            if Instant::now() >= deadline {
                return run;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn captures_output_and_reaches_terminal_state() {
        let store = test_store();
        let run_id = new_run_id();
        seed_run(&store, &run_id);
        let reg = SupervisorRegistry::new();

        // Spawn a plain `echo` (non-JSON → Output events + a synthesized RunEnd).
        // build_command wraps in `cmd.exe /c echo …` on Windows; on unix it's
        // `sh -c "echo …"`. Either way EchoAdapter::program supplies the binary.
        let args: Vec<String> = if cfg!(windows) {
            vec!["hello-from-loopd".into()]
        } else {
            vec!["-c".into(), "echo hello-from-loopd".into()]
        };
        reg.spawn_raw(&EchoAdapter, &run_id, &args, "", store.clone())
            .expect("spawn echo");

        let run = wait_terminal(&store, &run_id, Duration::from_secs(15))
            .expect("run should exist");
        assert_eq!(run.status, RunStatus::Done, "echo run must finish cleanly");

        let events = store.lock().unwrap().events_for_run(&run_id, 100).unwrap();
        assert!(!events.is_empty(), "echo output must be captured as events");
    }

    #[test]
    fn kill_stops_a_long_running_process() {
        let store = test_store();
        let run_id = new_run_id();
        seed_run(&store, &run_id);
        let reg = SupervisorRegistry::new();

        // A long sleeper so we can kill it mid-flight.
        let args: Vec<String> = if cfg!(windows) {
            // build_command → `cmd.exe /c ping -n 30 127.0.0.1` (~30s).
            vec!["-n".into(), "30".into(), "127.0.0.1".into()]
        } else {
            vec!["-c".into(), "sleep 30".into()]
        };
        let prog = SleeperAdapter;
        let handle = reg
            .spawn_raw(&prog, &run_id, &args, "", store.clone())
            .expect("spawn sleeper");
        assert!(handle.pid.is_some(), "must have a pid to kill");

        // Give it a moment to actually start, then kill the tree.
        std::thread::sleep(Duration::from_millis(500));
        assert!(reg.kill(&run_id), "kill should find the owned run");

        let run = wait_terminal(&store, &run_id, Duration::from_secs(15))
            .expect("run should exist");
        assert_eq!(run.status, RunStatus::Killed, "killed run must be marked Killed");
    }

    // Minimal adapters whose `program` is the test command; they reuse the
    // Claude parser (non-JSON output just becomes Output events).
    struct EchoAdapter;
    impl Adapter for EchoAdapter {
        fn id(&self) -> &str {
            "echo"
        }
        fn program(&self) -> &str {
            if cfg!(windows) {
                "echo"
            } else {
                "sh"
            }
        }
        fn build_args(&self, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn resume_args(&self, _s: &str, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn new_parser(&self, run_id: &str) -> Box<dyn crate::agents::StreamParser> {
            ClaudeAdapter::new().new_parser(run_id)
        }
    }

    struct SleeperAdapter;
    impl Adapter for SleeperAdapter {
        fn id(&self) -> &str {
            "sleeper"
        }
        fn program(&self) -> &str {
            if cfg!(windows) {
                "ping"
            } else {
                "sh"
            }
        }
        fn build_args(&self, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn resume_args(&self, _s: &str, _t: &str, _o: &RunOpts) -> Vec<String> {
            Vec::new()
        }
        fn new_parser(&self, run_id: &str) -> Box<dyn crate::agents::StreamParser> {
            ClaudeAdapter::new().new_parser(run_id)
        }
    }
}
