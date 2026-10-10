//! cargogc — reclaim the cargo build caches cargo will never read again.
//!
//! Layer split (this is the whole point of the tool: one implementation of the
//! judgement, reused by every repository, instead of one shell script per repo):
//!
//! * [`judge`]   — which paths may be deleted, and on what mechanical grounds
//! * [`account`] — how much deleting them actually returns (inode-honest `du`)
//! * [`gate`]    — prove that no build is writing into the tree, or refuse
//! * [`ledger`]  — append-only event log, plus the JSON writer/reader shared with `--json`
//! * [`report`]  — human-readable and `--json` rendering
//!
//! `main.rs` owns the CLI and the exit codes; nothing here parses arguments, and
//! nothing here knows about any particular repository (see the design's refusal list:
//! the tool does not read `Cargo.toml`, it is *told* which profile trees are dead).

pub mod account;
pub mod gate;
pub mod judge;
pub mod ledger;
pub mod report;

/// Version string reported by `--version`, the JSON output and the ledger.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
