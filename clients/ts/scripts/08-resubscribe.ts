#!/usr/bin/env ts-node
/**
 * `resubscribe` smoke script (ADR-008 stage-2 SDK demo).
 *
 * Demonstrates the SDK's `resubscribeOrSubscribe` composite-tx helper:
 *  - Fetches the Subscription PDA for `(subscriber, plan)`.
 *  - If absent → submits a plain `subscribe` (single-ix tx).
 *  - If in `Cancelled` tombstone → composes `[close_session × N?, cleanup,
 *    subscribe]` in one atomic transaction.
 *  - If alive (Active/Paused/GracePeriod/Exhausted) → refuses with a
 *    clear error message and exits non-zero.
 *
 * Usage:
 *   ts-node clients/ts/scripts/08-resubscribe.ts \
 *     --network=devnet \
 *     --keypair=~/.config/solana/id.json \
 *     --plan=<plan-pda-base58> \
 *     --periods-to-prefund=2 \
 *     --close-paysessions=true
 *
 * Defaults:
 *   --network            = devnet
 *   --keypair            = ~/.config/solana/id.json
 *   --periods-to-prefund = 2
 *   --close-paysessions  = true
 *
 * Exit codes:
 *   0 — submission succeeded (resubscribe or fresh subscribe)
 *   1 — argv / fs error
 *   2 — RPC / Anchor error (decoded code printed)
 *
 * @see ADR-008 §Decision (composite cleanup+subscribe)
 * @see ADR-013 §Q7 (re-subscribe race resolution)
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";

import { AnchorProvider, Program, Wallet } from "@anchor-lang/core";
import {
  Connection,
  Keypair,
  PublicKey,
  clusterApiUrl,
} from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";

import {
  deriveSubscriptionPda,
  normalizeSubscriptionAccount,
  resubscribeOrSubscribe,
  SubscriptionState,
  type Nakama,
} from "../src";

interface ScriptArgs {
  network: "devnet" | "localnet";
  keypairPath: string;
  plan: PublicKey;
  periodsToPrefund: number;
  closePaySessions: boolean;
}

function parseArgs(argv: string[]): ScriptArgs {
  const args: Record<string, string> = {};
  for (const raw of argv.slice(2)) {
    const [k, v] = raw.replace(/^--/, "").split("=");
    if (k && v !== undefined) args[k] = v;
  }
  const network =
    args.network === "localnet" ? "localnet" : ("devnet" as const);
  const keypairPath = args.keypair
    ? args.keypair.replace(/^~(?=$|\/|\\)/, os.homedir())
    : path.join(os.homedir(), ".config", "solana", "id.json");

  if (!args.plan) {
    throw new Error("missing --plan=<base58 pubkey>");
  }
  const periodsRaw = args["periods-to-prefund"] ?? "2";
  const periodsToPrefund = Number.parseInt(periodsRaw, 10);
  if (
    !Number.isFinite(periodsToPrefund) ||
    periodsToPrefund < 1 ||
    periodsToPrefund > 255
  ) {
    throw new Error(
      `--periods-to-prefund must be a u8 in 1..=255, got: ${periodsRaw}`,
    );
  }
  const closeFlag = (args["close-paysessions"] ?? "true").toLowerCase();
  const closePaySessions = closeFlag !== "false";

  return {
    network,
    keypairPath,
    plan: new PublicKey(args.plan),
    periodsToPrefund,
    closePaySessions,
  };
}

function loadKeypair(filePath: string): Keypair {
  const raw = fs.readFileSync(filePath, "utf8");
  const bytes = Uint8Array.from(JSON.parse(raw) as number[]);
  return Keypair.fromSecretKey(bytes);
}

function rpcUrlFor(network: "devnet" | "localnet"): string {
  return network === "localnet"
    ? "http://127.0.0.1:8899"
    : clusterApiUrl("devnet");
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function loadIdl(): { idl: any; programId: PublicKey } {
  const idlPath = path.resolve(
    __dirname,
    "../../../nakama/target/idl/nakama.json",
  );
  const idl = JSON.parse(fs.readFileSync(idlPath, "utf8"));
  const programId = new PublicKey(idl.address);
  return { idl, programId };
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
function decodeAnchorError(err: unknown, idl: any): string {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const e = err as any;
  if (e?.error?.errorCode?.code) {
    return `${e.error.errorCode.code} (${e.error.errorCode.number})`;
  }
  const logs: string[] = e?.logs ?? e?.transactionLogs ?? [];
  for (const line of logs) {
    const m = line.match(/Error Code:\s*(\w+)\.\s*Error Number:\s*(\d+)/);
    if (m) {
      const knownNames =
        idl.errors?.map((er: { name: string }) => er.name) ?? [];
      const name = m[1];
      const matched = knownNames.includes(name) ? name : `${name}?`;
      return `${matched} (${m[2]})`;
    }
  }
  return e?.message ?? String(err);
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv);
  const signer = loadKeypair(args.keypairPath);
  const connection = new Connection(rpcUrlFor(args.network), "confirmed");
  const wallet = new Wallet(signer);
  const provider = new AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });

  const { idl, programId } = loadIdl();
  const program = new Program(idl, provider) as unknown as Program<Nakama>;

  console.log(`Network:            ${args.network}`);
  console.log(`Program ID:         ${programId.toBase58()}`);
  console.log(`Signer (subscriber):${signer.publicKey.toBase58()}`);
  console.log(`Plan:               ${args.plan.toBase58()}`);
  console.log(`Periods to prefund: ${args.periodsToPrefund}`);
  console.log(`Close PaySessions:  ${args.closePaySessions}`);
  console.log("");

  // Pre-flight: read Plan to source `token_mint`; derive subscriber ATA.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const planAccount = await (program.account as any).plan.fetch(args.plan);
  const tokenMint: PublicKey = planAccount.tokenMint;
  const subscriberAta = getAssociatedTokenAddressSync(
    tokenMint,
    signer.publicKey,
  );

  const [subscriptionPda] = deriveSubscriptionPda(
    programId,
    signer.publicKey,
    args.plan,
  );

  // Pre-state report — purely informational, the SDK helper re-fetches.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const preRaw = await (program.account as any).subscription.fetchNullable(
    subscriptionPda,
  );
  if (preRaw === null) {
    console.log("Pre-state: Subscription PDA does not exist → fresh subscribe.");
  } else {
    const pre = normalizeSubscriptionAccount(preRaw);
    console.log(`Pre-state: state = ${SubscriptionState[pre.state]}`);
    if (pre.state !== SubscriptionState.Cancelled) {
      console.log(
        "  (Subscription is alive — helper will refuse with a clear error.)",
      );
    }
  }
  console.log("");

  let result;
  try {
    result = await resubscribeOrSubscribe({
      program,
      subscriber: signer.publicKey,
      plan: args.plan,
      tokenMint,
      subscriberAta,
      periodsToPrefund: args.periodsToPrefund,
      closeAlivePaySessions: args.closePaySessions,
      signers: [signer],
    });
  } catch (err) {
    const decoded = decodeAnchorError(err, idl);
    console.error(`resubscribe FAILED: ${decoded}`);
    process.exit(2);
  }

  console.log(`Signature:            ${result.signature}`);
  console.log(
    `Action:               ${result.resubscribed ? "resubscribe (composite cleanup+subscribe)" : "fresh subscribe"}`,
  );
  console.log(
    `PaySessions closed:   ${result.paySessionsClosedInTx} (in-tx via composite)`,
  );

  // Post-state — confirm Active.
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const postRaw = await (program.account as any).subscription.fetch(
    subscriptionPda,
  );
  const post = normalizeSubscriptionAccount(postRaw);
  console.log(`Post-state:           state = ${SubscriptionState[post.state]}`);
  console.log(
    `Subscription PDA:     ${subscriptionPda.toBase58()}`,
  );
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
