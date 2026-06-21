/**
 * @loopd/sdk — Surface 2 of loopd.
 *
 * loopd governs CLI agents (Claude Code, Codex) by spawning or observing them.
 * This SDK extends that reach to *programmatic* loops: a plain Anthropic-SDK
 * loop, an API agent, or (v1.5) a LangGraph graph. You wrap your loop with
 * {@link track} and report iterations / tool calls / cost; loopd then shows it
 * in the same cockpit and — crucially — can *enforce* governance on it.
 *
 * The enforcement channel is the daemon's response to every report and to
 * {@link TrackedRun.check}: it carries a {@link Verdict}, and `check()` throws
 * when that verdict is `pause` or `kill`. loopd never owns a programmatic loop's
 * process, so the worst it can do is *return* `kill` — the SDK obeys it. That
 * return channel is what makes the SDK a governor, not just a telemetry emitter
 * (ARCHITECTURE §4).
 *
 * **Fail-open by design.** loopd is a safety net, not a hard dependency. If the
 * daemon is unreachable, registration and reports degrade to no-ops and `check()`
 * never throws — your loop keeps running. A governor that is down must never be
 * the thing that breaks your work.
 *
 * Wire types (`Verdict`, `EventKind`, ...) are NOT hand-written here — they are
 * generated from the Rust core into `./types` via `ts-rs`. We deliberately keep
 * the SDK's request/response surface free of the `bigint` fields (`ts`,
 * `toolInputHash`, timestamps) that ride on the full `LoopEvent`/`Run`: the
 * daemon builds those, so the SDK only ever sends/receives plain JSON numbers.
 */

import type { EventKind } from "./types/EventKind.js";
import type { IngestResponse } from "./types/IngestResponse.js";
import type { OnTrip } from "./types/OnTrip.js";
import type { ToolStatus } from "./types/ToolStatus.js";
import type { Verdict } from "./types/Verdict.js";

export type { EventKind, OnTrip, ToolStatus, Verdict };

/** Default daemon port (mirrors the Rust `config.daemon.port` default). */
const DEFAULT_PORT = 7777;

/** Resolve the daemon base URL: `$LOOPD_URL`, else `127.0.0.1:$LOOPD_PORT|7777`. */
function defaultDaemonUrl(): string {
  const url = process.env.LOOPD_URL;
  if (url && url.length > 0) return url.replace(/\/+$/, "");
  const port = process.env.LOOPD_PORT ?? String(DEFAULT_PORT);
  return `http://127.0.0.1:${port}`;
}

/** Options for {@link track}. Caps map onto the same governance the CLI uses. */
export interface TrackOptions {
  /** Working directory the loop runs in (for the no-progress signal). */
  cwd?: string;
  /** Display agent/vendor shown in the cockpit (e.g. `"anthropic"`). Default `"sdk"`. */
  agent?: string;
  /** Model id the loop drives, if known (display + future pricing). */
  model?: string;
  /** Trip when cumulative cost exceeds this many USD. */
  maxCostUsd?: number;
  /** Trip after this many iterations. */
  maxIterations?: number;
  /** Trip after this many wall-clock minutes. */
  maxDurationMin?: number;
  /**
   * What loopd should do when a cap/detector trips. `kill`/`pause` are enforced
   * via {@link TrackedRun.check} (the SDK obeys the returned verdict). Defaults
   * to the daemon's config default (`warn` — flag only).
   */
  onTrip?: OnTrip;
  /** Daemon base URL. Defaults to `$LOOPD_URL` or `http://127.0.0.1:7777`. */
  daemonUrl?: string;
}

/** A single report's payload — what one loop step just did / incurred. */
export interface ReportEvent {
  /** The kind of event (defaults to `output`). */
  kind?: EventKind;
  /** Tool name, for a tool call. */
  tool?: string;
  /** Tool outcome, for a tool result. */
  toolStatus?: ToolStatus;
  /** Free-text payload (assistant text, error message, …). */
  text?: string;
  /** Iterations to add to the run's turn count. */
  iterations?: number;
  /** Input tokens to add to the cumulative total. */
  tokensIn?: number;
  /** Output tokens to add to the cumulative total. */
  tokensOut?: number;
  /** Cost (USD) to add to the cumulative total — the cost cap reads the sum. */
  costUsd?: number;
}

/** A handle to a run registered with loopd. Each report returns the current {@link Verdict}. */
export interface TrackedRun {
  /** Opaque run id assigned by the daemon (empty string if registration failed). */
  readonly runId: string;
  /** Mark the start of a new loop iteration. */
  iteration(): Promise<Verdict>;
  /** Report a tool/function call the loop made. */
  toolUse(tool: string, status?: ToolStatus): Promise<Verdict>;
  /** Report cost (USD), and optionally token counts, the loop just incurred. */
  cost(usd: number, tokens?: { in?: number; out?: number }): Promise<Verdict>;
  /** Report an arbitrary event (the general form behind the helpers above). */
  event(e: ReportEvent): Promise<Verdict>;
  /**
   * Read the latest verdict and enforce it: throws {@link LoopdHaltError} if the
   * run has been told to `pause` or `kill`. Call this at the top of each loop turn.
   */
  check(): Promise<void>;
}

/**
 * Thrown by {@link TrackedRun.check} when loopd's verdict is `pause` or `kill`.
 * This is the SDK's half of the enforcement contract: loopd decides, the loop
 * obeys by unwinding.
 */
export class LoopdHaltError extends Error {
  constructor(
    readonly verdict: Verdict,
    readonly runId: string,
  ) {
    super(`loopd: run ${runId} was told to ${verdict}`);
    this.name = "LoopdHaltError";
  }
}

/** Concrete {@link TrackedRun}. `enabled === false` is the detached, fail-open mode. */
class DaemonTrackedRun implements TrackedRun {
  constructor(
    readonly runId: string,
    private readonly daemonUrl: string,
    private readonly enabled: boolean,
  ) {}

  iteration(): Promise<Verdict> {
    return this.event({ kind: "assistant", iterations: 1 });
  }

  toolUse(tool: string, status?: ToolStatus): Promise<Verdict> {
    return this.event({ kind: "tool_use", tool, toolStatus: status });
  }

  cost(usd: number, tokens?: { in?: number; out?: number }): Promise<Verdict> {
    return this.event({
      kind: "token_usage",
      costUsd: usd,
      tokensIn: tokens?.in,
      tokensOut: tokens?.out,
    });
  }

  async event(e: ReportEvent): Promise<Verdict> {
    if (!this.enabled) return "ok";
    const resp = await post(this.daemonUrl, "/sdk/report", { runId: this.runId, ...e });
    return resp?.verdict ?? "ok";
  }

  async check(): Promise<void> {
    if (!this.enabled) return;
    const resp = await get(this.daemonUrl, `/sdk/runs/${this.runId}`);
    const verdict = resp?.verdict ?? "ok";
    if (verdict === "pause" || verdict === "kill") {
      throw new LoopdHaltError(verdict, this.runId);
    }
  }
}

/**
 * Register a programmatic loop with loopd and get a handle to report against.
 * Resolves even if the daemon is unreachable — in that case the returned handle
 * is detached (its reports are no-ops and `check()` never throws), so wrapping a
 * loop with loopd can never be the reason the loop fails.
 */
export async function track(label: string, opts: TrackOptions = {}): Promise<TrackedRun> {
  const daemonUrl = opts.daemonUrl ?? defaultDaemonUrl();
  const resp = await post(daemonUrl, "/sdk/track", {
    label,
    agent: opts.agent,
    cwd: opts.cwd,
    model: opts.model,
    maxCostUsd: opts.maxCostUsd,
    maxIterations: opts.maxIterations,
    maxDurationMin: opts.maxDurationMin,
    onTrip: opts.onTrip,
  });
  const runId = resp?.runId ?? null;
  if (!runId) {
    warnOnce(`loopd daemon unreachable at ${daemonUrl}; running un-governed`);
    return new DaemonTrackedRun("", daemonUrl, false);
  }
  return new DaemonTrackedRun(runId, daemonUrl, true);
}

// --- transport (fail-open) ---------------------------------------------------

/** POST JSON and parse the {@link IngestResponse}; `null` on any network error. */
async function post(
  daemonUrl: string,
  path: string,
  body: unknown,
): Promise<IngestResponse | null> {
  try {
    const res = await fetch(`${daemonUrl}${path}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) return null;
    return (await res.json()) as IngestResponse;
  } catch {
    return null;
  }
}

/** GET and parse the {@link IngestResponse}; `null` on any network error. */
async function get(daemonUrl: string, path: string): Promise<IngestResponse | null> {
  try {
    const res = await fetch(`${daemonUrl}${path}`);
    if (!res.ok) return null;
    return (await res.json()) as IngestResponse;
  } catch {
    return null;
  }
}

let warned = false;
/** Emit a degraded-mode warning at most once per process. */
function warnOnce(message: string): void {
  if (warned) return;
  warned = true;
  console.warn(`[loopd] ${message}`);
}
