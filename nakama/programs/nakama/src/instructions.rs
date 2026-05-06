//! Instruction module aggregator.
//!
//! MVP day 1–7 surface: `create_plan` (ADR-014), `subscribe` (ADR-002),
//! `charge` (ADR-004), `cancel` (ADR-002 + ADR-013 split).
//! Cycle-3 addition: `cleanup` (ADR-013).
//! Cycle-4 addition: `top_up` (ADR-007).
//! Cycle-6+ ADR-x402-001: `open_session`, `close_session` (Phase 2),
//! `settle_usage` (Phase 3 — pending).

pub mod cancel;
pub mod charge;
pub mod cleanup;
pub mod close_session;
pub mod create_plan;
pub mod open_session;
pub mod pause;
pub mod resume;
pub mod settle_usage;
pub mod subscribe;
pub mod top_up;

// Glob re-exports so Anchor's `#[program]` macro finds the generated
// `__client_accounts_*` and `__cpi_client_accounts_*` modules at the crate
// root. Each handler's free function is renamed below to avoid the glob
// collision on the bare name `handler`.
pub use cancel::*;
pub use charge::*;
pub use cleanup::*;
pub use close_session::*;
pub use create_plan::*;
pub use open_session::*;
pub use pause::*;
pub use resume::*;
pub use settle_usage::*;
pub use subscribe::*;
pub use top_up::*;
