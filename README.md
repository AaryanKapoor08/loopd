<div align="center">

# loopd

**A vendor-neutral control plane for AI agent loops.**
See, unify, and govern every agent loop you run — in one cockpit.

[![CI](https://github.com/AaryanKapoor08/loopd/actions/workflows/ci.yml/badge.svg)](https://github.com/AaryanKapoor08/loopd/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@loopd/sdk.svg)](https://www.npmjs.com/package/@loopd/sdk)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<img src="docs/loop-dash.gif" alt="loop dash — the live cockpit" width="820">

</div>

---

loopd sits **on top of** the agents you already run — Claude Code, Codex, a plain
Anthropic-SDK loop — and gives you one place to watch them and one set of rules to
hold them to. It is **not** an agent framework, **not** a Claude Code dashboard clone,
and **not** an IDE. It is a control plane: a background daemon that ingests every
loop, unifies them into one model, and **governs** them with budget / runaway /
no-progress policies.

> **Why this exists.** Anthropic's Agent view manages Claude sessions; Antigravity
> manages Gemini; LangSmith traces LangGraph. None of them unify *across vendors and
> surfaces*, and none of them **govern** (caps + auto-stop). That gap — cross-surface
> visibility plus enforcement — is what loopd fills.

## The one safety promise

**loopd never edits your code.** Its own process never writes to your repository or
working tree. The worst thing it can ever do is:

- **stop an agent it owns** (Mode A — it kills/pauses the process it spawned), or
- **return a `kill` verdict your loop obeys** (the SDK — loopd doesn't own the
  process, so it just *asks*, and your loop unwinds).

The no-progress detector reads git **read-only** (`diff`/`status`); the optional
test command is one *you* wrote. That's the whole blast radius.

## How it works

```
   CLIENTS (thin)                  THE DAEMON (loopd)              SURFACES
 ┌───────────────┐   HTTP/JSON   ┌────────────────────────┐
 │ loop run / ps │ ────────────► │  axum API (localhost)  │ ◄── Mode A: owns CC/Codex
 │ kill / logs   │               │  • SQLite store (WAL)  │     via a PTY, parses the
 │ loop dash TUI │ ◄─ poll ───── │  • Supervisor registry │     stream → LoopEvent
 └───────────────┘               │  • Governor (1.5s tick)│ ◄── Mode B: CC hooks +
 ┌───────────────┐   POST /sdk   │  • Config / policies   │     transcript tailer (RO)
 │ @loopd/sdk    │ ────────────► │                        │ ◄── Surface 2: the SDK
 │ (your loop)   │ ◄─ verdict ── │  one LoopEvent model   │     (/sdk/* + verdict)
 └───────────────┘               └────────────────────────┘
```

Every surface normalizes to **one `LoopEvent`** model and converges on the same store
+ governor. The daemon is the only process with state; the CLI, the TUI, and the SDK
are all thin clients over its local HTTP API.

## Install

The shipped binary is **`loopd`**. These docs use `loop` for brevity — alias it once
(optional), or just type `loopd`:

```sh
alias loop=loopd              # bash / zsh  (add to your ~/.bashrc or ~/.zshrc)
# PowerShell:  Set-Alias loop loopd
```

### Download a release binary (recommended)

Grab the latest from the [**Releases**](https://github.com/AaryanKapoor08/loopd/releases)
page, or use the one-line installer:

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/AaryanKapoor08/loopd/releases/latest/download/loopd-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/AaryanKapoor08/loopd/releases/latest/download/loopd-installer.ps1 | iex"
```

Prebuilt binaries are produced for Windows, macOS (Intel + Apple Silicon), and Linux
(x86_64 + arm64).

### Build from source

```sh
cargo install --git https://github.com/AaryanKapoor08/loopd loopd
```

(Requires a Rust toolchain and a C compiler — `rusqlite` bundles SQLite.)

### The SDK (Surface 2)

```sh
npm i @loopd/sdk
```

## Quickstart

```sh
loop init                       # writes ~/.loopd/config.yaml, checks your agents,
                                # and starts the daemon — idempotent, run it anytime

loop run "add a --json flag to the export command"   # spawn an owned agent loop
loop dash                       # open the live cockpit (auto-starts the daemon)
```

You don't need to start the daemon yourself — `run`, `ps`, `dash`, and `logs` all
**auto-start it** on first use. Other useful commands:

```sh
loop ps                         # one-line-per-run table: status, iters, tokens, $, ctx%
loop logs <id> --follow         # stream a run's events
loop kill <id>                  # stop a run (the worst action loopd takes)
loop run --agent codex "<task>" # same cockpit, different vendor
```

### Set guardrails

Caps trip a configurable action (`warn` / `notify` / `pause` / `kill`). Defaults live
in `~/.loopd/config.yaml` (`maxIterations: 50`, `maxCostUsd: 2.00`,
`maxDurationMin: 30`, on-trip `warn`); override per run:

```sh
loop run "refactor the parser" --max-cost 1.50 --max-iterations 30 --on-trip kill
```

Beyond caps, the governor flags **runaway** loops (same tool+input repeating, error
streaks, context exhaustion) and — opt-in, if you set a `testCommand` — **no-progress**
(no git diff + failing tests across N iterations).

## Two modes

loopd watches agents two ways, and both land in the same cockpit:

**Mode A — supervisor (owned).** `loop run "<task>"` spawns the agent through a PTY, so
loopd owns the process: it can pause (checkpoint + native `--resume`) or kill it. Loops
survive your terminal closing because the daemon, not your shell, owns them.

**Mode B — observer (read-only).** For `claude` sessions *you* start yourself:

```sh
loop hooks install              # merges loopd's hooks into ~/.claude/settings.json
```

Now your own Claude Code sessions appear in `loop dash` as `obs` (observed). loopd
ingests their hooks + tails the transcript, but it **never** owns the process — for
observed runs every on-trip action degrades to `notify`. (`loop hooks remove` /
`loop hooks status` manage it; the merge is non-destructive.)

## Surface 2: govern a programmatic loop

The third surface is the [`@loopd/sdk`](sdk): wrap a plain API / framework loop so it
appears in the same cockpit and obeys the **same caps** as a CLI run. loopd doesn't own
your process, so it enforces through a **verdict** your loop reads — `check()` throws
`LoopdHaltError` when the verdict is `pause`/`kill`.

```ts
import { track, LoopdHaltError } from "@loopd/sdk";

const run = await track("anthropic api loop", {
  agent: "anthropic",
  maxCostUsd: 0.05,   // same cap that governs `loop run`
  onTrip: "kill",
});

try {
  for (let turn = 1; turn <= 50; turn++) {
    await run.check();          // throws once a cap trips — the enforcement seam
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

It's **fail-open**: if the daemon is down, `track()`/`check()` degrade to no-ops and
your loop keeps running — a governor that's offline must never be the thing that breaks
your work. A full runnable example is in [`sdk/examples/api-loop.ts`](sdk/examples/api-loop.ts).

## Development

```sh
cargo build                     # the loopd engine
cargo test                      # 101 unit + 2 integration tests
cd sdk && npm run typecheck     # the SDK (wire types are generated from Rust via ts-rs)
```

The wire types in `sdk/src/types/` are generated from the Rust core (`cargo test
export_bindings`) — never hand-edit them. Releases are cut by tagging a version
(`vX.Y.Z`); [cargo-dist](https://github.com/axodotdev/cargo-dist) builds the binaries
and the npm package is published with `npm publish` from `sdk/`.

## License

[MIT](LICENSE)
