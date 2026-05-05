/**
 * `settle_usage` instruction builder (ADR-x402-001 §"settle_usage").
 *
 * Facilitator-signed CPI transfer `vault → merchant_ata` with shared
 * accounting on `parent.withdrawn_amount` (ADR-002 single source of
 * truth). Composes with `charge` linearly — both writers, no
 * double-spend.
 *
 * @see ADR-x402-001 §"settle_usage", §"Composability with charge"
 */

import { Program, BN } from "@anchor-lang/core";
import {
  PublicKey,
  TransactionInstruction,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";

import { Nakama } from "../types";
import { derivePaySessionPda, deriveVaultPda } from "../pdas";

export interface BuildSettleUsageIxArgs {
  program: Program<Nakama>;
  /** Facilitator pubkey — must match `pay_session.facilitator` AND be tx signer. */
  facilitator: PublicKey;
  /** Parent Subscription PDA. */
  subscription: PublicKey;
  /** PaySession session_id (the same nonce passed to open_session). */
  sessionId: BN;
  /** Snapshot merchant_ata from PaySession (NOT parent — they're equal at open). */
  merchantAta: PublicKey;
  /** Settle amount in USDC base units. Must be > 0 and ≤ unlocked-withdrawn. */
  amount: BN;
}

export async function buildSettleUsageIx(
  args: BuildSettleUsageIxArgs,
): Promise<TransactionInstruction> {
  const [paySessionPda] = derivePaySessionPda(
    args.program.programId,
    args.subscription,
    args.sessionId,
  );
  const [vaultPda] = deriveVaultPda(args.program.programId, args.subscription);

  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const methods = args.program.methods as any;
  return await methods
    .settleUsage(args.amount)
    .accounts({
      parent: args.subscription,
      paySession: paySessionPda,
      vault: vaultPda,
      merchantAta: args.merchantAta,
      facilitator: args.facilitator,
      tokenProgram: TOKEN_PROGRAM_ID,
    })
    .instruction();
}
