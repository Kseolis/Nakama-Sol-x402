//! Nakama x402 facilitator binary entry point.
//!
//! See `lib.rs` for routing / handler logic. This thin shell is responsible
//! for: env loading, tracing init, RPC client construction, optional demo
//! subscriber keypair load, server bind. All fallible startup paths use
//! `anyhow` (binary-only); per-request errors use `thiserror` (crate-internal).

use anyhow::Context;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nakama_x402_facilitator::run()
        .await
        .context("facilitator exited with error")
}
