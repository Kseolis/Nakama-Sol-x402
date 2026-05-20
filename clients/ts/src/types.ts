/**
 * Shared TS types for the Nakama SDK.
 *
 * Permanent `Idl`-alias trade-off (see structural note on `Nakama` below).
 * The canonical Anchor-generated `Nakama` IDL type lives at
 * `nakama/target/types/nakama.ts` and IS up-to-date with every shipped
 * ADR (top_up, cancel, GracedSubscription, etc). This module deliberately
 * does NOT import it — `rootDir` cannot reach outside `clients/ts/`
 * without breaking the published `dist/` layout. Local mirrors below are
 * byte-equivalent to the on-chain layout per the cited ADRs.
 *
 *   - `SubscriptionState` enum (ADR-003 frozen byte spec)
 *   - `SubscriptionAccount` (post-BLK-05 canonical field names)
 *   - `GracedSubscriptionAccount` (ADR-007 layout)
 *   - `PausedSubscriptionAccount` (ADR-006 layout, partial)
 *
 * @see ../../../nakama/target/types/nakama.ts — canonical IDL types (not imported)
 * @see ../../../nakama/programs/nakama/src/state.rs — on-chain source of truth
 */

import type { Idl } from "@anchor-lang/core";
import { PublicKey } from "@solana/web3.js";
import BN from "bn.js";

// IDL type alias — STRUCTURAL trade-off, not a TODO.
//
// `Nakama` is permanently aliased to Anchor's structural `Idl` type
// because importing the generated `nakama/target/types/nakama.ts` would
// require expanding tsconfig `rootDir` outside `clients/ts/`, which
// breaks the published `dist/` layout under `tsc --build`. This is the
// reason every `program.methods.<ix>(...)` call site inside the SDK
// carries a localised `as any` cast on `program.methods` — the casts
// are NOT IDL-staleness workarounds; the IDL itself is current.
//
// Future split (out of scope for hackathon): publish a separate
// `@nakama/idl-types` package that re-exports the generated `Nakama`
// type. SDK consumers can then either:
//   - import `Nakama` from `@nakama/idl-types`, or
//   - import their own copy of the generated IDL types and pass the
//     resulting `Program<Nakama>` to our builders.
//
// Both routes are wire-compatible with the current `Program<Idl>`
// surface; the change is purely about static type sharpness.
export type Nakama = Idl;

/**
 * SubscriptionState FSM byte (ADR-003).
 *
 * Discriminants are byte-stable: must NOT shift across program redeploys
 * — ADR-001 §`#[non_exhaustive]` scope clarification.
 *
 * MVP writes only `Active = 0` and `Cancelled = 4`. Post-MVP variants
 * (`Paused`, `GracePeriod`, `Exhausted`) become reachable as their ADRs
 * land — ADR-006, ADR-007, future cleanup ADR.
 */
export enum SubscriptionState {
  Active = 0,
  Paused = 1,
  GracePeriod = 2,
  Exhausted = 3,
  Cancelled = 4,
}

/**
 * Decode a state byte from an on-chain Subscription account into the TS
 * enum. Anchor returns the state field as `{ active: {} }` /
 * `{ gracePeriod: {} }` (camelCase variant tags) when fetching via
 * `program.account.subscription.fetch(...)`. This helper normalises that
 * to the byte-stable enum.
 *
 * Unknown variants return `null` so callers can surface a corrupt-state
 * error instead of crashing.
 */
export function decodeSubscriptionState(raw: unknown): SubscriptionState | null {
  if (typeof raw !== "object" || raw === null) return null;
  const tag = Object.keys(raw)[0];
  switch (tag) {
    case "active":
      return SubscriptionState.Active;
    case "paused":
      return SubscriptionState.Paused;
    case "gracePeriod":
      return SubscriptionState.GracePeriod;
    case "exhausted":
      return SubscriptionState.Exhausted;
    case "cancelled":
      return SubscriptionState.Cancelled;
    default:
      return null;
  }
}

/**
 * In-memory shape of a Subscription account, decoded.
 *
 * Field names mirror the on-chain Borsh layout exactly (post-BLK-05
 * canonical names: `deposited_amount`, `withdrawn_amount`,
 * `rate_per_second`, `stream_start`). See `state.rs:Subscription`.
 *
 * Anchor's TS client returns `BN` for u64/i64 — we keep that shape.
 */
export interface SubscriptionAccount {
  nextChargeAt: BN;
  subscriber: PublicKey;
  plan: PublicKey;
  price: BN;
  period: BN;
  tokenMint: PublicKey;
  merchant: PublicKey;
  merchantAta: PublicKey;
  state: SubscriptionState;
  bump: number;
  vaultBump: number;
  createdAt: BN;
  lastChargeAt: BN;
  depositedAmount: BN;
  withdrawnAmount: BN;
  ratePerSecond: BN;
  streamStart: BN;
  reserved: number[]; // [u8; 32]
}

/**
 * In-memory shape of a GracedSubscription satellite account (ADR-007).
 *
 * Layout: 8 (disc) + 32 (subscription) + 8 (entered_grace_at) +
 * 8 (grace_until) = 56 bytes on-chain.
 */
export interface GracedSubscriptionAccount {
  subscription: PublicKey;
  enteredGraceAt: BN;
  graceUntil: BN;
}

/**
 * In-memory shape of a PausedSubscription satellite (ADR-006).
 *
 * Currently unused in ADR-007 cycle; included as `null`-only optional
 * input to `deriveStatus` for forward-compat with ADR-006 ship.
 */
export interface PausedSubscriptionAccount {
  subscription: PublicKey;
  pausedAt: BN;
  // Other fields TBD by ADR-006 stage-2.
}
