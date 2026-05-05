//! Shared application state injected into every axum handler.
//!
//! ## Demo subscriber keypair sourcing
//!
//! Per agent rules and `CLAUDE.md` "Project facts", the demo subscriber
//! signer lives at `~/.config/solana/id.json`. We do NOT accept a
//! file path from environment input — that opens an env-controlled file
//! read that static analyzers (correctly) flag. Instead the operator
//! provides the keypair body inline via stdin at startup OR enables
//! "assemble-only" mode where the facilitator returns unsigned txs.
//!
//! Inline-stdin path keeps the data flow contained: bytes flow from a
//! controlled FD into a pure parser, never through the filesystem.

use std::sync::Arc;

use anyhow::{Context, Result};
use solana_keypair::Keypair;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub inner: Arc<AppStateInner>,
}

pub struct AppStateInner {
    pub config: Config,
    pub rpc: RpcClient,
    /// Optional demo subscriber keypair. `None` puts the facilitator in
    /// "assemble-only" mode: it builds and returns unsigned txs (not used by
    /// the current handlers, but reserved for production parity).
    pub demo_subscriber: Option<Keypair>,
}

impl AppState {
    /// Build with optional pre-loaded subscriber keypair. The keypair is
    /// passed in as raw bytes by the caller (typically `bin/`-level code
    /// that read it from stdin); we never touch the filesystem here.
    pub async fn new(config: Config, demo_subscriber: Option<Keypair>) -> Result<Self> {
        let rpc = RpcClient::new(config.rpc_url.clone());
        Ok(Self {
            inner: Arc::new(AppStateInner {
                config,
                rpc,
                demo_subscriber,
            }),
        })
    }
}

/// Decode a Solana JSON keypair (64-byte array). Pure parser — no I/O.
///
/// Callers feed this from stdin or from an in-memory byte source; the
/// facilitator binary deliberately does NOT take a path argument from env
/// input, to keep the data-flow scope narrow.
pub fn parse_keypair_json(body: &str) -> Result<Keypair> {
    let bytes: Vec<u8> =
        serde_json::from_str(body).context("keypair input is not a JSON byte array")?;
    if bytes.len() != 64 {
        anyhow::bail!("expected 64-byte keypair, got {} bytes", bytes.len());
    }
    // `Keypair::try_from(&[u8])` is the v3 successor to deprecated
    // `Keypair::from_bytes`. See `.claude/rules/source-of-truth.md`.
    Keypair::try_from(&bytes[..]).map_err(|e| anyhow::anyhow!("invalid keypair bytes: {e}"))
}
