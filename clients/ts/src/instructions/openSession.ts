/**
 * `open_session` instruction builder (ADR-x402-001 §"open_session").
 *
 * Subscriber-signed; initializes a PaySession satellite under their
 * Subscription. The `facilitator` argument names a delegate authority
 * that will sign subsequent `settle_usage` calls; the subscriber stays
 * non-custodial and can rotate by closing + reopening with a new
 * facilitator pubkey.
 *
 * @see ADR-x402-001 §"open_session"
 * @see ADR-x402-001 Q5 — on-chain delegation authority model
 */

import { Program, BN } from "@anchor-lang/core";
import {
  PublicKey,
  SystemProgram,
  TransactionInstruction,
} from "@solana/web3.js";

import { Nakama } from "../types";
import { derivePaySessionPda } from "../pdas";

export interface BuildOpenSessionIxArgs {
  program: Program<Nakama>;
  /** Subscriber wallet — must be the tx signer; matches `parent.subscriber`. */
  subscriber: PublicKey;
  /** Parent Subscription PDA. */
  subscription: PublicKey;
  /**
   * Client-chosen u64 nonce. SDK gens random; collision probability 2^-64.
   * Mirrors Rust `session_id.to_le_bytes()` in seed derivation.
   */
  sessionId: BN;
  /**
   * Authority pubkey for `settle_usage`. Caller's choice of delegate;
   * subscriber may use their own pubkey if no third-party facilitator.
   */
  facilitator: PublicKey;
  /**
   * Soft-cap on total session usage in USDC base units. `0` means
   * "unlimited up to remaining escrow"; otherwise must be ≤
   * `parent.deposited_amount - parent.withdrawn_amount`.
   */
  reservationCap: BN;
}

export async function buildOpenSessionIx(
  args: BuildOpenSessionIxArgs,
): Promise<TransactionInstruction> {
  const [paySessionPda] = derivePaySessionPda(
    args.program.programId,
    args.subscription,
    args.sessionId,
  );

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .openSession(args.sessionId, args.facilitator, args.reservationCap)
    .accounts({
      parent: args.subscription,
      paySession: paySessionPda,
      subscriber: args.subscriber,
      systemProgram: SystemProgram.programId,
    })
    .instruction();
}
