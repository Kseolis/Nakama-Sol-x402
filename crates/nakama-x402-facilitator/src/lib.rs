//! axum HTTP harness for ADR-007 demo flows.
//!
//! Endpoints (per ADR-007 kickoff §4):
//! * `POST /subscriptions/{sub_pda}/top-up`         body `{ "amount": u64 }`
//! * `GET  /subscriptions/{sub_pda}/computed-status`
//! * `GET  /healthz`
//!
//! No `/grace` endpoint — `computed-status` covers it (YAGNI).
//!
//! ## Demo signing model
//!
//! ADR-007 §HTTP API surface notes: in demo mode the facilitator holds a hot
//! subscriber keypair (env `NAKAMA_DEMO_SUBSCRIBER_KEYPAIR` → file path).
//! Production path post-hackathon: facilitator returns an unsigned tx for a
//! wallet adapter. We keep the signing path behind a `top_up_signed` helper
//! so the production refactor is one function swap.

pub mod config;
pub mod error;
pub mod handlers;
pub mod state;

use std::net::SocketAddr;

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::trace::TraceLayer;

pub use config::Config;
pub use error::ApiError;
pub use state::AppState;

/// Build the axum router. Exposed for integration tests that drive the app
/// without binding a real socket.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(handlers::healthz))
        .route(
            "/subscriptions/{sub_pda}/top-up",
            post(handlers::top_up::handle),
        )
        .route(
            "/subscriptions/{sub_pda}/computed-status",
            get(handlers::computed_status::handle),
        )
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

/// Top-level binary entry. Loads config from env, initializes tracing,
/// optionally reads a demo subscriber keypair from stdin, binds the
/// listener, serves until shutdown signal.
pub async fn run() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::from_env()?;
    tracing::info!(
        rpc_url = %config.rpc_url,
        program_id = %config.program_id,
        bind_addr = %config.bind_addr,
        "starting facilitator"
    );

    let demo_subscriber = if config.read_demo_keypair_from_stdin {
        Some(read_demo_keypair_from_stdin()?)
    } else {
        tracing::warn!(
            "demo subscriber keypair not loaded; signing endpoints will return 503. \
             Set NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN=1 and pipe ~/.config/solana/id.json on stdin to enable."
        );
        None
    };

    let state = AppState::new(config.clone(), demo_subscriber).await?;
    let app = router(state);

    let addr: SocketAddr = config.bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "facilitator listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Read a Solana JSON keypair from stdin (the operator pipes the file).
/// Bytes flow from a controlled FD into a pure parser; no filesystem path
/// is constructed from environment input.
fn read_demo_keypair_from_stdin() -> anyhow::Result<solana_keypair::Keypair> {
    use std::io::Read;
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|e| anyhow::anyhow!("read keypair from stdin: {e}"))?;
    state::parse_keypair_json(&body)
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,nakama_x402_facilitator=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}
