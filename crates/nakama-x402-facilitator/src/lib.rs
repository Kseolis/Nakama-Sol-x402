//! axum HTTP harness for ADR-007 demo flows.
//!
//! Endpoints (per ADR-007 kickoff §4):
//! * `POST /subscriptions/{sub_pda}/top-up`         body `{ "amount": u64 }`
//! * `GET  /subscriptions/{sub_pda}/computed-status`
//! * `GET  /healthz`
//!
//! No `/grace` endpoint — `computed-status` covers it (YAGNI).
//!
//! ## ADR-015 §F3 — auth + secure defaults
//!
//! Protected routes (every endpoint that touches RPC or signs a tx) are
//! gated by a `Bearer` token middleware. The token comes from
//! `NAKAMA_FACILITATOR_API_KEY` and is checked per-request via
//! `auth::require_bearer`. `/healthz` is intentionally open so probes work
//! against a freshly-started container without baking credentials into the
//! orchestrator.
//!
//! Default bind addr is loopback only (`127.0.0.1:8080`). Public exposure
//! requires `NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND=1` AND an explicit
//! `NAKAMA_BIND_ADDR` — see `config::Config::from_env`.
//!
//! Cheap response-header defence (Strict-Transport-Security,
//! X-Content-Type-Options) is layered via `tower_http::set_header` on the
//! whole router. Operators that front this with a TLS-terminating reverse
//! proxy get sane defaults without extra config.
//!
//! ## Demo signing model
//!
//! ADR-007 §HTTP API surface notes: in demo mode the facilitator holds a hot
//! subscriber keypair (env `NAKAMA_DEMO_SUBSCRIBER_KEYPAIR` → file path).
//! Production path post-hackathon: facilitator returns an unsigned tx for a
//! wallet adapter. We keep the signing path behind a `top_up_signed` helper
//! so the production refactor is one function swap.

pub mod auth;
pub mod config;
pub mod error;
pub mod handlers;
pub mod state;

use std::net::SocketAddr;

use axum::{
    http::{header, HeaderValue},
    middleware::from_fn_with_state,
    routing::{get, post},
    Router,
};
use tower_http::{set_header::SetResponseHeaderLayer, trace::TraceLayer};

pub use config::Config;
pub use error::ApiError;
pub use state::AppState;

/// Build the axum router. Exposed for integration tests that drive the app
/// without binding a real socket.
///
/// Route topology:
/// * `/healthz` — open (no auth).
/// * `/subscriptions/{pda}/top-up` — auth required.
/// * `/subscriptions/{pda}/computed-status` — auth required.
///
/// We attach `require_bearer` to a sub-router so `/healthz` stays open.
/// `Router::merge` composes the two without mounting under a path prefix.
pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/subscriptions/{sub_pda}/top-up",
            post(handlers::top_up::handle),
        )
        .route(
            "/subscriptions/{sub_pda}/computed-status",
            get(handlers::computed_status::handle),
        )
        .route_layer(from_fn_with_state(state.clone(), auth::require_bearer))
        .with_state(state.clone());

    let open = Router::new()
        .route("/healthz", get(handlers::healthz))
        .with_state(state);

    Router::new()
        .merge(open)
        .merge(protected)
        // Cheap defence headers — applied to every response. Strict-Transport-
        // Security tells browsers to upgrade to HTTPS for a year (only meaningful
        // when reverse-proxied over TLS, but harmless otherwise).
        // X-Content-Type-Options prevents MIME-sniffing on JSON responses.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
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
        max_top_up_amount = config.max_top_up_amount,
        auth_enabled = config.api_key.is_some(),
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
