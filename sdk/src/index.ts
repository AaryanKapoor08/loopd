/**
 * @loopd/sdk — Surface 2 of loopd.
 *
 * loopd governs CLI agents (Claude Code, Codex) by spawning or observing them.
 * This SDK extends that reach to *programmatic* loops: a plain Anthropic-SDK
 * loop, an API agent, or (v1.5) a LangGraph graph. You wrap your loop with
 * `track()` and report iterations / tool calls / cost; loopd then shows it in
 * the same cockpit and — crucially — can *enforce* governance on it.
 *
 * The enforcement channel is the return value of every report: the daemon's
 * `/ingest` endpoint replies with a {@link Verdict}, and {@link TrackedRun.check}
 * throws when that verdict is `pause` or `kill`. That return channel is what
 * makes the SDK a governor, not just a telemetry emitter.
 *
 * Status: Phase 0 skeleton. The wire types (`LoopEvent`, `Run`, ...) are NOT
 * hand-written here — they are generated from the Rust core into `./types` via
 * `ts-rs` (`cargo test export_bindings` runs before the SDK build). The bodies
 * below are stubbed until Phase 9; see `claude/BuildFlow.md`.
 */

/** The daemon's per-report decision. Mirrors the Rust `Verdict` enum. */
export type Verdict = "ok" | "warn" | "pause" | "kill";

/** Options for {@link track}. Caps map onto the same governance policy the CLI uses. */
export interface TrackOptions {
  /** Working directory the loop runs in (for the no-progress signal). */
  cwd?: string;
  /** Trip when cumulative cost exceeds this many USD. */
  maxCostUsd?: number;
  /** Trip after this many iterations. */
  maxIterations?: number;
}

/** A handle to a run registered with loopd. Each report returns the current {@link Verdict}. */
export interface TrackedRun {
  /** Opaque run id assigned by the daemon. */
  readonly runId: string;
  /** Mark the start of a new loop iteration. */
  iteration(): Promise<Verdict>;
  /** Report a tool/function call the loop made. */
  toolUse(tool: string): Promise<Verdict>;
  /** Report cost (USD) for token usage the loop just incurred. */
  cost(usd: number): Promise<Verdict>;
  /**
   * Read the latest verdict and enforce it: throws if the run has been told to
   * `pause` or `kill`. Call this at the top of each loop turn.
   */
  check(): Promise<void>;
}

/**
 * Register a programmatic loop with loopd and get a handle to report against.
 * Implemented in Phase 9.
 */
export function track(_label: string, _opts: TrackOptions = {}): TrackedRun {
  throw new Error("@loopd/sdk: track() is not implemented yet (Phase 9 skeleton).");
}
