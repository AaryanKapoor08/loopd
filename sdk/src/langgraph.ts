/**
 * @loopd/sdk — LangGraph / LangChain integration (v1.5 stub).
 *
 * Surface 2's `track()` governs *any* programmatic loop by hand (see
 * `examples/api-loop.ts`). A framework like LangGraph already has a callback
 * surface — `BaseCallbackHandler` — that fires on every LLM call, tool call, and
 * chain step. The v1.5 plan is a `LoopdCallback` that bridges those hooks onto a
 * {@link TrackedRun}: each LLM end reports cost/tokens, each tool start reports a
 * tool use, and a graph-node boundary calls `check()` so a tripped cap unwinds
 * the graph the same way it halts the hand-written loop here.
 *
 * This file is the **shape only** — no implementation, no `@langchain/*`
 * dependency. It pins the wire the impl will satisfy so the API is reviewable now
 * and the impl is a drop-in later (it would `implements` LangChain's
 * `BaseCallbackHandler` and own a `TrackedRun` internally).
 */

import type { ReportEvent, TrackedRun, TrackOptions, Verdict } from "./index.js";

/** Options for {@link LoopdCallback}: the same {@link TrackOptions} the manual API takes. */
export type LoopdCallbackOptions = TrackOptions;

/**
 * The planned LangGraph/LangChain callback handler. A future impl wraps a
 * {@link TrackedRun} and maps framework events onto its reports; the methods
 * below mirror the `BaseCallbackHandler` hooks loopd would implement.
 *
 * v1.5 — not implemented in v1. The fields/signatures are the contract only.
 */
export interface LoopdCallback {
  /** The underlying run this handler reports against. */
  readonly run: TrackedRun;

  /**
   * Graph/chain step boundary → enforcement point. Mirrors `check()`: throws if
   * the daemon's verdict is `pause`/`kill`, unwinding the graph run.
   */
  handleChainStart?(): Promise<void>;

  /** A new LLM call began → advance the iteration counter. */
  handleLLMStart?(): Promise<Verdict>;

  /** An LLM call finished → report its cost / token usage. */
  handleLLMEnd?(event: Pick<ReportEvent, "costUsd" | "tokensIn" | "tokensOut">): Promise<Verdict>;

  /** A tool/function call began → report the tool use. */
  handleToolStart?(tool: string): Promise<Verdict>;

  /** A tool call finished → report its outcome. */
  handleToolEnd?(event: Pick<ReportEvent, "toolStatus" | "text">): Promise<Verdict>;
}

/**
 * Factory the v1.5 impl will expose: register a run and return a
 * {@link LoopdCallback} to pass into a LangGraph/LangChain `callbacks` array.
 * Declared (no body) so the intended signature is fixed; calling it is a
 * v1.5-not-implemented concern.
 */
export type CreateLoopdCallback = (
  label: string,
  opts?: LoopdCallbackOptions,
) => Promise<LoopdCallback>;
