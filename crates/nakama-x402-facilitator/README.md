# nakama-x402-facilitator

axum HTTP harness for the ADR-007 demo flow (top-up + computed-status).
Conditional on day-8 GO for the full x402 layer; the two endpoints here
ship as part of the ADR-007 stage-2 deliverable so the demo can run
without a frontend.

## Scope (and a note on timeline — BLK-007-MAJ-4)

This crate was committed during ADR-007 stage-2 implementation (cycle-4)
**before** the day-8 GO/no-go decision for the broader x402 protocol layer
(`PaySession`, `open_session`, `close_session`, `settle_usage`, etc.).
Architect-review flagged this as scope creep relative to Strategy Option D
in [`CLAUDE.md`](../../CLAUDE.md) and the resolution was: keep the crate,
document the timeline inline.

Reasoning: the ADR-007 demo path (top-up rescue from `GracePeriod` +
`computed-status` surfacing) needs an HTTP API surface for the 3-min Loom
moment ("subscription falls into grace → operator runs one curl → state
flips back to Active"). Building only on-chain top-up does not enable that
beat without a frontend, and the frontend is itself one-shot scope on
day 13.

The two endpoints exposed today are subscription-layer only and stable:

* `POST /subscriptions/{id}/top-up`
* `GET  /subscriptions/{id}/computed-status`

Neither depends on the x402 `PaySession` ADR landing. The day-8 GO
decision still applies for **adding** x402-specific endpoints
(`open_session`, `close_session`, `settle_usage`); those will be additive
and live alongside the existing two routes without changing them.

Reference: [`docs/reviews/adr-007-review-2026-05-05.md`](../../docs/reviews/adr-007-review-2026-05-05.md) §BLK-007-MAJ-4.

## Endpoints

| Method | Path                                              | Body                  |
|--------|---------------------------------------------------|-----------------------|
| GET    | `/healthz`                                        | —                     |
| POST   | `/subscriptions/{sub_pda}/top-up`                 | `{ "amount": u64 }`   |
| GET    | `/subscriptions/{sub_pda}/computed-status`        | —                     |

Full schema: [`OPENAPI.yaml`](./OPENAPI.yaml).

## Run locally

```bash
# Default config (devnet RPC, port 8080 loopback, hardcoded program ID).
# NAKAMA_FACILITATOR_API_KEY is REQUIRED — the binary refuses to start
# without it (ADR-015 §F3). Generate one with `openssl rand -hex 32`.
NAKAMA_FACILITATOR_API_KEY=$(openssl rand -hex 32) \
NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN=1 \
  cargo run -p nakama-x402-facilitator < ~/.config/solana/id.json
```

Available env vars:

* `NAKAMA_FACILITATOR_API_KEY` — **REQUIRED**. Shared bearer token enforced
  on all protected routes (`/top-up`, `/computed-status`). The facilitator
  refuses to start without one (ADR-015 §F3 fail-closed gate).
* `NAKAMA_FACILITATOR_MAX_TOP_UP_AMOUNT` — hard cap on `/top-up.amount`
  (default `1_000_000_000` = $1000 USDC). Requests above are rejected with
  400 before any RPC fetch.
* `NAKAMA_FACILITATOR_ALLOW_PUBLIC_BIND` — set to `1` to opt-in to a
  non-loopback bind addr. Without it the facilitator refuses to bind
  anywhere except `127.0.0.1` / `::1`.
* `NAKAMA_RPC_URL` (default: `https://api.devnet.solana.com`)
* `NAKAMA_BIND_ADDR` (default: `127.0.0.1:8080`)
* `NAKAMA_PROGRAM_ID` (default: hardcoded devnet program ID per `CLAUDE.md`)
* `NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN` — when set, reads a 64-byte JSON
  array (Solana CLI keypair format) from stdin at startup. Without it the
  facilitator runs in **assemble-only mode** — `/top-up` returns 503
  `signing_unavailable`, `/computed-status` works.

### Auth scheme

Protected routes require `Authorization: Bearer <API_KEY>`. `/healthz` is
open by design (orchestrator liveness probe).

Test the gate:

```bash
# Wrong / missing token → 401.
curl -sf -o /dev/null -w '%{http_code}\n' \
  http://localhost:8080/subscriptions/$SUB_PDA/computed-status
# → 401

# With the right key → 200 (or 404 if PDA doesn't exist).
curl -s -H "Authorization: Bearer $NAKAMA_FACILITATOR_API_KEY" \
  http://localhost:8080/subscriptions/$SUB_PDA/computed-status
```

Rotation: stop the binary, set a new value, restart. Multi-key /
key-rotation strategy is deferred to `future-work.md`.

## Demo curl

After the keeper has driven the subscription into `GracePeriod`:

```bash
SUB_PDA=...                                      # from your subscribe tx output
AUTH="Authorization: Bearer $NAKAMA_FACILITATOR_API_KEY"

# 1. Inspect current state.
curl -s -H "$AUTH" http://localhost:8080/subscriptions/$SUB_PDA/computed-status | jq .
# → { "state": "InGrace", "grace_until": 1715000000, "seconds_remaining": 600000 }

# 2. Top up 1 USDC (= 1_000_000 base units).
curl -sX POST -H "$AUTH" -H 'content-type: application/json' \
  -d '{"amount": 1000000}' \
  http://localhost:8080/subscriptions/$SUB_PDA/top-up | jq .
# → { "tx_signature": "5xK...4f" }

# 3. Verify state flipped back to Active.
curl -s -H "$AUTH" http://localhost:8080/subscriptions/$SUB_PDA/computed-status | jq .
# → { "state": "Active", "unlocked_pct": 30, "claimable": 14000 }
```

## Error model

```json
{ "error": "amount must be greater than zero", "code": "bad_request" }
```

| `code`                | HTTP status | Notes                                          |
|-----------------------|-------------|------------------------------------------------|
| `bad_request`         | 400         | Off-chain pre-validation failed (incl. `amount > max_top_up_amount`) |
| `unauthorized`        | 401         | Missing / wrong `Authorization: Bearer <key>` (ADR-015 §F3) |
| `not_found`           | 404         | Subscription PDA absent, or RPC-returned account has wrong owner / discriminator (ADR-015 §F5) |
| `signing_unavailable` | 503         | Facilitator started without a demo keypair     |
| `decode_error`        | 502         | On-chain account bytes don't match expected layout |
| `rpc_error`           | 502         | Solana RPC failure (no retry-with-backoff)     |
| `internal_error`      | 500         | Programmer error                               |

## What this is NOT

* **Production facilitator.** No retry-with-backoff, no rate-limiting, no
  facilitator HA — deferred per `.claude/rules/out-of-scope.md` and
  `CLAUDE.md` "Anti-perfectionism guard".
* **Wallet replacement.** Demo signing model only. Production parity
  requires wallet-adapter signed-tx pass-through (out of scope ADR-007).
* **A frontend.** The frontend connects directly to RPC; this binary
  serves only the two endpoints needed for end-to-end CLI demos.

## ADR alignment

* ADR-007 §HTTP API surface — endpoints + signing model.
* ADR-007 §"Off-chain ComputedStatus derive" — boundary contract.
* `.claude/rules/out-of-scope.md` — anti-perfectionism scope.
* `CLAUDE.md` — `Pubkey` v3 type-alias guidance, hardcoded program ID,
  `~/.config/solana/id.json` wallet convention.
