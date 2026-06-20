//! Config — `~/.loopd/config.yaml` (serde_yaml), with sane defaults.
//!
//! Lives at the crate root (not under `core`) because both the daemon (Phase 2)
//! and the agent adapters (Phase 3, `headlessArgs`) need it before the rest of
//! the domain is built. Defaults come from PLAN Part 10.
//!
//! Planned contents (Phase 1): `Config` struct + `load()`/validation.
