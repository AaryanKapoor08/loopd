/**
 * api-loop.ts — a tiny Anthropic-SDK loop governed by loopd.
 *
 * This is Surface 2 of loopd in action: a *programmatic* loop (not a CLI agent)
 * that reports each turn into the daemon and is **killed by the same cost cap
 * that governs `loop run`**. The loop calls `loopd.track(...)` once, then on
 * every turn:
 *   1. `run.check()` at the top — throws {@link LoopdHaltError} if the daemon's
 *      verdict is `pause`/`kill` (this is the enforcement seam, ARCHITECTURE §4);
 *   2. `run.iteration()` to advance the turn counter;
 *   3. one Anthropic `messages.create` call;
 *   4. `run.cost(usd, {in, out})` with the turn's cost so the daemon's
 *      `maxCostUsd` cap can trip.
 *
 * Run it and watch it appear in `loop dash` alongside the CLI/Codex agents; when
 * cumulative cost crosses `maxCostUsd`, the next `check()` throws and the loop
 * unwinds — loopd never touches the process, the loop obeys the verdict.
 *
 * Prerequisites:
 *   - the loopd daemon running (`loopd daemon start`);
 *   - `npm i @anthropic-ai/sdk` in this folder (kept out of the SDK's own deps);
 *   - `ANTHROPIC_API_KEY` in the environment.
 *
 * Run:  npx tsx examples/api-loop.ts
 */

import Anthropic from "@anthropic-ai/sdk";
import { track, LoopdHaltError } from "@loopd/sdk";

/** Latest Claude model (see the `claude-api` skill / `/claude-api`). */
const MODEL = "claude-opus-4-8";
/** Opus 4.8 pricing, USD per 1M tokens (input / output). */
const INPUT_PER_MTOK = 5.0;
const OUTPUT_PER_MTOK = 25.0;

/** Resolve a turn's cost from reported usage (the precedence loopd uses too:
 *  agent-reported when available, else computed from tokens). */
function costOf(usage: Anthropic.Usage): number {
  const inTokens =
    usage.input_tokens +
    (usage.cache_creation_input_tokens ?? 0) +
    (usage.cache_read_input_tokens ?? 0);
  return (inTokens / 1e6) * INPUT_PER_MTOK + (usage.output_tokens / 1e6) * OUTPUT_PER_MTOK;
}

async function main(): Promise<void> {
  const client = new Anthropic();

  // Register the loop. `onTrip: "kill"` mirrors `loop run --on-trip kill`: when a
  // cap trips, loopd returns a `kill` verdict and `check()` throws.
  const run = await track("anthropic api loop", {
    agent: "anthropic",
    model: MODEL,
    maxCostUsd: 0.05, // a deliberately small cap so the demo halts in a few turns
    onTrip: "kill",
  });
  console.log(`tracking run ${run.runId} — open \`loop dash\` to watch it`);

  // A simple open-ended loop: keep asking the model to continue brainstorming.
  const messages: Anthropic.MessageParam[] = [
    {
      role: "user",
      content:
        "Brainstorm names for a vendor-neutral control plane for AI agent loops. " +
        "Give me three new ideas, then wait for me to say 'continue'.",
    },
  ];

  try {
    for (let turn = 1; turn <= 50; turn++) {
      // Enforcement point: throws once cumulative cost crosses maxCostUsd.
      await run.check();
      await run.iteration();

      const response = await client.messages.create({
        model: MODEL,
        max_tokens: 1024,
        messages,
      });

      const text = response.content
        .filter((b): b is Anthropic.TextBlock => b.type === "text")
        .map((b) => b.text)
        .join("");
      console.log(`\n── turn ${turn} ─────────────────────────────\n${text}`);

      // Report the turn's cost + tokens; the returned verdict lets the loop react
      // inline too (we additionally gate at the top of the next turn via check()).
      const verdict = await run.cost(costOf(response.usage), {
        in: response.usage.input_tokens,
        out: response.usage.output_tokens,
      });
      console.log(`[loopd] verdict after turn ${turn}: ${verdict}`);

      // Keep the conversation going.
      messages.push({ role: "assistant", content: response.content });
      messages.push({ role: "user", content: "continue" });
    }
  } catch (err) {
    if (err instanceof LoopdHaltError) {
      console.log(`\n[loopd] halted by governance: ${err.verdict} (run ${err.runId})`);
      return;
    }
    throw err;
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
