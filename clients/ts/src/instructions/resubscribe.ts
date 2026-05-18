/**
 * `resubscribe` composite-tx builder (ADR-008).
 *
 * Re-subscribe is **client-side only** — zero on-chain code change. SDK
 * detects a `Cancelled` tombstone Subscription PDA and prepends a
 * `cleanup` instruction (ADR-013) to the regular `subscribe` instruction
 * in one atomic transaction. Optionally enumerates alive `PaySession`
 * satellites (ADR-x402-001) and prepends `close_session × N` so the
 * tx fully reclaims orphan rent.
 *
 * Designed as a pair:
 *   - `buildResubscribeIxs` — pure builder, returns an instruction array.
 *     Caller composes the `Transaction` / `VersionedTransaction`,
 *     attaches compute-budget tweaks, and signs.
 *   - `resubscribeOrSubscribe` — top-level helper that performs the RPC
 *     state-fetch, dispatches the builder, then sends + confirms via the
 *     supplied `Connection`. Convenience for CLI / scripts; production
 *     callers usually own their submission path and use the builder
 *     directly.
 *
 * GRASP roles:
 *   - `buildResubscribeIxs` — Pure Fabrication (orchestrates ix assembly
 *     given pre-computed inputs; no I/O, no signing).
 *   - `findAlivePaySessions` — Information Expert (knows the on-chain
 *     `PaySession` layout: Anchor 8-byte discriminator, `subscription`
 *     pubkey at offset 8, `state` byte at offset 8+168=176).
 *   - `resubscribeOrSubscribe` — Controller (fetches state, dispatches,
 *     submits).
 *
 * @see ADR-008 §Decision (composite cleanup + subscribe)
 * @see ADR-008 §"x402 forward-compat" (PaySession orphan handling)
 * @see ADR-013 §Q7 (subscribe-after-cancel-before-cleanup race)
 */

import { Program } from "@anchor-lang/core";
import {
  Connection,
  PublicKey,
  Signer,
  SystemProgram,
  Transaction,
  TransactionInstruction,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";

import { Nakama, SubscriptionState, decodeSubscriptionState } from "../types";
import {
  derivePaySessionPda,
  deriveSubscriptionPda,
  deriveVaultPda,
} from "../pdas";

// ─────────────────────────────────────────────────────────────────────────
// Constants and offsets
// ─────────────────────────────────────────────────────────────────────────

/**
 * Anchor account discriminator length (bytes prepended to every Anchor
 * account on the wire — `sha256("account:<name>")[..8]`).
 */
const ANCHOR_DISCRIMINATOR_LEN = 8;

/**
 * `PaySession.subscription: Pubkey` byte offset within the account data.
 * Offset = `8 (Anchor disc) + 0 (first field)`. Verified against
 * `state.rs:PaySession` layout table (ADR-x402-001 §"PaySession PDA Layout").
 */
const PAY_SESSION_SUBSCRIPTION_OFFSET = ANCHOR_DISCRIMINATOR_LEN + 0;

/**
 * `PaySession.state: u8` byte offset within the account data.
 * Offset = `8 (disc) + 168 (state field offset in payload)` = 176.
 * `PaySessionState::Open = 0`, `Settling = 1`, `Closed = 2`.
 */
const PAY_SESSION_STATE_OFFSET = ANCHOR_DISCRIMINATOR_LEN + 168;

/** `PaySessionState::Open` discriminant — closable via `close_session`. */
const PAY_SESSION_STATE_OPEN = 0;

/**
 * Soft cap on alive PaySession satellites the composite builder will close
 * in one transaction. ADR-008 §"x402 forward-compat" envelope check:
 * 3-session worst-case fits in ~700B / ~90k CU. The cap is set to 4 to
 * leave envelope headroom; callers with more sessions must fall back to a
 * multi-tx flow (close some out-of-band first).
 *
 * @see ADR-008 §"x402 forward-compat" (Q3 / Q4 envelope checks)
 */
const MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE = 4;

// ─────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────

/**
 * Inputs for `buildResubscribeIxs`. All PDAs and ATAs are pre-computed
 * by the caller; this builder does no derivation beyond what is needed
 * to compose the typed Anchor methods calls.
 */
export interface BuildResubscribeIxsArgs {
  /** Anchor `Program<Nakama>` from the canonical IDL. */
  program: Program<Nakama>;

  /** Subscriber wallet — single signer for the composite tx. */
  subscriber: PublicKey;

  /** Plan PDA the subscriber wants to (re-)subscribe to. */
  plan: PublicKey;

  /** Token mint snapshot (must equal `plan.token_mint`). */
  tokenMint: PublicKey;

  /** Subscriber USDC ATA — source of the prefund transfer. */
  subscriberAta: PublicKey;

  /**
   * Prefund period count (u8, 1..=255). Forwarded to `subscribe()` per
   * the on-chain handler signature `subscribe(periods_to_prefund: u8)`.
   * BLK-07 enforces `>= 1`.
   */
  periodsToPrefund: number;

  /**
   * Result of fetching the Subscription PDA via `connection.getAccountInfo`
   * decoded into the SDK's `SubscriptionState` enum, OR `null` if the PDA
   * does not exist. Drives the cleanup-prepend branch.
   *
   * Caller is responsible for the RPC fetch — keeps this builder pure.
   * `resubscribeOrSubscribe` does it on behalf of the caller.
   */
  existingState: SubscriptionState | null;

  /**
   * Alive PaySession PDAs to close before `cleanup` (ADR-008 §"x402
   * forward-compat"). Empty array when not closing them in-tx (or no
   * sessions exist). `resubscribeOrSubscribe` enumerates this list via
   * `findAlivePaySessions` when `closeAlivePaySessions` is true.
   *
   * Each entry MUST be `PaySessionState::Open`; `Settling` cannot be
   * closed via `close_session` (handler requires `state == Open`).
   *
   * Each entry corresponds to `(subscription, sessionId)` — the
   * `close_session` ix needs both since the PDA seeds are
   * `[b"pay_session", subscription, session_id_le]`.
   */
  alivePaySessions: AlivePaySession[];
}

/**
 * Reference to an alive PaySession PDA for in-tx closure. The PDA itself
 * is re-derivable from `(subscription, sessionId)`, so we don't carry it.
 */
export interface AlivePaySession {
  /** u64 session id (matches the on-chain `pay_session.session_id`). */
  sessionId: bigint;
}

/**
 * Inputs for `resubscribeOrSubscribe` — the top-level Controller helper
 * that owns RPC fetch + submission. Mirrors the builder args but is
 * higher-level: no `existingState` (helper fetches it).
 */
export interface ResubscribeOrSubscribeArgs {
  program: Program<Nakama>;
  subscriber: PublicKey;
  plan: PublicKey;
  tokenMint: PublicKey;
  subscriberAta: PublicKey;
  periodsToPrefund: number;

  /**
   * If `true`, enumerate alive PaySession PDAs via `getProgramAccounts`
   * and prepend `close_session × N` to the composite tx. Default `true`
   * per ADR-008 §"x402 forward-compat" architect recommendation (clean
   * UX, reclaims rent). Set `false` to accept orphan PaySession rent
   * lockup until later cleanup.
   *
   * @see ADR-008 §E5 "Satellite PDAs alive at cleanup time"
   */
  closeAlivePaySessions?: boolean;

  /** Signers — typically `[subscriberKeypair]`. Forwarded to send. */
  signers: Signer[];
}

/** Result of a successful `resubscribeOrSubscribe` call. */
export interface ResubscribeOrSubscribeResult {
  /** Transaction signature (base58). */
  signature: string;
  /**
   * Whether the composite tx included a `cleanup` ix. `true` = tombstone
   * detected and re-subscribed; `false` = no prior subscription, plain
   * subscribe.
   */
  resubscribed: boolean;
  /** Number of alive PaySession PDAs closed in the composite. */
  paySessionsClosedInTx: number;
}

// ─────────────────────────────────────────────────────────────────────────
// Builder — pure, no I/O
// ─────────────────────────────────────────────────────────────────────────

/**
 * Build the composite-tx instruction list for re-subscribe.
 *
 * Branches:
 *  - `existingState === null` → returns `[subscribeIx]` (fresh subscribe).
 *  - `existingState === Cancelled` → returns
 *    `[...closeSessionIx × N, cleanupIx, subscribeIx]`.
 *  - `existingState ∈ {Active, Paused, GracePeriod, Exhausted}` → throws.
 *    Caller misuse — the subscription is still alive and re-subscribe is
 *    not the right primitive (use `top_up`, `resume`, etc).
 *
 * Pure: does not sign, send, or touch RPC. Caller wraps the returned
 * instructions in a `Transaction` / `VersionedTransaction`.
 *
 * @example Cancelled tombstone, two alive PaySessions to close in-tx:
 * ```ts
 * const ixs = await buildResubscribeIxs({
 *   program,
 *   subscriber: subscriber.publicKey,
 *   plan: planPda,
 *   tokenMint: USDC_MINT,
 *   subscriberAta,
 *   periodsToPrefund: 2,
 *   existingState: SubscriptionState.Cancelled,
 *   alivePaySessions: [{ sessionId: 1n }, { sessionId: 2n }],
 * });
 * // ixs.length === 4 → [close_session×2, cleanup, subscribe]
 * const tx = new Transaction().add(...ixs);
 * ```
 *
 * @throws Error if `existingState` is alive (not null and not Cancelled).
 * @throws Error if `alivePaySessions.length > 4` (soft cap; use multi-tx).
 */
export async function buildResubscribeIxs(
  args: BuildResubscribeIxsArgs,
): Promise<TransactionInstruction[]> {
  // ── Guards ─────────────────────────────────────────────────────────────
  if (args.alivePaySessions.length > MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE) {
    throw new Error(
      `Too many alive PaySessions (N=${args.alivePaySessions.length}); ` +
        `composite tx soft-cap is ${MAX_ALIVE_PAY_SESSIONS_IN_COMPOSITE}. ` +
        `Use a multi-tx flow: close some PaySessions out-of-band first.`,
    );
  }

  if (args.existingState !== null) {
    if (args.existingState !== SubscriptionState.Cancelled) {
      const stateName = SubscriptionState[args.existingState] ?? `byte=${args.existingState}`;
      throw new Error(
        `Cannot resubscribe — existing subscription is alive ` +
          `(state=${stateName}). Re-subscribe is only valid when the ` +
          `Subscription PDA is absent (fresh) or in Cancelled tombstone. ` +
          `For Active/Paused/GracePeriod use top_up / resume / cancel.`,
      );
    }
  }

  if (args.periodsToPrefund < 1 || args.periodsToPrefund > 255) {
    throw new Error(
      `periodsToPrefund must be in 1..=255 (u8 on-chain); got ${args.periodsToPrefund}.`,
    );
  }

  // ── Derive ─────────────────────────────────────────────────────────────
  const [subscriptionPda] = deriveSubscriptionPda(
    args.program.programId,
    args.subscriber,
    args.plan,
  );
  const [vaultPda] = deriveVaultPda(args.program.programId, subscriptionPda);

  // ── Compose ────────────────────────────────────────────────────────────
  const ixs: TransactionInstruction[] = [];

  // 1) close_session × N (only when tombstone exists; alive subscriptions
  //    are rejected above, fresh subscriptions have no satellites).
  if (args.existingState === SubscriptionState.Cancelled) {
    for (const session of args.alivePaySessions) {
      const [paySessionPda] = derivePaySessionPda(
        args.program.programId,
        subscriptionPda,
        // BN-compat: derivePaySessionPda takes BN; we accept bigint here
        // and convert once. Avoid a new bn.js import surface — reuse the
        // helper's expected type via a tiny adapter.
        toBn(session.sessionId),
      );
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      const methods = args.program.methods as any;
      const ix: TransactionInstruction = await methods
        .closeSession()
        .accounts({
          parent: subscriptionPda,
          paySession: paySessionPda,
          subscriber: args.subscriber,
        })
        .instruction();
      ixs.push(ix);
    }
  }

  // 2) cleanup (only when tombstone exists).
  if (args.existingState === SubscriptionState.Cancelled) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const methods = args.program.methods as any;
    const cleanupIx: TransactionInstruction = await methods
      .cleanup()
      .accounts({
        subscription: subscriptionPda,
        subscriber: args.subscriber,
      })
      .instruction();
    ixs.push(cleanupIx);
  }

  // 3) subscribe — always present. Same account list as `subscribe.rs`
  //    Accounts<Subscribe>. Mirrors `clients/ts/scripts/00-full-demo.ts`
  //    Phase 2 (verified ABI).
  {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const methods = args.program.methods as any;
    const subscribeIx: TransactionInstruction = await methods
      .subscribe(args.periodsToPrefund)
      .accounts({
        subscriber: args.subscriber,
        plan: args.plan,
        tokenMint: args.tokenMint,
        subscription: subscriptionPda,
        vault: vaultPda,
        subscriberAta: args.subscriberAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .instruction();
    ixs.push(subscribeIx);
  }

  return ixs;
}

// ─────────────────────────────────────────────────────────────────────────
// Alive PaySession enumeration
// ─────────────────────────────────────────────────────────────────────────

/**
 * Find PaySession satellite PDAs that belong to `subscription` and are
 * in `PaySessionState::Open` (closable via `close_session`).
 *
 * Implementation: `connection.getProgramAccounts` with two filters:
 *  - Anchor discriminator (first 8 bytes) — narrows to PaySession type.
 *  - `subscription` field bytes (offset 8) — narrows to this parent.
 *
 * Then JS-side filter on `state` byte (offset 176) to skip `Settling`
 * (transient) and `Closed` (Anchor-closed accounts vanish, but we guard
 * defensively).
 *
 * Returns the alive sessions sorted ascending by `session_id` so the
 * resulting composite tx is deterministic across calls (eases test
 * reproducibility).
 *
 * **Note**: `getProgramAccounts` is RPC-expensive at scale (>10k accounts
 * per program). For a single (subscriber, plan) tuple the filter narrows
 * to a few rows; acceptable in the SDK happy path. ADR-008 §"Defer to
 * Future work" notes the >10k case routes to Yellowstone gRPC.
 *
 * @see ADR-x402-001 §"R1 closure" (close_session contract)
 * @see ADR-008 §"x402 forward-compat" (enumeration pattern)
 */
export async function findAlivePaySessions(
  connection: Connection,
  program: Program<Nakama>,
  subscription: PublicKey,
): Promise<AlivePaySession[]> {
  // Pull the PaySession Anchor discriminator from the IDL — keeps the SDK
  // resilient if Anchor regenerates discriminators across program renames.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const idl = (program as unknown as { idl: any }).idl;
  const paySessionAcct = idl?.accounts?.find(
    (a: { name: string }) => a.name === "PaySession",
  ) as { discriminator?: number[] } | undefined;
  if (!paySessionAcct?.discriminator || paySessionAcct.discriminator.length !== 8) {
    // IDL drift — fail loud rather than over-fetch.
    throw new Error(
      "IDL is missing the PaySession account discriminator. Regenerate via `anchor build`.",
    );
  }
  const discBytes = Buffer.from(paySessionAcct.discriminator);

  const matches = await connection.getProgramAccounts(program.programId, {
    commitment: "confirmed",
    filters: [
      // 1) Anchor type filter.
      { memcmp: { offset: 0, bytes: bs58Encode(discBytes) } },
      // 2) Parent Subscription filter.
      { memcmp: { offset: PAY_SESSION_SUBSCRIPTION_OFFSET, bytes: subscription.toBase58() } },
    ],
  });

  const alive: AlivePaySession[] = [];
  for (const { account } of matches) {
    const data = account.data;
    if (data.length < PAY_SESSION_STATE_OFFSET + 1) continue;
    const stateByte = data[PAY_SESSION_STATE_OFFSET];
    if (stateByte !== PAY_SESSION_STATE_OPEN) continue;

    // session_id: u64 LE at offset 8 (disc) + 128 (offset in payload) = 136.
    const sessionIdOffset = ANCHOR_DISCRIMINATOR_LEN + 128;
    const sessionId = readU64Le(data, sessionIdOffset);
    alive.push({ sessionId });
  }

  alive.sort((a, b) =>
    a.sessionId === b.sessionId ? 0 : a.sessionId < b.sessionId ? -1 : 1,
  );
  return alive;
}

// ─────────────────────────────────────────────────────────────────────────
// Controller — top-level convenience wrapper
// ─────────────────────────────────────────────────────────────────────────

/**
 * One-shot re-subscribe helper: fetch state, dispatch builder, submit.
 *
 * Convenience wrapper around `buildResubscribeIxs` for CLI / demo
 * scripts. Production callers that own their wallet / sending path
 * should use `buildResubscribeIxs` + `findAlivePaySessions` directly.
 *
 * @example
 * ```ts
 * const { signature, resubscribed } = await resubscribeOrSubscribe({
 *   program,
 *   subscriber: subscriber.publicKey,
 *   plan: planPda,
 *   tokenMint: USDC_MINT,
 *   subscriberAta,
 *   periodsToPrefund: 2,
 *   closeAlivePaySessions: true,
 *   signers: [subscriber],
 * });
 * console.log(resubscribed ? "Resubscribed" : "Fresh subscribe", signature);
 * ```
 */
export async function resubscribeOrSubscribe(
  args: ResubscribeOrSubscribeArgs,
): Promise<ResubscribeOrSubscribeResult> {
  const provider = args.program.provider;
  if (!provider || typeof provider.sendAndConfirm !== "function") {
    throw new Error(
      "Program provider does not support sendAndConfirm — pass an AnchorProvider.",
    );
  }
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const connection: Connection = (provider as any).connection;
  if (!connection) {
    throw new Error("Program provider has no `connection` — cannot fetch state.");
  }

  const [subscriptionPda] = deriveSubscriptionPda(
    args.program.programId,
    args.subscriber,
    args.plan,
  );

  // Detect tombstone / fresh.
  const existingState = await fetchSubscriptionState(
    args.program,
    subscriptionPda,
  );

  // Enumerate alive PaySessions only when there's a tombstone to clean up
  // AND the caller opted in. Fresh subscribe has no satellites.
  const wantClose =
    args.closeAlivePaySessions !== false /* default true */ &&
    existingState === SubscriptionState.Cancelled;
  const alivePaySessions: AlivePaySession[] = wantClose
    ? await findAlivePaySessions(connection, args.program, subscriptionPda)
    : [];

  const ixs = await buildResubscribeIxs({
    program: args.program,
    subscriber: args.subscriber,
    plan: args.plan,
    tokenMint: args.tokenMint,
    subscriberAta: args.subscriberAta,
    periodsToPrefund: args.periodsToPrefund,
    existingState,
    alivePaySessions,
  });

  const tx = new Transaction().add(...ixs);

  // Solana legacy-tx envelope is 1232 bytes. Composite-tx in ADR-008 envelope
  // analysis fits ≤700B even with 3 PaySession closes — overflow is unlikely
  // unless future ADRs grow the account list. Pre-flight check surfaces
  // overflow with an actionable error before send.
  //
  // We cannot fully serialize without a recent blockhash; use a sentinel and
  // tolerate the SystemProgram.programId default for fee payer. If
  // serialization throws on size, surface the ADR-008 §E5 split-tx hint.
  try {
    // Pre-flight: blockhash unknown, so use a dummy. Anchor's Transaction
    // serializer will compute size from the ix list + signature slot.
    tx.feePayer = args.subscriber;
    tx.recentBlockhash = "11111111111111111111111111111111";
    const wire = tx.serialize({ requireAllSignatures: false, verifySignatures: false });
    if (wire.length > 1232) {
      throw new Error(
        `Composite tx exceeds 1232-byte limit (got ${wire.length}B); ` +
          `split into separate cleanup + subscribe calls or close some ` +
          `PaySessions out-of-band first. See ADR-008 §"x402 forward-compat".`,
      );
    }
  } catch (err) {
    // Only re-throw if the error is our envelope error; otherwise the
    // dummy-blockhash path can confuse downstream callers — let the real
    // send below surface any other error.
    if (err instanceof Error && err.message.startsWith("Composite tx exceeds")) {
      throw err;
    }
    // Reset for real send.
    tx.recentBlockhash = undefined as unknown as string;
  }

  const signature = await provider.sendAndConfirm(tx, args.signers, {
    commitment: "confirmed",
  });

  return {
    signature,
    resubscribed: existingState === SubscriptionState.Cancelled,
    paySessionsClosedInTx: alivePaySessions.length,
  };
}

// ─────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────

/**
 * Read the Subscription PDA and decode its FSM state byte, or return
 * `null` if the account does not exist.
 */
async function fetchSubscriptionState(
  program: Program<Nakama>,
  subscriptionPda: PublicKey,
): Promise<SubscriptionState | null> {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const raw = await (program.account as any).subscription.fetchNullable(
    subscriptionPda,
  );
  if (raw === null) return null;
  const state = decodeSubscriptionState(raw.state);
  if (state === null) {
    throw new Error(
      `Subscription PDA ${subscriptionPda.toBase58()} has unrecognised ` +
        `state byte: ${JSON.stringify(raw.state)}.`,
    );
  }
  return state;
}

/**
 * Read a little-endian u64 from `data` at `offset` as a `bigint`.
 * Avoids `Number` precision loss on full u64 range.
 */
function readU64Le(data: Buffer | Uint8Array, offset: number): bigint {
  const buf = Buffer.isBuffer(data) ? data : Buffer.from(data);
  return buf.readBigUInt64LE(offset);
}

/**
 * Convert a bigint session id to the `BN` shape `derivePaySessionPda`
 * expects (the helper signature is older than the SDK's `bigint`
 * adoption; rather than churn the helper, adapt here).
 */
function toBn(sessionId: bigint): import("bn.js") {
  // Late-binding require to keep this module free of a direct bn.js
  // import at the top (mirrors `computedStatus.ts` pattern).
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const BN = require("bn.js");
  return new BN(sessionId.toString());
}

/**
 * Base58 encoder for the `memcmp.bytes` filter input — `getProgramAccounts`
 * requires base58 strings, not raw bytes. We pull bs58 indirectly via
 * `@solana/web3.js`'s `PublicKey.toBase58` route for 32-byte arguments;
 * for shorter byte sequences (the 8-byte discriminator) we encode manually.
 */
function bs58Encode(bytes: Buffer): string {
  // bs58 is a transitive dep of @solana/web3.js; resolve at runtime to
  // avoid adding a top-level dep. If unavailable, surface a clear error.
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    const bs58 = require("bs58");
    // bs58 versions differ on default vs named export; cover both.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const encoder: any = bs58.default ?? bs58;
    return encoder.encode(bytes);
  } catch {
    throw new Error(
      "bs58 module not available — required by findAlivePaySessions for " +
        "the discriminator filter. Install bs58 or upgrade @solana/web3.js.",
    );
  }
}
