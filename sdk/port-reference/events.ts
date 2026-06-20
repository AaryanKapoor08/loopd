// The one normalized event model that BOTH input adapters converge on.
// Model A (supervisor) and Model B (observer) both produce LoopEvents;
// the store, detector, and dashboard only ever see this shape.

export type EventSource = "supervisor" | "hook" | "transcript";

export type EventType =
  | "run_start"
  | "run_end"
  | "tool_use"
  | "assistant"
  | "user"
  | "output"
  | "error"
  | "stop";

export interface LoopEvent {
  runId: string;
  source: EventSource;
  type: EventType;
  tool?: string | null;
  iteration?: number | null;
  tokensIn?: number | null;
  tokensOut?: number | null;
  costUsd?: number | null;
  text?: string | null;
  ts: number;
}

export type RunStatus =
  | "running"
  | "done"
  | "failed"
  | "killed"
  | "stuck";

export interface Run {
  runId: string;
  label: string;
  agent: string;
  cwd: string;
  status: RunStatus;
  pid: number | null;
  iteration: number;
  costUsd: number;
  startedAt: number;
  endedAt: number | null;
  lastEventAt: number;
  flags: string[];
  killRequested: boolean;
  /** Whether loopd owns the process (Model A) and can kill it. */
  owned: boolean;
}

export function defaultRun(runId: string): Run {
  const now = Date.now();
  return {
    runId,
    label: runId,
    agent: "unknown",
    cwd: "",
    status: "running",
    pid: null,
    iteration: 0,
    costUsd: 0,
    startedAt: now,
    endedAt: null,
    lastEventAt: now,
    flags: [],
    killRequested: false,
    owned: false,
  };
}

export function newRunId(prefix = "run"): string {
  return `${prefix}_${Date.now().toString(36)}_${Math.random()
    .toString(36)
    .slice(2, 7)}`;
}
