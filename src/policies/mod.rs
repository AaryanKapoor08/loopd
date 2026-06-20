//! Policies — the extensible seam for governance detectors.
//!
//! A [`Policy`] is one named, pure check: given a run and its recent events, it
//! returns `Some(flag)` if the run trips it, else `None`. The daemon's detector
//! (`core::detector`) runs the whole [`builtin_policies`] registry on each tick
//! and unions the flags. New detectors are added here as one more `impl Policy`
//! — the registry is the "plugin" seam (modeled loosely on portkey's
//! `plugins/<name>/{manifest,handler}`, but kept to an in-process trait+registry
//! for v1; a real manifest-loaded engine is a later concern, ARCHITECTURE §7).
//!
//! **Tiering (ARCHITECTURE §7).** Everything here is the **v1 deterministic**
//! tier: caps (iterations / cost / duration), repeated-action, error-streak, and
//! context-exhaustion. Each is a pure function of data the Phase-3 parser already
//! produced (`tool_input_hash`, `tool_status`, token totals) — **no
//! agent-specific logic**. The **best-effort** no-progress detector is stateful
//! (it needs the cross-tick git fingerprint) so it lives in `core::detector`, not
//! here. Oscillation (v1.5) is deferred.

use crate::config::{Caps, Runaway};
use crate::core::events::{EventKind, LoopEvent, Run, ToolStatus};

/// Everything a [`Policy`] needs to decide, gathered once per run per tick. The
/// caps/runaway thresholds are already **resolved** (per-run override else config
/// default) by the caller, so a policy never reaches back into config.
pub struct DetectCtx<'a> {
    /// The run under evaluation (live: `Running`/`Stuck`).
    pub run: &'a Run,
    /// Recent events for this run, **chronological** (oldest first), windowed.
    pub recent: &'a [LoopEvent],
    /// Effective cap thresholds for this run.
    pub caps: Caps,
    /// Effective runaway thresholds for this run.
    pub runaway: Runaway,
    /// "Now" in epoch ms (the tick stamps one value for the whole pass).
    pub now_ms: i64,
    /// Context-window fraction (0.0–1.0) at/above which to flag exhaustion.
    pub context_pct: f64,
}

impl DetectCtx<'_> {
    /// Wall-clock minutes this run has been alive (to `ended_at` if it has one).
    fn elapsed_min(&self) -> u32 {
        let end = self.run.ended_at.unwrap_or(self.now_ms);
        (end.saturating_sub(self.run.started_at) / 60_000).max(0) as u32
    }
}

/// One governance detector. Pure: same inputs → same flag. The `id` is the flag
/// string persisted onto `Run.flags` and shown in `ps`/`dash`.
pub trait Policy: Send + Sync {
    /// The flag this policy raises (also its stable id).
    fn id(&self) -> &'static str;
    /// Returns `Some(self.id())` when the run trips this policy, else `None`.
    fn check(&self, ctx: &DetectCtx) -> Option<String>;
}

/// The v1 deterministic registry, in display order. The detector runs all of
/// these every tick.
pub fn builtin_policies() -> Vec<Box<dyn Policy>> {
    vec![
        Box::new(CapIterations),
        Box::new(CapCost),
        Box::new(CapDuration),
        Box::new(RepeatedAction),
        Box::new(ErrorStreak),
        Box::new(ContextExhaustion),
    ]
}

// --- caps --------------------------------------------------------------------

/// `cap-iterations` — the run reached its iteration cap.
pub struct CapIterations;
impl Policy for CapIterations {
    fn id(&self) -> &'static str {
        "cap-iterations"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        (ctx.caps.max_iterations > 0 && ctx.run.iteration >= ctx.caps.max_iterations)
            .then(|| self.id().to_string())
    }
}

/// `cap-cost` — cumulative cost reached the cap.
pub struct CapCost;
impl Policy for CapCost {
    fn id(&self) -> &'static str {
        "cap-cost"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        (ctx.caps.max_cost_usd > 0.0 && ctx.run.cost_usd >= ctx.caps.max_cost_usd)
            .then(|| self.id().to_string())
    }
}

/// `cap-duration` — wall-clock runtime reached the cap.
pub struct CapDuration;
impl Policy for CapDuration {
    fn id(&self) -> &'static str {
        "cap-duration"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        (ctx.caps.max_duration_min > 0 && ctx.elapsed_min() >= ctx.caps.max_duration_min)
            .then(|| self.id().to_string())
    }
}

// --- runaway -----------------------------------------------------------------

/// `repeated-action` — the same tool with the same input fired `repeated_action`
/// times in a row (a classic stuck loop: re-running an identical command). Reads
/// the trailing streak of `ToolUse` events by `(tool, tool_input_hash)`.
pub struct RepeatedAction;
impl Policy for RepeatedAction {
    fn id(&self) -> &'static str {
        "repeated-action"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        let threshold = ctx.runaway.repeated_action;
        if threshold == 0 {
            return None;
        }
        // The identity of each tool call, newest first; only calls that carry a
        // stable input hash can be compared.
        let calls: Vec<(&str, u64)> = ctx
            .recent
            .iter()
            .rev()
            .filter(|e| e.kind == EventKind::ToolUse)
            .filter_map(|e| Some((e.tool.as_deref()?, e.tool_input_hash?)))
            .collect();
        let Some(head) = calls.first() else {
            return None;
        };
        let streak = calls.iter().take_while(|c| *c == head).count() as u32;
        (streak >= threshold).then(|| self.id().to_string())
    }
}

/// `error-streak` — `error_streak` tool calls failed in a row with no success in
/// between (the agent is flailing). Reads the trailing run of `ToolResult`
/// outcomes; a single `Ok` resets it.
pub struct ErrorStreak;
impl Policy for ErrorStreak {
    fn id(&self) -> &'static str {
        "error-streak"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        let threshold = ctx.runaway.error_streak;
        if threshold == 0 {
            return None;
        }
        let outcomes: Vec<ToolStatus> = ctx
            .recent
            .iter()
            .rev()
            .filter(|e| e.kind == EventKind::ToolResult)
            .filter_map(|e| e.tool_status)
            .collect();
        let streak = outcomes
            .iter()
            .take_while(|s| **s == ToolStatus::Error)
            .count() as u32;
        (streak >= threshold).then(|| self.id().to_string())
    }
}

// --- context -----------------------------------------------------------------

/// `context-exhaustion` — the run is filling the model's context window (≥
/// `context_pct`). Cheap: the parser already tracks `context_tokens` /
/// `context_window`. A leading indicator that a loop is about to derail.
pub struct ContextExhaustion;
impl Policy for ContextExhaustion {
    fn id(&self) -> &'static str {
        "context-exhaustion"
    }
    fn check(&self, ctx: &DetectCtx) -> Option<String> {
        let window = ctx.run.context_window;
        if window == 0 {
            return None;
        }
        let frac = ctx.run.context_tokens as f64 / window as f64;
        (frac >= ctx.context_pct).then(|| self.id().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{new_run_id, now_ms, Run, Source};

    /// Default context-exhaustion fraction used by these tests + the detector.
    const CTX_PCT: f64 = 0.90;

    fn ctx<'a>(run: &'a Run, recent: &'a [LoopEvent], caps: Caps, runaway: Runaway) -> DetectCtx<'a> {
        DetectCtx {
            run,
            recent,
            caps,
            runaway,
            now_ms: now_ms(),
            context_pct: CTX_PCT,
        }
    }

    fn tool_use(tool: &str, hash: u64) -> LoopEvent {
        let mut e = LoopEvent::new("r", Source::Supervisor, EventKind::ToolUse);
        e.tool = Some(tool.to_string());
        e.tool_input_hash = Some(hash);
        e
    }

    fn tool_result(status: ToolStatus) -> LoopEvent {
        let mut e = LoopEvent::new("r", Source::Supervisor, EventKind::ToolResult);
        e.tool_status = Some(status);
        e
    }

    #[test]
    fn caps_trip_at_their_thresholds() {
        let mut run = Run::new(new_run_id());
        run.iteration = 50;
        run.cost_usd = 2.0;
        run.started_at = now_ms() - 31 * 60_000; // 31 minutes ago
        let c = ctx(&run, &[], Caps::default(), Runaway::default());
        assert_eq!(CapIterations.check(&c).as_deref(), Some("cap-iterations"));
        assert_eq!(CapCost.check(&c).as_deref(), Some("cap-cost"));
        assert_eq!(CapDuration.check(&c).as_deref(), Some("cap-duration"));
    }

    #[test]
    fn caps_are_quiet_below_threshold() {
        let mut run = Run::new(new_run_id());
        run.iteration = 10;
        run.cost_usd = 0.5;
        run.started_at = now_ms() - 60_000; // 1 minute
        let c = ctx(&run, &[], Caps::default(), Runaway::default());
        assert!(CapIterations.check(&c).is_none());
        assert!(CapCost.check(&c).is_none());
        assert!(CapDuration.check(&c).is_none());
    }

    #[test]
    fn repeated_action_needs_a_consecutive_streak() {
        let run = Run::new(new_run_id());
        // Three identical bash calls in a row → trip (default threshold 3).
        let three = vec![
            tool_use("bash", 7),
            tool_use("bash", 7),
            tool_use("bash", 7),
        ];
        assert_eq!(
            RepeatedAction
                .check(&ctx(&run, &three, Caps::default(), Runaway::default()))
                .as_deref(),
            Some("repeated-action")
        );
        // A different input breaks the streak.
        let broken = vec![
            tool_use("bash", 7),
            tool_use("bash", 8),
            tool_use("bash", 7),
        ];
        assert!(RepeatedAction
            .check(&ctx(&run, &broken, Caps::default(), Runaway::default()))
            .is_none());
    }

    #[test]
    fn error_streak_resets_on_a_success() {
        let run = Run::new(new_run_id());
        let four = vec![
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
            tool_result(ToolStatus::Error),
        ];
        assert_eq!(
            ErrorStreak
                .check(&ctx(&run, &four, Caps::default(), Runaway::default()))
                .as_deref(),
            Some("error-streak")
        );
        // An Ok at the tail resets the trailing streak to zero.
        let mut reset = four.clone();
        reset.push(tool_result(ToolStatus::Ok));
        assert!(ErrorStreak
            .check(&ctx(&run, &reset, Caps::default(), Runaway::default()))
            .is_none());
    }

    #[test]
    fn context_exhaustion_trips_at_ninety_percent() {
        let mut run = Run::new(new_run_id());
        run.context_window = 200_000;
        run.context_tokens = 185_000; // 92.5%
        assert_eq!(
            ContextExhaustion
                .check(&ctx(&run, &[], Caps::default(), Runaway::default()))
                .as_deref(),
            Some("context-exhaustion")
        );
        run.context_tokens = 100_000; // 50%
        assert!(ContextExhaustion
            .check(&ctx(&run, &[], Caps::default(), Runaway::default()))
            .is_none());
    }
}
