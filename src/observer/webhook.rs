//! Webhook — `POST /ingest` for Claude Code hooks (Mode B).
//!
//! A user-started `claude` session fires hooks (PostToolUse/Stop/SessionStart);
//! the installed hook command (`loop ingest`) POSTs the hook's stdin payload
//! here. We normalize it, correlate it to the one observed run for that session
//! (creating it on first sight, `owned = false`), and return the run's current
//! [`Verdict`] — the enforcement return channel the Phase-9 SDK also uses
//! (ARCHITECTURE §4).
//!
//! **Precedence / dedup (ARCHITECTURE §4).** The transcript JSONL is the
//! *canonical* source of token/iteration rollup and per-tool events (Phase 7
//! `transcript.rs`). Hooks are the *low-latency liveness* feed: they make the run
//! appear the instant a session starts and keep `lastEventAt` fresh, but they do
//! **not** roll up tokens (a hook carries none) and do **not** emit per-tool
//! events (the transcript does). That separation is the dedup: a tool can never
//! be counted twice because only one feed ever counts it. The single hook-sourced
//! `LoopEvent` we emit is a one-time `RunStart` on first sight — which the
//! transcript never produces (it has no `system/init` line), so it can't collide.
//!
//! The exact payload fields below were captured live (CC 2.x, 2026-06-20; §8 Q7)
//! and are pinned in the tests.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agents::adapter_for;
use crate::core::events::{EventKind, LoopEvent, Source, Verdict};
use crate::core::store::Store;

use super::{observe_run, touch_run, verdict_for};

/// The response body of `POST /ingest`: the correlated run and the daemon's
/// current verdict on it. `runId` is `None` only when the payload carried no
/// session id to correlate. Exported to the SDK via `ts-rs`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct IngestResponse {
    /// The run this payload was attributed to (`None` if uncorrelatable).
    pub run_id: Option<String>,
    /// The daemon's verdict — `ok`/`warn` for observed runs.
    pub verdict: Verdict,
}

/// The subset of a Claude Code hook payload we use. Every event carries
/// `session_id`/`transcript_path`/`cwd`/`hook_event_name`; tool events add
/// `tool_name`/`tool_use_id`; SessionStart adds `source`. Unknown fields are
/// ignored (forward-compatible). Verified live (§8 Q7).
#[derive(Debug, Default, Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    /// SessionStart `source` (`startup`/`resume`/`clear`/…).
    #[serde(default)]
    pub source: Option<String>,
}

/// Normalize a CC hook payload, correlate it to the observed run, and return the
/// verdict. `store` is the daemon's single store (the only writer). Pure
/// read/observe — loopd never touches the user's process or codebase here.
pub fn ingest_hook(store: &Arc<Mutex<Store>>, payload: &serde_json::Value) -> IngestResponse {
    // Route the payload to an agent (only `claude` in v1) and pull the session id
    // — both via the adapter's `match_hook`, the multi-adapter routing seam.
    let session_id = adapter_for("claude").and_then(|a| a.match_hook(payload));
    let p: HookPayload = serde_json::from_value(payload.clone()).unwrap_or_default();
    let session_id = match session_id.or(p.session_id) {
        Some(s) => s,
        None => {
            return IngestResponse {
                run_id: None,
                verdict: Verdict::Ok,
            }
        }
    };

    let Some((run_id, created)) = observe_run(store, &session_id, p.cwd.as_deref(), None) else {
        return IngestResponse {
            run_id: None,
            verdict: Verdict::Ok,
        };
    };

    // First sight → one hook-sourced RunStart (transcripts never emit one, so no
    // collision). Otherwise just refresh liveness; tokens/tools are the
    // transcript's job.
    if created {
        if let Ok(store) = store.lock() {
            let ev = LoopEvent::new(run_id.clone(), Source::Hook, EventKind::RunStart);
            let _ = store.insert_event(&ev);
        }
    } else {
        touch_run(store, &run_id);
    }

    // Read back the current verdict for the return channel.
    let verdict = store
        .lock()
        .ok()
        .and_then(|s| s.get_run(&run_id).ok().flatten())
        .map(|r| verdict_for(&r))
        .unwrap_or(Verdict::Ok);

    IngestResponse {
        run_id: Some(run_id),
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, RunReason, RunStatus};

    fn test_store() -> Arc<Mutex<Store>> {
        let dir = std::env::temp_dir().join(format!("loopd_wh_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(Mutex::new(Store::open(dir.join("t.db")).unwrap()))
    }

    // Pinned from a live capture (CC 2.x, 2026-06-20).
    fn session_start() -> serde_json::Value {
        serde_json::json!({
            "session_id": "sess_obs_1",
            "transcript_path": "C:\\Users\\x\\.claude\\projects\\C--dev-loop\\sess_obs_1.jsonl",
            "cwd": "C:\\dev\\loop",
            "hook_event_name": "SessionStart",
            "source": "startup"
        })
    }
    fn post_tool_use() -> serde_json::Value {
        serde_json::json!({
            "session_id": "sess_obs_1",
            "transcript_path": "C:\\Users\\x\\.claude\\projects\\C--dev-loop\\sess_obs_1.jsonl",
            "cwd": "C:\\dev\\loop",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_response": {"stdout": "hi", "stderr": "", "interrupted": false},
            "tool_use_id": "toolu_abc"
        })
    }

    #[test]
    fn first_hook_creates_an_observed_read_only_run() {
        let store = test_store();
        let resp = ingest_hook(&store, &session_start());
        let run_id = resp.run_id.expect("a run id");
        assert_eq!(resp.verdict, Verdict::Ok);

        let run = store.lock().unwrap().get_run(&run_id).unwrap().unwrap();
        assert!(!run.owned, "observed runs are read-only");
        assert_eq!(run.run_reason, RunReason::Observed);
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.agent_session_id.as_deref(), Some("sess_obs_1"));
        assert!(run.label.contains("observed"));
        // The one hook-sourced event is a RunStart.
        let events = store.lock().unwrap().events_for_run(&run_id, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, Source::Hook);
        assert_eq!(events[0].kind, EventKind::RunStart);
    }

    #[test]
    fn later_hooks_correlate_to_the_same_run_without_new_events() {
        let store = test_store();
        let first = ingest_hook(&store, &session_start()).run_id.unwrap();
        // A later tool hook for the same session must NOT create a second run, and
        // must NOT emit a per-tool event (the transcript owns those — no dup).
        let second = ingest_hook(&store, &post_tool_use());
        assert_eq!(second.run_id.as_deref(), Some(first.as_str()));
        assert_eq!(store.lock().unwrap().list_runs().unwrap().len(), 1);
        let events = store.lock().unwrap().events_for_run(&first, 10).unwrap();
        assert_eq!(events.len(), 1, "only the first-sight RunStart; no tool dup");
    }

    #[test]
    fn flagged_run_yields_a_warn_verdict() {
        let store = test_store();
        let run_id = ingest_hook(&store, &session_start()).run_id.unwrap();
        {
            let s = store.lock().unwrap();
            let mut run = s.get_run(&run_id).unwrap().unwrap();
            run.flags = vec!["repeated-action".into()];
            s.upsert_run(&run).unwrap();
        }
        // The verdict channel reflects the governance flag (observed → warn, never
        // pause/kill).
        let resp = ingest_hook(&store, &post_tool_use());
        assert_eq!(resp.verdict, Verdict::Warn);
    }

    #[test]
    fn payload_without_a_session_id_is_a_harmless_noop() {
        let store = test_store();
        let resp = ingest_hook(&store, &serde_json::json!({"hook_event_name": "Stop"}));
        assert!(resp.run_id.is_none());
        assert_eq!(resp.verdict, Verdict::Ok);
        assert!(store.lock().unwrap().list_runs().unwrap().is_empty());
    }
}
