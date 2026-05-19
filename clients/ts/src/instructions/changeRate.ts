/**
 * `changeRate` composite-tx builder (ADR-005 — Variable Rate / Mid-Subscription
 * Price Change via Composition).
 *
 * ADR-005 is **POST-HACKATHON, documentational**. There is **ZERO on-chain
 * change** — no new instruction, no new error variant, no new event. Rate
 * change ≡ canonical composite sequence
 *
 *   [close_session × N?, cancel(old_sub), cleanup(old_sub), subscribe(plan_v2)]
 *
 * assembled in one atomic transaction. Subscriber signs. Plan v2 is just
 * another Plan PDA created via standard `create_plan`. Old Plan stays
 * immutable (ADR-001 invariant).
 *
 * This builder is a **thin Controller** on top of the existing primitives:
 *   - `buildResubscribeIxs` (ADR-008) already composes
 *     `close_session × N + cleanup + subscribe`. The migration tx adds a
 *     `cancel(old_sub)` prefix to that — `cleanup` from Cancelled tombstone
 *     stays valid (ADR-013).
 *   - `findAlivePaySessions` (ADR-008) enumerates alive PaySession
 *     satellites under the OLD subscription. Reused as-is.
 *   - `buildCancelIx` (ADR-009 / ADR-013) builds the polymorphic `cancel`
 *     ix; here the signer is always the subscriber (ADR-005 Q1).
 *
 * GRASP roles:
 *   - `buildChangeRateTx` — Controller. Owns the RPC fetches (Plan v1/v2,
 *     old Subscription, alive PaySessions), invariant gates, and ix
 *     assembly delegation.
 *
 * @see ADR-005 §Decision (composition over mutation)
 * @see ADR-005 §Q1   (subscriber-initiated only)
 * @see ADR-005 §Q5   (same-mint only)
 * @see ADR-005 §Q7   (x402 PaySession pre-scan)
 * @see ADR-005 §Q8   (state matrix — Active / Paused / Grace / Cancelled / Exhausted)
 * @see ADR-005 §Q11  (fresh-subscribe fallback when old_sub absent)
 * @see ADR-005 §"SDK composition contract"
 * @see ADR-008      (composite-tx primitive being extended)
 * @see ADR-013      (cancel + cleanup split, settle/refund flow)
 * @see ADR-015 §F5  (owner-check trust boundary for RPC reads)
 */

import { Program } from "@anchor-lang/core";
import {
  Connection,
  PublicKey,
  Transaction,
  TransactionInstruction,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";

import { Nakama, SubscriptionState, decodeSubscriptionState } from "../types";
import { deriveSubscriptionPda, deriveVaultPda } from "../pdas";
import {
  AccountFetchError,
  decodeProgramOwnedAccount,
  fetchProgramOwnedAccountNullable,
} from "../accounts";
import { buildCancelIx } from "./cancel";
import { findAlivePaySessions, AlivePaySession } from "./resubscribe";

// ─────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────

/**
 * Soft cap on alive PaySession satellites the migration composite will
 * close in one transaction. Mirrors ADR-008's cap (N ≤ 4) since the
 * migration tx is strictly larger than re-subscribe (adds `cancel` prefix);
 * the 1232-byte envelope tightens, not loosens. ADR-005 §Q4 tx-size
 * analysis allows up to ~5 satellites without `cancel`; we keep 4 as
 * conservative.
 *
 * @see ADR-005 §Q4 "Tx-size budget"
 * @see ADR-008 §"x402 forward-compat"
 */
const MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE = 4;

// ─────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────

/**
 * Typed error codes surfaced by `buildChangeRateTx`. These are SDK-level
 * gates (off-chain, ADR-005 invariants); they are NOT new on-chain error
 * variants. Throw shape is `Error` whose `.message` starts with one of
 * these codes so callers can pattern-match without coupling to a class.
 */
export type ChangeRateError =
  | "CrossMintMigrationUnsupported"
  | "TooManyAliveSessions"
  | "OldSubscriptionNotFound"
  | "NewPlanInactive";

/**
 * Inputs for `buildChangeRateTx`.
 */
export interface ChangeRateOptions {
  /** Anchor `Program<Nakama>` from the canonical IDL. */
  program: Program<Nakama>;
  /** Subscriber wallet — single signer for the entire composite tx. */
  subscriber: PublicKey;
  /** Old Plan PDA (the one being migrated FROM). */
  oldPlan: PublicKey;
  /** New Plan PDA (the one being migrated TO). Must share the same mint. */
  newPlan: PublicKey;
  /**
   * Prefund period count for the new subscription, forwarded to
   * `subscribe(periods_to_prefund: u8)`. MUST be in `1..=255`.
   */
  newDepositPeriods: number;
  /**
   * If `true` (default), pre-scan alive PaySession satellites under the
   * OLD subscription and prepend `close_session × N`. ADR-005 §Q7.
   * Set `false` to accept orphan PaySession rent lockup (R1 graceful
   * degradation per ADR-x402-001).
   */
  closeAlivePaySessions?: boolean;
}

// Minimal structural shape of the decoded Plan account we read. We only
// touch `tokenMint` (ADR-005 Q5 same-mint check) and `merchantAta` (needed
// for the cancel ix). Field names are camelCase to match Anchor's
// `program.coder.accounts.decode("Plan", data)` output.
interface DecodedPlan {
  tokenMint: PublicKey;
  merchantAta: PublicKey;
}

// Minimal structural shape of the decoded Subscription account fields we
// need for the cancel ix (`merchantAta`, `subscriberAta`-equivalent via
// snapshot, and the FSM state). Mirrors `state.rs:Subscription` byte
// layout.
interface DecodedSubscription {
  merchantAta: PublicKey;
  subscriber: PublicKey;
  tokenMint: PublicKey;
  state: SubscriptionState;
}

// ─────────────────────────────────────────────────────────────────────────
// Public API — Controller
// ─────────────────────────────────────────────────────────────────────────

/**
 * Build the composite migration transaction for a subscriber moving from
 * `oldPlan` to `newPlan` on the same token mint.
 *
 * Returns an unsigned `Transaction`. The caller is responsible for
 * setting `feePayer`, `recentBlockhash`, and signing. Atomicity is a
 * Solana runtime primitive — if any instruction fails, the entire tx
 * reverts and the old subscription survives unchanged (ADR-005 §E2).
 *
 * Invariants enforced (all surface as `Error` with a typed
 * `ChangeRateError` code prefix in `.message`):
 *  1. **Q5 same-mint** — `oldPlan.tokenMint === newPlan.tokenMint`,
 *     otherwise `CrossMintMigrationUnsupported`.
 *  2. **Q7 x402 pre-scan** — alive PaySession PDAs under the OLD
 *     subscription are enumerated when `closeAlivePaySessions !== false`,
 *     soft-capped at 4 (`TooManyAliveSessions` otherwise).
 *  3. **Q11 fresh-subscribe fallback** — if the OLD subscription PDA does
 *     not exist (e.g. already cleaned up by an earlier tx), the helper
 *     STILL needs the OLD plan only for the same-mint check. The
 *     composite collapses to a plain `subscribe(newPlan)` ix. (Surface
 *     the absence to the caller via the `OldSubscriptionNotFound` debug
 *     hint in the `Error` thrown when the caller asked for satellites
 *     close on a non-existent subscription — see `closeAlivePaySessions`
 *     behaviour below.)
 *  4. **Q1 subscriber-only** — the cancel ix is always built with
 *     `signer = subscriber`. Merchant-initiated migration is not a thing
 *     in ADR-005.
 *
 * F5 (ADR-015): every RPC fetch goes through `decodeProgramOwnedAccount`
 * / `fetchProgramOwnedAccountNullable` — owner check ALWAYS runs before
 * Borsh decode. `getProgramAccounts` filter (inside `findAlivePaySessions`)
 * includes a positive 8-byte Anchor discriminator memcmp at offset 0.
 *
 * @example
 * ```ts
 * const tx = await buildChangeRateTx(program, {
 *   subscriber: wallet.publicKey,
 *   oldPlan: planV1Pda,
 *   newPlan: planV2Pda,
 *   newDepositPeriods: 2,
 * });
 * tx.feePayer = wallet.publicKey;
 * tx.recentBlockhash = (await connection.getLatestBlockhash()).blockhash;
 * tx.sign(wallet);
 * const sig = await connection.sendRawTransaction(tx.serialize());
 * ```
 *
 * @throws Error prefixed with `CrossMintMigrationUnsupported` when mints differ.
 * @throws Error prefixed with `TooManyAliveSessions` when N > 4 satellites.
 * @throws Error prefixed with `NewPlanInactive` if the NEW plan account is
 *   missing / wrong-owner / un-decodable (collapses "no such Plan v2" and
 *   "spoofed Plan v2" into one actionable code).
 */
export async function buildChangeRateTx(
  program: Program<Nakama>,
  opts: ChangeRateOptions,
): Promise<Transaction> {
  // ── Provider / connection plumbing ────────────────────────────────────
  const connection = getConnection(program);

  if (opts.newDepositPeriods < 1 || opts.newDepositPeriods > 255) {
    throw new Error(
      `newDepositPeriods must be in 1..=255 (u8 on-chain); got ${opts.newDepositPeriods}.`,
    );
  }

  // ── Fetch Plans (F5 owner-check on every RPC read) ────────────────────
  // ADR-005 Q5: both Plans must be readable and share the same mint.
  // Old Plan may legitimately be absent if a hypothetical future
  // `close_plan` lands (E4) — for ADR-005 MVP it cannot be closed; we
  // still tolerate `null` to keep the helper future-proof.
  const [oldPlanData, newPlanData] = await Promise.all([
    fetchPlan(connection, program, opts.oldPlan),
    fetchPlan(connection, program, opts.newPlan),
  ]);

  if (newPlanData === null) {
    // ADR-005 Q5 boundary: we cannot verify same-mint without Plan v2.
    // Collapse "no such plan" and "wrong-owner plan" into one actionable
    // code; the WrongAccountOwner case already threw inside fetchPlan.
    throw new Error(
      `NewPlanInactive: Plan v2 ${opts.newPlan.toBase58()} not found on-chain.`,
    );
  }

  if (oldPlanData !== null) {
    if (!oldPlanData.tokenMint.equals(newPlanData.tokenMint)) {
      // ADR-005 Q5: cross-mint migration is its own future ADR.
      throw new Error(
        `CrossMintMigrationUnsupported: oldPlan.tokenMint=` +
          `${oldPlanData.tokenMint.toBase58()} != newPlan.tokenMint=` +
          `${newPlanData.tokenMint.toBase58()}. ADR-005 §Q5.`,
      );
    }
  }
  // If oldPlanData === null we keep going — fresh-subscribe fallback path
  // (Q11): subscriber is effectively starting clean on Plan v2. No
  // cancel/cleanup possible without a Plan v1 anchor anyway.

  // ── Fetch OLD subscription state ──────────────────────────────────────
  const [oldSubPda] = deriveSubscriptionPda(
    program.programId,
    opts.subscriber,
    opts.oldPlan,
  );
  const oldSub = await fetchSubscription(connection, program, oldSubPda);

  // ── Q11 graceful fallback: OLD subscription absent ────────────────────
  if (oldSub === null) {
    // No old subscription to cancel/cleanup → composite collapses to a
    // single subscribe ix on the new plan. Same semantics as
    // `resubscribeOrSubscribe` returning `resubscribed=false`.
    const subscribeIx = await buildSubscribeIx(
      program,
      opts.subscriber,
      opts.newPlan,
      newPlanData.tokenMint,
      await deriveSubscriberAtaForMint(opts.subscriber, newPlanData.tokenMint),
      opts.newDepositPeriods,
    );
    return new Transaction().add(subscribeIx);
  }

  // Sanity defence-in-depth: the OLD subscription's snapshotted token
  // mint must equal Plan v1's mint. If oldPlanData was null we trust the
  // subscription snapshot directly (ADR-001 BLK-23 — Subscription carries
  // immutable Plan snapshots).
  if (!oldSub.tokenMint.equals(newPlanData.tokenMint)) {
    throw new Error(
      `CrossMintMigrationUnsupported: oldSubscription.tokenMint=` +
        `${oldSub.tokenMint.toBase58()} != newPlan.tokenMint=` +
        `${newPlanData.tokenMint.toBase58()}. ADR-005 §Q5.`,
    );
  }

  // ── Q7 pre-scan alive PaySession satellites ───────────────────────────
  const wantClose = opts.closeAlivePaySessions !== false; /* default true */
  const alivePaySessions: AlivePaySession[] = wantClose
    ? await findAlivePaySessions(connection, program, oldSubPda)
    : [];

  if (alivePaySessions.length > MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE) {
    // ADR-005 §Q4 envelope check: composite tx with cancel + cleanup +
    // subscribe + close_session × N strictly larger than ADR-008's; cap
    // at 4 to keep a margin.
    throw new Error(
      `TooManyAliveSessions: ${alivePaySessions.length} alive PaySession ` +
        `satellites detected (soft-cap ${MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE}). ` +
        `Close some out-of-band first, then retry.`,
    );
  }

  // ── Compose tx: [close_session × N, cancel, cleanup, subscribe] ──────
  const tx = new Transaction();

  // 1) close_session × N — same shape as ADR-008. We import the
  //    derivation + handler inline rather than recomposing
  //    `buildResubscribeIxs` because the migration composite needs
  //    `cancel` BEFORE `cleanup`, which `buildResubscribeIxs` does not
  //    do (it goes straight to cleanup, since re-subscribe assumes a
  //    Cancelled tombstone). Reuse is at the enumeration layer, not the
  //    full builder.
  for (const session of alivePaySessions) {
    const ix = await buildCloseSessionIxInline(
      program,
      oldSubPda,
      opts.subscriber,
      session.sessionId,
    );
    tx.add(ix);
  }

  // 2) cancel(old_sub) — ADR-013 settle+refund. Signer = subscriber per
  //    ADR-005 Q1 (subscriber-only migration). State-driven optional
  //    GracedSubscription satellite handled by `buildCancelIx` itself.
  const [vaultPda] = deriveVaultPda(program.programId, oldSubPda);
  // Resolve subscriber's ATA for the shared mint (same ATA reused for
  // refund destination and new-subscribe prefund source, ADR-005 §Q5).
  const subscriberAta = await deriveSubscriberAtaForMint(
    opts.subscriber,
    oldSub.tokenMint,
  );
  const cancelIx = await buildCancelIx({
    program,
    signer: opts.subscriber,
    subscriber: oldSub.subscriber,
    subscription: oldSubPda,
    vault: vaultPda,
    merchantAta: oldSub.merchantAta,
    subscriberAta,
    state: oldSub.state,
  });
  tx.add(cancelIx);

  // 3) cleanup(old_sub) — ADR-013 rent reclaim. Standalone ix; reuses
  //    `program.methods.cleanup()` since there is no dedicated builder
  //    helper file (the only callers so far are inline in
  //    `buildResubscribeIxs`).
  const cleanupIx = await buildCleanupIxInline(
    program,
    oldSubPda,
    opts.subscriber,
  );
  tx.add(cleanupIx);

  // 4) subscribe(plan_v2) — fresh subscription on the new rate. The
  //    refund landed in subscriberAta during `cancel`; this CPI debits
  //    it for the new prefund. Solana atomicity guarantees both happen
  //    or neither does (ADR-005 §Q4 atomicity).
  const subscribeIx = await buildSubscribeIx(
    program,
    opts.subscriber,
    opts.newPlan,
    newPlanData.tokenMint,
    subscriberAta,
    opts.newDepositPeriods,
  );
  tx.add(subscribeIx);

  return tx;
}

// ─────────────────────────────────────────────────────────────────────────
// Internal helpers — RPC fetch with F5 owner-check
// ─────────────────────────────────────────────────────────────────────────

/**
 * Fetch a Plan account (or return `null` if absent). Validates
 * `account.owner == program.programId` BEFORE Borsh decode per ADR-015 §F5.
 * Anchor's decoder validates the 8-byte discriminator internally, so we
 * pass `null` for `expectedDiscriminator`.
 */
async function fetchPlan(
  connection: Connection,
  program: Program<Nakama>,
  plan: PublicKey,
): Promise<DecodedPlan | null> {
  try {
    return await fetchProgramOwnedAccountNullable(
      connection,
      plan,
      program.programId,
      null,
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (data: Buffer) => (program.coder as any).accounts.decode("Plan", data) as DecodedPlan,
    );
  } catch (err) {
    // F5 mismatches are configuration / spoofing — re-throw verbatim.
    if (err instanceof AccountFetchError) throw err;
    throw err;
  }
}

/**
 * Fetch the OLD Subscription PDA via the owner-checked trust boundary
 * (ADR-015 §F5). Mirrors `fetchSubscriptionState` in `resubscribe.ts` but
 * also surfaces the merchant ATA + token mint snapshots needed by
 * `buildCancelIx`.
 */
async function fetchSubscription(
  connection: Connection,
  program: Program<Nakama>,
  subscriptionPda: PublicKey,
): Promise<DecodedSubscription | null> {
  const info = await connection.getAccountInfo(subscriptionPda, "confirmed");
  if (info === null) return null;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const raw = decodeProgramOwnedAccount<any>(
    info,
    program.programId,
    null,
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (data: Buffer) => (program.coder as any).accounts.decode("Subscription", data),
  );
  const state = decodeSubscriptionState(raw.state);
  if (state === null) {
    throw new Error(
      `Subscription PDA ${subscriptionPda.toBase58()} has unrecognised ` +
        `state byte: ${JSON.stringify(raw.state)}.`,
    );
  }
  return {
    merchantAta: raw.merchantAta as PublicKey,
    subscriber: raw.subscriber as PublicKey,
    tokenMint: raw.tokenMint as PublicKey,
    state,
  };
}

/**
 * Derive the SPL Associated Token Account address for `(owner, mint)`.
 *
 * Avoids a top-level import of `@solana/spl-token`'s
 * `getAssociatedTokenAddress` to keep the dependency surface identical
 * to the rest of `clients/ts/src/` — late-require pattern mirrors the
 * bs58 fallback in `resubscribe.ts`.
 */
async function deriveSubscriberAtaForMint(
  owner: PublicKey,
  mint: PublicKey,
): Promise<PublicKey> {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const splToken = require("@solana/spl-token");
  // `getAssociatedTokenAddressSync` is sync; older SDKs export
  // `getAssociatedTokenAddress` (async). Cover both for resilience.
  if (typeof splToken.getAssociatedTokenAddressSync === "function") {
    return splToken.getAssociatedTokenAddressSync(mint, owner, true) as PublicKey;
  }
  return (await splToken.getAssociatedTokenAddress(mint, owner, true)) as PublicKey;
}

// ─────────────────────────────────────────────────────────────────────────
// Internal helpers — inline ix builders
// ─────────────────────────────────────────────────────────────────────────

/**
 * Inline `close_session` ix builder. ADR-005 inherits ADR-x402-001's
 * `close_session` contract verbatim — signer = subscriber. The standalone
 * `buildCloseSessionIx` in `closeSession.ts` requires the full PDA; we
 * already have `(subscription, sessionId)` so we derive locally.
 */
async function buildCloseSessionIxInline(
  program: Program<Nakama>,
  subscription: PublicKey,
  subscriber: PublicKey,
  sessionId: bigint,
): Promise<TransactionInstruction> {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const BN = require("bn.js");
  const sessionIdLe = new BN(sessionId.toString()).toArrayLike(
    Buffer,
    "le",
    8,
  );
  const { derivePaySessionPda } = await import("../pdas");
  const [paySessionPda] = derivePaySessionPda(
    program.programId,
    subscription,
    new BN(sessionId.toString()),
  );
  void sessionIdLe; // PDA derivation owns the seed bytes.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = program.methods as any;
  return await methods
    .closeSession()
    .accounts({
      parent: subscription,
      paySession: paySessionPda,
      subscriber,
    })
    .instruction();
}

/**
 * Inline `cleanup` ix builder — ADR-013 rent-reclaim. Signer =
 * subscriber. No dedicated builder file exists today (used inline by
 * `buildResubscribeIxs`); we keep the same shape here.
 */
async function buildCleanupIxInline(
  program: Program<Nakama>,
  subscription: PublicKey,
  subscriber: PublicKey,
): Promise<TransactionInstruction> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = program.methods as any;
  return await methods
    .cleanup()
    .accounts({
      subscription,
      subscriber,
    })
    .instruction();
}

/**
 * Inline `subscribe(periods_to_prefund)` ix builder. Mirrors the
 * `subscribe` block in `buildResubscribeIxs` — same ABI, same accounts.
 */
async function buildSubscribeIx(
  program: Program<Nakama>,
  subscriber: PublicKey,
  plan: PublicKey,
  tokenMint: PublicKey,
  subscriberAta: PublicKey,
  periodsToPrefund: number,
): Promise<TransactionInstruction> {
  const [subscriptionPda] = deriveSubscriptionPda(
    program.programId,
    subscriber,
    plan,
  );
  const [vaultPda] = deriveVaultPda(program.programId, subscriptionPda);
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = program.methods as any;
  return await methods
    .subscribe(periodsToPrefund)
    .accounts({
      subscriber,
      plan,
      tokenMint,
      subscription: subscriptionPda,
      vault: vaultPda,
      subscriberAta,
      tokenProgram: TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
      rent: SYSVAR_RENT_PUBKEY,
    })
    .instruction();
}

/**
 * Extract `Connection` from a `Program<Nakama>` provider, or throw with
 * an actionable error. Same pattern as `resubscribeOrSubscribe`.
 */
function getConnection(program: Program<Nakama>): Connection {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const connection: Connection | undefined = (
    program.provider as unknown as { connection?: Connection }
  ).connection;
  if (!connection) {
    throw new Error(
      "Program provider has no `connection` — buildChangeRateTx needs an " +
        "AnchorProvider with a configured RPC endpoint.",
    );
  }
  return connection;
}
