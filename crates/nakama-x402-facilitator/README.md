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
# Default config (devnet RPC, port 8080, hardcoded program ID).
NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN=1 \
  cargo run -p nakama-x402-facilitator < ~/.config/solana/id.json
```

Available env vars:

* `NAKAMA_RPC_URL` (default: `https://api.devnet.solana.com`)
* `NAKAMA_BIND_ADDR` (default: `0.0.0.0:8080`)
* `NAKAMA_PROGRAM_ID` (default: hardcoded devnet program ID per `CLAUDE.md`)
* `NAKAMA_READ_DEMO_KEYPAIR_FROM_STDIN` — when set, reads a 64-byte JSON
  array (Solana CLI keypair format) from stdin at startup. Without it the
  facilitator runs in **assemble-only mode** — `/top-up` returns 503
  `signing_unavailable`, `/computed-status` works.

## Demo curl

After the keeper has driven the subscription into `GracePeriod`:

```bash
SUB_PDA=...   # from your subscribe tx output

# 1. Inspect current state.
curl -s http://localhost:8080/subscriptions/$SUB_PDA/computed-status | jq .
# → { "state": "InGrace", "grace_until": 1715000000, "seconds_remaining": 600000 }

# 2. Top up 1 USDC (= 1_000_000 base units).
curl -sX POST -H 'content-type: application/json' \
  -d '{"amount": 1000000}' \
  http://localhost:8080/subscriptions/$SUB_PDA/top-up | jq .
# → { "tx_signature": "5xK...4f" }

# 3. Verify state flipped back to Active.
curl -s http://localhost:8080/subscriptions/$SUB_PDA/computed-status | jq .
# → { "state": "Active", "unlocked_pct": 30, "claimable": 14000 }
```

## Error model

```json
{ "error": "amount must be greater than zero", "code": "bad_request" }
```

| `code`                | HTTP status | Notes                                          |
|-----------------------|-------------|------------------------------------------------|
| `bad_request`         | 400         | Off-chain pre-validation failed                |
| `not_found`           | 404         | Subscription PDA not found via RPC             |
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
