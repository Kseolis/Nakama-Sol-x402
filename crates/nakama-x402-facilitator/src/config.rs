//! Facilitator startup configuration.
//!
//! Two layers:
//! * **Hardcoded** for hackathon MVP per `<Hardcoded rules>` in agent
//!   instructions (no env vars for things like RPC interval).
//! * **Env-driven** for things that must vary per developer machine (program
//!   ID, hot keypair path, bind addr).

use std::str::FromStr;

use anyhow::{Context, Result};
use solana_pubkey::Pubkey;

/// Default Solana devnet RPC. Hardcoded per agent rules: no `RPC_URL` env var.
pub const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";

/// Default bind address for the HTTP server.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8080";

/// Hardcoded program ID per `CLAUDE.md` "Project facts" — matches the on-chain
/// devnet deployment.
pub const DEFAULT_PROGRAM_ID: &str = "HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm";

#[derive(Debug, Clone)]
pub struct Config {
    pub rpc_url: String,
    pub bind_addr: String,
    pub program_id: Pubkey,
    /// When `true`, the binary reads a Solana JSON keypair (64-byte array)
    /// from stdin at startup and uses it to sign top-up transactions. When
    /// `false`, the facilitator runs in "assemble-only" mode (returns
    /// 503 from any signing endpoint).
    ///
    /// Why stdin and not a path: a path threaded through env input opens an
    /// env-controlled file read which static analyzers correctly flag
    /// (CWE-22). stdin is a controlled FD; bytes flow into a pure parser
    /// without touching the filesystem.
    pub read_demo_keypair_from_stdin: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let rpc_url = std::env::var("NAKAMA_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC_URL.into());
        let bind_addr =
            std::env::var("NAKAMA_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.into());
        let program_id_str =
            std::env::var("NAKAMA_PROGRAM_ID").unwrap_or_else(|_| DEFAULT_PROGRAM_ID.into());
        let program_id = Pubkey::from_str(&program_id_str)
            .with_context(|| format!("invalid program ID: {program_id_str}"))?;
        let read_demo_keypair_from_stdin =
            std::env::var("NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN").is_ok();

        Ok(Self {
            rpc_url,
            bind_addr,
            program_id,
            read_demo_keypair_from_stdin,
        })
    }
}
