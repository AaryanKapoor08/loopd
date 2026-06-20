//! Core domain — the data and rules everything else depends on.
//!
//! This is the brain's model layer. Everything that happens in loopd, on any
//! surface, converges here as exactly one normalized type: `LoopEvent`. New
//! agents and surfaces are added as adapters that emit `LoopEvent`s — never as
//! a parallel data model.
//!
//! Contents:
//! - `events`   — `LoopEvent`, `Run`, and their enums (`#[derive(Serialize, TS)]`).
//! - `pricing`  — model→price map; cost fallback when an agent reports tokens only.
//! - `store`    — `rusqlite` (WAL) persistence; daemon-only writer.
//!
//! Planned (later phases):
//! - `detector` — governance: caps + runaway/no-progress detection (Phase 6).
//! - `git`      — read-only `git diff` hashing for the no-progress signal (Phase 6).

pub mod events;
pub mod pricing;
pub mod store;
