/**
 * Off-chain `ComputedStatus` derive (ADR-007 §"Off-chain ComputedStatus
 * derive" boundary contract).
 *
 * This MUST stay byte-equivalent in dispatch logic to the Rust impl in
 * `crates/nakama-client/src/computed_status.rs` and to any x402
 * facilitator port. ADR-007 commits all three sites to the same dispatch
 * — keeper bot, indexer, x402 facilitator. DRY-risk noted in ADR-007
 * §3 ("слабые места" item 3); shared crate deferred post-MVP.
 *
 * Inputs:
 *   - `subscription`: decoded Subscription account (state byte source of truth)
 *   - `graced`: GracedSubscription satellite OR null (presence is part of dispatch)
 *   - `paused`: PausedSubscription satellite OR null (ADR-006; null in this cycle)
 *   - `now`: i64 unix timestamp; caller passes `Math.floor(Date.now()/1000)`
 *     OR `await connection.getSlot` → `getBlockTime` for chain-time accuracy.
 *
 * @see ADR-007 §Decision (passive expiry contract)
 * @see ADR-007 §"Per-state eligibility table"
 */

import BN from "bn.js";

import {
  GracedSubscriptionAccount,
  PausedSubscriptionAccount,
  SubscriptionAccount,
  SubscriptionState,
} from "./types";

/**
 * Computed status surfaced to UI / keeper / x402 facilitator.
 *
 * `Active` / `ActiveLowFunds` are the only two derived states for `state == Active`.
 * `InGrace` / `GraceExpired` are the only two derived states for `state == GracePeriod`,
 * and the split is purely time-based against satellite `grace_until`.
 * `Cancelled` / `Exhausted` mirror state byte directly.
 *
 * The `Corrupt` variant fires only when the on-chain state byte and the
 * satellite presence disagree (e.g. state == GracePeriod but no satellite
 * found). Off-chain only — never written to chain. Surface to operator.
 *
 * `unlockedPct` is an INTEGER in `[0, 100]` — mirrors Rust `derive_status`'s
 * `u8` so JSON output is byte-equivalent across keeper / indexer / x402
 * facilitator (BLK-007-MAJ-2). Not a fraction in [0, 1].
 */
export type ComputedStatus =
  | { kind: "Active"; unlockedPct: number; claimable: bigint }
  | {
      kind: "ActiveLowFunds";
      unlockedPct: number;
      claimable: bigint;
      // BLK-007-MAJ-3 — runway gate + daysRemaining payload mirrors Rust derive_status
      daysRemaining: number;
    }
  | { kind: "Paused" }
  | { kind: "InGrace"; graceUntil: bigint; secondsRemaining: bigint }
  | { kind: "GraceExpired"; graceUntil: bigint }
  | { kind: "Cancelled" }
  | { kind: "Exhausted" }
  | { kind: "Corrupt"; reason: string };

/**
 * Threshold from ADR-007 derive_status — `> 80` (integer percent) flips to
 * ActiveLowFunds. Strict inequality: `unlockedPct == 80` stays `Active`.
 */
const LOW_FUNDS_UTILIZATION_THRESHOLD = 80;

/**
 * Runway threshold mirroring Rust `ACTIVE_LOW_FUNDS_DAYS = 7`. Strict
 * inequality: `daysRemaining == 7` stays `Active`.
 */
const ACTIVE_LOW_FUNDS_DAYS = 7;

/** Mirrors Rust `SECONDS_PER_DAY` constant. */
const SECONDS_PER_DAY = 86_400n;

/**
 * Sentinel for "effectively infinite" runway when `ratePerSecond == 0`.
 * Mirrors Rust `u32::MAX`. Capped at `Number.MAX_SAFE_INTEGER` semantics in
 * JS, but we use `0xFFFF_FFFF` to keep the JSON payload byte-equivalent to
 * the Rust facilitator response.
 */
const DAYS_REMAINING_SENTINEL = 0xffff_ffff;

/**
 * Compute `(unlocked, claimable, utilization, daysRemaining)` for an Active
 * subscription.
 *
 * F4-mirror (ADR-015 §F4 "Lazy precise unlock math"): the canonical
 * formula is now lazy precise division using the snapshotted
 * `(price, period)` pair on Subscription:
 *
 *   unlocked   = min(deposited, (elapsed * price) / period)
 *   claimable  = unlocked - withdrawn
 *
 * The previous form `rate_per_second * elapsed` under-paid the merchant
 * by `(price mod period) / period` base units per second of accrual —
 * up to ~22% on plans where `price < period`. The on-chain math in
 * `charge.rs` / `cancel.rs` / `settle_usage.rs` is being migrated to
 * the same formula in the same ADR cycle (anchor-engineer); this
 * mirror keeps off-chain derive byte-equivalent.
 *
 * `Subscription.ratePerSecond` field is retained on-chain for indexer
 * ergonomics and display (ADR-015 §F4 "rate_per_second is kept for
 * off-chain consumers ... but no longer authoritative for unlock
 * math"). Runway calculation here ALSO migrates to (price, period) so
 * the entire derive uses one source of truth.
 *
 * BigInt arithmetic — never `Number(...)` until final display step.
 * BigInt division in JS is floor toward zero for non-negative operands;
 * matches Rust `u128 / u128` semantics.
 *
 * `unlockedPct` is INTEGER 0..=100, mirroring Rust `derive_status`'s `u8`
 * (BLK-007-MAJ-2 — pin boundary contract: cross-language byte-equivalence).
 *
 * `daysRemaining` is INTEGER days of runway from `(price, period)`.
 * Sentinel `0xFFFF_FFFF` when `price == 0` (defensive — production plans
 * reject zero-price at subscribe).
 */
function computeActiveAccrual(
  sub: SubscriptionAccount,
  now: bigint,
): { unlockedPct: number; claimable: bigint; daysRemaining: number } {
  const deposited = BigInt(sub.depositedAmount.toString());
  const withdrawn = BigInt(sub.withdrawnAmount.toString());
  const price = BigInt(sub.price.toString());
  const period = BigInt(sub.period.toString());
  const streamStart = BigInt(sub.streamStart.toString());

  // Clock-skew defence (ADR-002 §cancel step 3): if validator clock moved
  // backwards relative to stream_start, treat elapsed as 0.
  const elapsed = now > streamStart ? now - streamStart : 0n;

  // F4-mirror canonical formula. Guard period == 0 defensively (on-chain
  // `InvalidPeriod` guard rejects this at subscribe, but a corrupt
  // satellite-state read shouldn't crash the derive).
  const accrued = period > 0n ? (elapsed * price) / period : 0n;
  const unlocked = accrued < deposited ? accrued : deposited;
  const claimable = unlocked > withdrawn ? unlocked - withdrawn : 0n;

  // Utilization = floor(withdrawn * 100 / deposited), clamped to 100. Avoid
  // div-by-zero on freshly subscribed accounts (deposited == 0 should never
  // happen post-subscribe but defensive default = 0%).
  // BLK-007-MAJ-2 — pin boundary contract: cross-language byte-equivalence.
  const utilization =
    deposited === 0n
      ? 0
      : Math.min(100, Math.floor(Number((withdrawn * 100n) / deposited)));

  // Runway: remaining liquid balance / rate, in days. F4-mirror — derive
  // rate from (price, period) inline rather than reading the now-advisory
  // `rate_per_second` snapshot. Formula:
  //   days_of_runway = (remaining_liquid * period) / (price * SECONDS_PER_DAY)
  // Algebraically equivalent to `remaining_liquid / rate / SECONDS_PER_DAY`
  // with exact-arithmetic semantics (no rate truncation).
  const remainingLiquid = deposited > withdrawn ? deposited - withdrawn : 0n;
  const daysRemaining =
    price === 0n
      ? DAYS_REMAINING_SENTINEL
      : Number((remainingLiquid * period) / (price * SECONDS_PER_DAY));

  return { unlockedPct: utilization, claimable, daysRemaining };
}

/**
 * Derive the user-facing computed status from on-chain state + clock.
 *
 * Conventions (mirrored from Rust `derive_status` for cross-language
 * byte-equivalence — ADR-007 boundary contract, BLK-007-MAJ-2/3):
 *  - `unlockedPct`: integer 0..=100 (NOT a fraction).
 *  - `daysRemaining` (ActiveLowFunds only): integer days at snapshotted
 *    `ratePerSecond`. Sentinel `0xFFFF_FFFF` if rate is zero.
 *  - `ActiveLowFunds` fires on EITHER `unlockedPct > 80` OR
 *    `daysRemaining < 7` (strict; both boundaries inclusive of `Active`).
 *
 * @example
 * ```ts
 * const sub = await program.account.subscription.fetch(subPda);
 * const graced = await program.account.gracedSubscription.fetchNullable(gracedPda);
 * const status = deriveStatus(sub, graced, null, BigInt(Math.floor(Date.now()/1000)));
 * if (status.kind === "InGrace") {
 *   console.log(`Grace expires in ${status.secondsRemaining}s`);
 * }
 * if (status.kind === "ActiveLowFunds") {
 *   console.log(`Low funds: ${status.unlockedPct}% used, ${status.daysRemaining}d left`);
 * }
 * ```
 */
export function deriveStatus(
  subscription: SubscriptionAccount,
  graced: GracedSubscriptionAccount | null,
  paused: PausedSubscriptionAccount | null,
  now: bigint,
): ComputedStatus {
  switch (subscription.state) {
    case SubscriptionState.Active: {
      const { unlockedPct, claimable, daysRemaining } = computeActiveAccrual(
        subscription,
        now,
      );
      // BLK-007-MAJ-3 — runway gate + daysRemaining payload mirrors Rust derive_status.
      // Either gate fires: utilization > 80 OR runway < 7 days. Strict inequalities.
      const utilizationLow = unlockedPct > LOW_FUNDS_UTILIZATION_THRESHOLD;
      const runwayLow = daysRemaining < ACTIVE_LOW_FUNDS_DAYS;
      if (utilizationLow || runwayLow) {
        return {
          kind: "ActiveLowFunds",
          unlockedPct,
          claimable,
          daysRemaining,
        };
      }
      return { kind: "Active", unlockedPct, claimable };
    }

    case SubscriptionState.Paused: {
      // ADR-006: PausedSubscription satellite expected; absence = corrupt.
      // Stage-2 of ADR-007 cycle does not implement ADR-006, so we soft-tolerate
      // null paused (paused state is unreachable in MVP cycle-3 lifecycle).
      void paused;
      return { kind: "Paused" };
    }

    case SubscriptionState.GracePeriod: {
      if (graced === null) {
        // State byte says GracePeriod but no satellite — off-chain anomaly.
        return {
          kind: "Corrupt",
          reason:
            "Subscription.state == GracePeriod but no GracedSubscription satellite found",
        };
      }
      const graceUntil = BigInt(graced.graceUntil.toString());
      if (now <= graceUntil) {
        return {
          kind: "InGrace",
          graceUntil,
          secondsRemaining: graceUntil - now,
        };
      }
      // State byte stale, time elapsed — passive expiry per ADR-007 Decision.
      return { kind: "GraceExpired", graceUntil };
    }

    case SubscriptionState.Cancelled:
      return { kind: "Cancelled" };

    case SubscriptionState.Exhausted:
      return { kind: "Exhausted" };

    default:
      // Should be unreachable given enum exhaustiveness; guard against
      // future variant added on-chain that pre-dates this client.
      return {
        kind: "Corrupt",
        reason: `Unknown SubscriptionState byte: ${subscription.state}`,
      };
  }
}

/**
 * Helper: convert Anchor's `BN`-shaped Subscription account return into the
 * SDK's typed `SubscriptionAccount`. Anchor returns `state` as a tagged
 * variant `{ active: {} }` etc — normalised here.
 *
 * Caller passes the raw return of `program.account.subscription.fetch(...)`.
 */
export function normalizeSubscriptionAccount(
  raw: {
    nextChargeAt: BN;
    subscriber: import("@solana/web3.js").PublicKey;
    plan: import("@solana/web3.js").PublicKey;
    price: BN;
    period: BN;
    tokenMint: import("@solana/web3.js").PublicKey;
    merchant: import("@solana/web3.js").PublicKey;
    merchantAta: import("@solana/web3.js").PublicKey;
    state: unknown;
    bump: number;
    vaultBump: number;
    createdAt: BN;
    lastChargeAt: BN;
    depositedAmount: BN;
    withdrawnAmount: BN;
    ratePerSecond: BN;
    streamStart: BN;
    reserved: number[];
  },
): SubscriptionAccount {
  // Late-binding import to avoid circular module init.
  const { decodeSubscriptionState } = require("./types") as typeof import("./types");
  const state = decodeSubscriptionState(raw.state);
  if (state === null) {
    throw new Error(
      `Cannot decode SubscriptionState — raw value: ${JSON.stringify(raw.state)}`,
    );
  }
  return {
    ...raw,
    state,
  };
}

/*
 * ─────────────────────────────────────────────────────────────────────────
 * Doctest cases (machine-readable; no test runner configured in this
 * workspace per `package.json`). Each case is the off-chain mirror of a
 * Rust unit test in `crates/nakama-client/src/computed_status.rs`. If a
 * test runner lands later, port these to mocha/vitest verbatim.
 * BLK-007-MAJ-2 / MAJ-3 boundary cases pinned here.
 *
 * F4-mirror (ADR-015 §F4): all cases below use the canonical
 * `(price, period)` snapshot. The previous form referenced
 * `rate_per_second` which is now advisory only.
 * ─────────────────────────────────────────────────────────────────────────
 *
 * Case 1 — Active, full runway (effective rate = 1 base unit/sec):
 *   sub: { state: Active, deposited: 1_000_000, withdrawn: 0,
 *          price: 86_400, period: 86_400, streamStart: 0 }
 *   now: 100n
 *   → { kind: "Active", unlockedPct: 0, claimable: 100n }
 *   ((100 * 86_400) / 86_400 = 100 unlocked; runway = 1_000_000 / 86_400
 *    ≈ 11 days → no low-funds gate fires)
 *
 * Case 2 — ActiveLowFunds via runway:
 *   sub: { state: Active, deposited: 86_400, withdrawn: 0,
 *          price: 86_400, period: 86_400, streamStart: 0 }
 *   now: 0n
 *   → { kind: "ActiveLowFunds", unlockedPct: 0, claimable: 0n, daysRemaining: 1 }
 *   (1 day runway < 7 → runway gate fires)
 *
 * Case 3 — ActiveLowFunds via utilization:
 *   sub: { state: Active, deposited: 100_000_000, withdrawn: 85_000_000,
 *          price: 86_400, period: 86_400, streamStart: 0 }
 *   now: 0n
 *   → { kind: "ActiveLowFunds", unlockedPct: 85, claimable: 0n, daysRemaining: 173 }
 *   (15_000_000 / 86_400 ≈ 173 days → runway OK; utilization 85 > 80 → fires)
 *
 * Case 4 — F4 precision proof (the key new test):
 *   sub: { state: Active, deposited: 10_000_000, withdrawn: 0,
 *          price: 10_000_000, period: 2_592_000, streamStart: 0 }
 *   now: 2_592_000n (full 30-day period elapsed)
 *   → unlocked = (2_592_000 * 10_000_000) / 2_592_000 = 10_000_000 (full period)
 *   Pre-F4 (rate_per_second = 10_000_000 / 2_592_000 = 3 truncated):
 *     unlocked_old = 3 * 2_592_000 = 7_776_000 → under-pay by ~22%.
 *   F4 mirror reproduces the on-chain post-fix value exactly.
 *
 * Case 5 — Zero deposited (defensive):
 *   sub: { state: Active, deposited: 0, withdrawn: 0,
 *          price: 86_400, period: 86_400, streamStart: 0 }
 *   now: 100n
 *   → { kind: "ActiveLowFunds", unlockedPct: 0, claimable: 0n, daysRemaining: 0 }
 *   (remaining_liquid = 0 → runway gate fires; utilization clamped to 0)
 */
