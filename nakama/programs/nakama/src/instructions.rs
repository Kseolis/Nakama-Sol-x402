//! Instruction module aggregator.
//!
//! `create_plan` (ADR-014), `subscribe` (ADR-002), `cancel` (ADR-002) —
//! MVP day 1–7 surface. `charge` (ADR-004) follows in a separate task.

pub mod cancel;
pub mod create_plan;
pub mod subscribe;

// Glob re-exports so Anchor's `#[program]` macro finds the generated
// `__client_accounts_*` and `__cpi_client_accounts_*` modules at the crate
// root. Each handler's free function is renamed below to avoid the glob
// collision on the bare name `handler`.
pub use cancel::*;
pub use create_plan::*;
pub use subscribe::*;
