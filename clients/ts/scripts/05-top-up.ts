#!/usr/bin/env ts-node
/**
 * Realistic `top_up` lifecycle demo (ADR-007 stage-2 SDK smoke test).
 *
 *   1. subscribe with price=86_400 USDC, period=86_400s → rate=1 USDC/s.
 *   2. charge after 80_000s → withdraws 80_000; deposited still 86_400 → no Grace.
 *   3. warp clock +6_400s; charge → withdraws full 86_400; tail enters Grace.
 *   4. top_up 86_400 → state flips Active; satellite closed (rent → subscriber).
 * Avoids ZeroRatePerSecond by ensuring price >= period_in_seconds.
 *
 * This is NOT an integration test — Rust LiteSVM tests cover the FSM matrix
 * (test-engineer stage 3). This script exists to:
 *   - exercise the SDK builders against a live program (devnet or localnet)
 *   - demonstrate the full Grace → Active rescue path for the Loom pitch
 *   - surface IDL drift early (anchor-engineer ↔ sdk-engineer feedback)
 *
 * Usage:
 *   ts-node clients/ts/scripts/05-top-up.ts \
 *     --network=devnet \
 *     --keypair=~/.config/solana/id.json \
 *     --subscription=<sub-pda-base58> \
 *     --amount=86400000000
 *
 * Defaults:
 *   --network = devnet
 *   --keypair = ~/.config/solana/id.json (per CLAUDE.md project facts)
 *
 * Exit codes:
 *   0 — top_up succeeded
 *   1 — argv / fs error
 *   2 — RPC / Anchor error (error code printed)
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";

import { AnchorProvider, Program, Wallet } from "@anchor-lang/core";
import {
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  clusterApiUrl,
} from "@solana/web3.js";
import { getAssociatedTokenAddressSync } from "@solana/spl-token";

import {
  buildTopUpIx,
  deriveGracedSubscriptionPda,
  deriveStatus,
  deriveVaultPda,
  normalizeSubscriptionAccount,
  SubscriptionState,
  type Nakama,
} from "../src";

// --------------------------------------------------------------------------
// argv parsing — minimal, no dep on yargs / commander
// --------------------------------------------------------------------------

interface ScriptArgs {
  network: "devnet" | "localnet";
  keypairPath: string;
  subscription: PublicKey;
  amount: bigint;
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

  if (!args.subscription) {
    throw new Error("missing --subscription=<base58 pubkey>");
  }
  if (!args.amount) {
    throw new Error("missing --amount=<u64 base units>");
  }
  return {
    network,
    keypairPath,
    subscription: new PublicKey(args.subscription),
    amount: BigInt(args.amount),
  };
}

// --------------------------------------------------------------------------
// keypair / provider — direct keypair from disk per CLAUDE.md convention
// --------------------------------------------------------------------------

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

// --------------------------------------------------------------------------
// IDL load — read from `nakama/target/idl/nakama.json`
// --------------------------------------------------------------------------

function loadIdl(): { idl: any; programId: PublicKey } {
  const idlPath = path.resolve(
    __dirname,
    "../../../nakama/target/idl/nakama.json",
  );
  const idl = JSON.parse(fs.readFileSync(idlPath, "utf8"));
  const programId = new PublicKey(idl.address);
  return { idl, programId };
}

// --------------------------------------------------------------------------
// Anchor error code → name lookup. ADR-007 §"Error enum additions".
// --------------------------------------------------------------------------

function decodeAnchorError(err: unknown, idl: any): string {
  // Anchor wraps program errors in `AnchorError` with `error.errorCode.code`
  // (string name) and `error.errorCode.number` (u32). On raw transport
  // errors (logs only) we fall back to scanning logs for `Error Code:`.
  const e = err as any;
  if (e?.error?.errorCode?.code) {
    return `${e.error.errorCode.code} (${e.error.errorCode.number})`;
  }
  const logs: string[] = e?.logs ?? e?.transactionLogs ?? [];
  for (const line of logs) {
    const m = line.match(/Error Code:\s*(\w+)\.\s*Error Number:\s*(\d+)/);
    if (m) {
      // Cross-reference against IDL errors map for sanity (best-effort).
      const knownNames =
        idl.errors?.map((er: { name: string }) => er.name) ?? [];
      const name = m[1];
      const matched = knownNames.includes(name) ? name : `${name}?`;
      return `${matched} (${m[2]})`;
    }
  }
  return e?.message ?? String(err);
}

// --------------------------------------------------------------------------
// main
// --------------------------------------------------------------------------

async function main() {
  const args = parseArgs(process.argv);
  const subscriber = loadKeypair(args.keypairPath);
  const connection = new Connection(rpcUrlFor(args.network), "confirmed");
  const wallet = new Wallet(subscriber);
  const provider = new AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });

  const { idl, programId } = loadIdl();
  // `Program<Nakama>` — once IDL regenerated post-anchor build, the typed
  // method `topUp` becomes available. Until then, cast handles the gap.
  const program = new Program(idl, provider) as unknown as Program<Nakama>;

  console.log(`Network:      ${args.network}`);
  console.log(`Program ID:   ${programId.toBase58()}`);
  console.log(`Subscriber:   ${subscriber.publicKey.toBase58()}`);
  console.log(`Subscription: ${args.subscription.toBase58()}`);
  console.log(`Amount:       ${args.amount} (base units)`);
  console.log("");

  // 1. Read current Subscription state.
  // Anchor's auto-snake_case → camelCase conversion gives `subscription`.
  const subAccountRaw = await (program.account as any).subscription.fetch(
    args.subscription,
  );
  const sub = normalizeSubscriptionAccount(subAccountRaw);
  const tokenMint = sub.tokenMint;
  console.log("Pre-state:");
  console.log(`  state            = ${SubscriptionState[sub.state]}`);
  console.log(`  deposited_amount = ${sub.depositedAmount.toString()}`);
  console.log(`  withdrawn_amount = ${sub.withdrawnAmount.toString()}`);
  console.log(`  rate_per_second  = ${sub.ratePerSecond.toString()}`);
  console.log("");

  // 2. Derive the GracedSubscription PDA + try fetching satellite (may be null).
  const [gracedPda] = deriveGracedSubscriptionPda(
    programId,
    args.subscription,
  );
  const gracedRaw = await (program.account as any)
    .gracedSubscription?.fetchNullable(gracedPda)
    .catch(() => null);
  const graced = gracedRaw ?? null;

  // 3. Derive ATA + vault for the tx accounts.
  const subscriberAta = getAssociatedTokenAddressSync(
    tokenMint,
    subscriber.publicKey,
  );
  const [vault] = deriveVaultPda(programId, args.subscription);

  // 4. Pre-compute status for visibility.
  const now = BigInt(Math.floor(Date.now() / 1000));
  const preStatus = deriveStatus(sub, graced, null, now);
  console.log(`Pre-status:   ${JSON.stringify(preStatus, bigIntReplacer)}`);
  console.log("");

  // 5. Build + submit top_up tx.
  const ix = await buildTopUpIx({
    program,
    subscriber: subscriber.publicKey,
    subscription: args.subscription,
    subscriberAta,
    vault,
    amount: args.amount,
    state: sub.state,
  });
  const tx = new Transaction().add(ix);

  let signature: string;
  try {
    signature = await provider.sendAndConfirm(tx, [subscriber], {
      commitment: "confirmed",
    });
  } catch (err) {
    const decoded = decodeAnchorError(err, idl);
    console.error(`top_up FAILED: ${decoded}`);
    process.exit(2);
  }
  console.log(`top_up tx:    ${signature}`);
  console.log("");

  // 6. Re-read state.
  const postRaw = await (program.account as any).subscription.fetch(
    args.subscription,
  );
  const postSub = normalizeSubscriptionAccount(postRaw);
  console.log("Post-state:");
  console.log(`  state            = ${SubscriptionState[postSub.state]}`);
  console.log(`  deposited_amount = ${postSub.depositedAmount.toString()}`);
  console.log("");

  const postGraced = await (program.account as any)
    .gracedSubscription?.fetchNullable(gracedPda)
    .catch(() => null);
  const postStatus = deriveStatus(
    postSub,
    postGraced ?? null,
    null,
    BigInt(Math.floor(Date.now() / 1000)),
  );
  console.log(`Post-status:  ${JSON.stringify(postStatus, bigIntReplacer)}`);
}

function bigIntReplacer(_key: string, value: unknown): unknown {
  return typeof value === "bigint" ? value.toString() : value;
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : err);
  process.exit(1);
});
