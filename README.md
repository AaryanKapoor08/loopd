<div align="center">

# loopd

**A vendor-neutral control plane for AI agent loops.**

See, unify, and govern every agent loop you run, from one cockpit.

[![CI](https://github.com/AaryanKapoor08/loopd/actions/workflows/ci.yml/badge.svg)](https://github.com/AaryanKapoor08/loopd/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@loopd/sdk.svg)](https://www.npmjs.com/package/@loopd/sdk)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.80%2B-orange.svg)](https://www.rust-lang.org)

</div>

---

loopd sits on top of the agents you already run, Claude Code, Codex, or a plain
Anthropic-SDK loop, and gives you one place to watch them and one set of rules to
hold them to. It is not an agent framework, not a Claude Code dashboard clone, and
not an IDE. It is a control plane: a background daemon that ingests every loop,
normalizes them into one model, and governs them with budget, runaway, and
no-progress policies.

**Why it exists.** Anthropic's Agent view manages Claude sessions. Antigravity
manages Gemini. LangSmith traces LangGraph. None of them unify *across vendors and
surfaces*, and none of them *govern* (caps plus auto-stop). That gap, cross-surface
visibility plus enforcement, is the whole reason loopd is here.

## The one safety promise

**loopd never edits your code.** Its own process never writes to your repository or
working tree. The worst thing it can ever do is one of two things:

- **stop an agent it owns** (Mode A: it kills or pauses the process it spawned), or
- **return a `kill` verdict your loop obeys** (the SDK: loopd does not own the
  process, so it asks, and your loop unwinds).

The no-progress detector reads git strictly read-only (`diff` and `status`). The
optional test command is one *you* wrote. That is the entire blast radius.

## Architecture

One Rust binary is the whole engine: the daemon, the supervisor, the TUI, and the
CLI all ship in `loopd`. The daemon is the only long-lived process and the only one
that holds state. Everything else is a thin client over its local HTTP API.

```
       CLIENTS (thin, stateless)                    THE DAEMON  (loopd, one process)
                                                ┌──────────────────────────────────────┐
   ┌────────────────────────┐   HTTP/JSON       │   axum HTTP API   (127.0.0.1 only)    │
   │  loop run / ps / kill  │ ────────────────► │  ────────────────────────────────    │
   │  loop logs  (clap CLI) │                   │                                       │
   └────────────────────────┘                   │   AppState                            │        INPUTS / SURFACES
   ┌────────────────────────┐                   │    • Store        rusqlite, SQLite    │
   │  loop dash  (ratatui   │ ◄── poll /runs ── │                   WAL, ~/.loopd/*.db  │   ┌──  Mode A: supervisor (owned)
   │  + crossterm TUI)      │                   │    • Supervisor   PTY registry        │◄──┤    portable-pty spawns CC / Codex
   └────────────────────────┘                   │    • Governor     1.5s tick           │   └──  headless, parses stream -> events
   ┌────────────────────────┐   POST /ingest    │    • Config       ~/.loopd/config.yaml │
   │  @loopd/sdk  (your TS  │ ────────────────► │                                       │   ┌──  Mode B: observer (read-only)
   │  loop, framework loop) │ ◄── verdict ───── │        one normalized LoopEvent       │◄──┤    CC hooks POST /ingest + notify
   └────────────────────────┘                   │   events -> Store -> Governor -> you  │   └──  transcript tailer (JSONL)
                                                └──────────────────────────────────────┘   ┌──  Surface 2: the SDK
                                                                                            └──  /ingest + verdict enforcement
```

Every input, an owned PTY stream, a Claude Code hook, a transcript line, or an SDK
call, normalizes to **one `LoopEvent`**. Runs are aggregated from events. The model
is defined once in Rust (`src/core/events.rs`) and exported to the TypeScript SDK
with `ts-rs`, so the wire types never drift from the core.

```rust
struct LoopEvent {
    run_id: String,
    source: Source,                     // Supervisor | Hook | Transcript | Sdk
    kind: EventKind,                    // RunStart | ToolUse | ToolResult | TokenUsage | ...
    tool: Option<String>,
    tool_input_hash: Option<u64>,       // stable hash of args -> repeated-action detection
    tool_status: Option<ToolStatus>,    // Ok | Error | Denied | TimedOut -> error-streak detection
    iteration: Option<u32>,
    tokens_in: Option<u32>,
    tokens_out: Option<u32>,
    cost_usd: Option<f64>,              // agent-reported when available, else computed
    parent_tool_use_id: Option<String>, // sub-agent / sidechain attribution
    ts: i64,
}
```

### The governor

The governor is loopd's wedge. It runs on the daemon tick (every 1.5 seconds),
evaluates each live run against a registry of deterministic policies plus a
best-effort no-progress signal, and returns a decision: the run's full flag set and
the on-trip action to take, already clamped by ownership.

| Signal | What trips it | Kind |
|---|---|---|
| **caps** | iterations, cost (USD), or wall-clock duration over budget | deterministic |
| **repeated-action** | the same tool and input hash firing in a tight loop | deterministic |
| **error-streak** | a run of `Error` / `Denied` / `TimedOut` tool results | deterministic |
| **context-exhaustion** | context window at or above 90 percent | deterministic |
| **no-progress** | no git diff plus failing opt-in test command across N iterations | best-effort |

The pure per-tick checks live in `src/policies/`, one `impl Policy` each, so adding
a detector is one more entry in the registry. The two signals that need memory
across ticks, the no-progress git fingerprint and the warn throttle, live in the
detector itself. On-trip actions escalate `warn -> notify -> pause -> kill`, and an
observed (unowned) run always clamps to `notify`, because there is no process to
stop.

## Install

The shipped binary is **`loopd`**. These docs use `loop` for brevity. Alias it once
if you like, or just type `loopd`:

```sh
alias loop=loopd              # bash / zsh  (add to ~/.bashrc or ~/.zshrc)
# PowerShell:  Set-Alias loop loopd
```

### Download a release binary (recommended)

Grab the latest from the [Releases](https://github.com/AaryanKapoor08/loopd/releases)
page, or use the one-line installer:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AaryanKapoor08/loopd/releases/latest/download/loopd-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/AaryanKapoor08/loopd/releases/latest/download/loopd-installer.ps1 | iex"
```

Prebuilt binaries ship for Windows, macOS (Intel and Apple Silicon), and Linux
(x86_64 and arm64).

### Build from source

```sh
cargo install --git https://github.com/AaryanKapoor08/loopd loopd
```

Requires a Rust toolchain (1.80+) and a C compiler, since `rusqlite` compiles SQLite
from source (MSVC on Windows).

### The SDK

```sh
npm i @loopd/sdk
```

## Quickstart

```sh
loop init                       # writes ~/.loopd/config.yaml, checks your agents,
                                # and starts the daemon. idempotent, run it anytime

loop run "add a --json flag to the export command"   # spawn an owned agent loop
loop dash                       # open the live cockpit (auto-starts the daemon)
```

You never start the daemon by hand. `run`, `ps`, `dash`, and `logs` all auto-start
it on first use. Other useful commands:

```sh
loop ps                         # one line per run: status, iters, tokens, $, ctx%
loop logs <id> --follow         # stream a run's events
loop kill <id>                  # stop a run (the worst action loopd takes)
loop run --agent codex "<task>" # same cockpit, different vendor
```

### Set guardrails

Caps trip a configurable action (`warn`, `notify`, `pause`, or `kill`). Defaults
live in `~/.loopd/config.yaml` (`maxIterations: 50`, `maxCostUsd: 2.00`,
`maxDurationMin: 30`, on-trip `warn`). Override per run:

```sh
loop run "refactor the parser" --max-cost 1.50 --max-iterations 30 --on-trip kill
```

Beyond caps, the governor flags runaway loops (same tool and input repeating, error
streaks, context exhaustion) and, if you set a `testCommand`, no-progress (no git
diff plus failing tests across N iterations).

## Two modes

loopd watches agents two ways, and both land in the same cockpit.

**Mode A, supervisor (owned).** `loop run "<task>"` spawns the agent through a PTY
(ConPTY on Windows), so loopd owns the process: it can pause or kill it. Loops
survive your terminal closing, because the daemon owns them, not your shell.

Pause is cross-platform without relying on process suspend. A PTY exposes no SIGSTOP
and ConPTY has none either, so pause captures the agent's native session id and
gracefully stops the child, and resume re-spawns with the native resume flag
(`claude --resume <session_id>` or `codex exec resume <thread_id>`). Status becomes
`Paused`. Pause is opt-in and never on the critical path.

**Mode B, observer (read-only).** For `claude` sessions *you* start yourself:

```sh
loop hooks install              # merges loopd's hooks into ~/.claude/settings.json
```

Now your own Claude Code sessions show up in `loop dash` as `obs` (observed). loopd
ingests their hooks and tails the transcript, but it never owns the process, so for
observed runs every on-trip action degrades to `notify`. (`loop hooks remove` and
`loop hooks status` manage it. The merge is non-destructive: it preserves your JSON
key order and leaves your other hooks alone.)

## Surface 2: govern a programmatic loop

The third surface is [`@loopd/sdk`](sdk). Wrap a plain API or framework loop so it
appears in the same cockpit and obeys the same caps as a CLI run. loopd does not own
your process, so it enforces through a verdict your loop reads: `check()` throws
`LoopdHaltError` when the verdict is `pause` or `kill`.

```ts
import { track, LoopdHaltError } from "@loopd/sdk";

const run = await track("anthropic api loop", {
  agent: "anthropic",
  maxCostUsd: 0.05,   // the same cap that governs `loop run`
  onTrip: "kill",
});

try {
  for (let turn = 1; turn <= 50; turn++) {
    await run.check();          // throws once a cap trips. this is the enforcement seam
    await run.iteration();

    const response = await client.messages.create({ /* ... */ });

    await run.cost(costOf(response.usage), {   // feed cost so the cap can trip
      in: response.usage.input_tokens,
      out: response.usage.output_tokens,
    });
  }
} catch (err) {
  if (err instanceof LoopdHaltError) {
    console.log(`halted by governance: ${err.verdict}`);
  } else throw err;
}
```

It is **fail-open**: if the daemon is down, `track()` and `check()` degrade to no-ops
and your loop keeps running. A governor that goes offline must never be the thing
that breaks your work. A full runnable example lives in
[`sdk/examples/api-loop.ts`](sdk/examples/api-loop.ts), and there is a LangGraph
adapter in [`sdk/src/langgraph.ts`](sdk/src/langgraph.ts).

## Use cases

- **Overnight refactors with a hard budget.** `loop run "<task>" --max-cost 3
  --on-trip kill` and close the laptop. The daemon owns the loop, so it keeps going,
  and the cap kills it before it burns your budget on a wrong turn.
- **Watch your own Claude Code sessions.** `loop hooks install`, then work normally.
  Every session shows in `loop dash` with live tokens, cost, and context percentage,
  and you get a `notify` the moment one starts spinning.
- **A fleet in one table.** Claude Code, Codex, and SDK loops all normalize to the
  same model, so `loop ps` is one table across every vendor and surface, not four
  dashboards in four tabs.
- **Govern a production agent loop.** Wrap your Anthropic-SDK or LangGraph loop with
  the SDK and it obeys the same cost and iteration caps as an interactive run, with
  the same auto-stop, and fails open if the daemon is not there.
- **Catch the classic failure modes.** Repeated-action, error-streak, and
  context-exhaustion flags surface the loops that are stuck editing the same file,
  retrying a failing tool, or about to fall off the end of their context window.

## Tech stack

One Rust binary is the daemon, supervisor, TUI, and CLI. No runtime services, no
system SQLite, no TLS stack. The daemon binds `127.0.0.1` only.

| Concern | Crate | Why this one |
|---|---|---|
| async runtime | `tokio` | drives the daemon: owns processes, hosts the API, runs the tick |
| HTTP API | `axum` | serves the local routes the CLI, TUI, and SDK all call |
| HTTP client | `reqwest` (blocking, no TLS) | keeps the CLI synchronous with its own runtime, localhost only |
| store | `rusqlite` (bundled) | one SQLite file in WAL mode, compiled from source, zero system deps |
| process ownership | `portable-pty` | spawns agents through a PTY (ConPTY on Windows) to capture streaming output |
| stream cleaning | `strip-ansi-escapes` | strips ANSI and line-discipline noise before parsing each JSON line |
| cockpit TUI | `ratatui` + `crossterm` | the `loop dash` live view |
| CLI parsing | `clap` (derive) | the command surface |
| serialization | `serde` / `serde_json` / `serde_yaml` | one model for wire types and config |
| key-order preservation | `serde_json` (`preserve_order`) | edits `~/.claude/settings.json` without churning the user's file |
| home resolution | `dirs` | `~/.loopd` resolves the same on Windows and unix |
| transcript watching | `notify` | tails Claude Code transcript JSONL as it is appended (Mode B) |
| TS type export | `ts-rs` (`import-esm`) | generates the SDK's wire types from Rust so they never drift |
| errors / logging | `anyhow` / `tracing` | ergonomic propagation and structured diagnostics |

The wire types in `sdk/src/types/` are generated (`cargo test export_bindings`), so
never hand-edit them. Design decisions and rationale live in
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Repository layout

```
src/
  main.rs          entry point: dispatches CLI subcommands
  cli/             clap subcommands: run, ps, kill, logs, dash, hooks, init, set, ...
  daemon/          axum server, lifecycle (spawn/detach), local HTTP client
  supervisor/      Mode A: PTY-owned agent processes and the run registry
  observer/        Mode B: CC hooks, transcript tailer, SDK ingest, webhook
  agents/          per-vendor stream parsers (claude, codex) -> LoopEvent
  core/            the model: events, store (rusqlite), detector, git, pricing
  policies/        the governance registry: one impl Policy per detector
  dashboard/       ratatui app + UI for `loop dash`
  config.rs        ~/.loopd/config.yaml (caps, runaway, on-trip)
sdk/               @loopd/sdk: TS SDK, generated types, LangGraph adapter, examples
```

## Development

```sh
cargo build                     # the loopd engine
cargo test                      # 101 unit + 2 integration tests
cd sdk && npm run typecheck     # the SDK (wire types generated from Rust via ts-rs)
```

Releases are cut by tagging a version (`vX.Y.Z`).
[cargo-dist](https://github.com/axodotdev/cargo-dist) builds the binaries, and the
npm package is published with `npm publish` from `sdk/`.

## License

[MIT](LICENSE)
