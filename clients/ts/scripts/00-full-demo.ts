#!/usr/bin/env ts-node
/**
 * Nakama Protocol — full devnet demo (`/demo-e2e` Phase 1).
 *
 * Drives the two-layer story end-to-end on devnet. Each phase emits a single
 * parse-anchor line `[PHASE N] <ix>: <signature>` so the Phase 3 log builder
 * can stitch the demo log deterministically.
 *
 * Flow: create_plan (1, merchant) → subscribe (2, subscriber, 2-period prefund)
 *  → top_up (3, +1 period) → sleep 65s (4) → charge (5, merchant=keeper)
 *  → open_session (6, subscriber) → settle ×2 (7,8 merchant=facilitator)
 *  → close_session (9) → pause+resume (10ab, merchant) → cancel+cleanup (11ab).
 *
 * Hardcoded inputs: devnet RPC, program HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm,
 * USDC 4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU, merchant from
 * ~/.config/solana/id.json, subscriber from $REPO/.env PRIVATE_KEY.
 *
 * Plan: plan_id = Date.now(), price = 2 USDC, period = 60s, prefund 2.
 * Rate = 33_333 µUSDC/s. Fail-fast: on any error print last-success phase
 * + full error/logs + exit 1.
 *
 * @see ADR-013 §Q1 — cleanup is subscriber-only.
 * @see ADR-006 — pause/resume are merchant-only.
 * @see ADR-007 §"charge handler tail" — graced satellite always attached.
 */

import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as crypto from "crypto";

import { AnchorProvider, BN, Program, Wallet } from "@anchor-lang/core";
import {
  Connection,
  Keypair,
  PublicKey,
  Transaction,
  clusterApiUrl,
} from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  createAssociatedTokenAccountInstruction,
  getAccount,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";

import {
  buildTopUpIx,
  buildOpenSessionIx,
  buildSettleUsageIx,
  buildCloseSessionIx,
  buildPauseIx,
  buildResumeIx,
  derivePlanPda,
  deriveSubscriptionPda,
  deriveVaultPda,
  deriveGracedSubscriptionPda,
  derivePaySessionPda,
  derivePausedSubscriptionPda,
  SubscriptionState,
  type Nakama,
} from "../src";

// bs58 v4 has no bundled types; destructure via `require` to keep tsc happy
// without adding @types/bs58 to package.json.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const bs58 = require("bs58") as {
  decode(s: string): Buffer;
  encode(b: Uint8Array): string;
};

// --------------------------------------------------------------------------
// Hardcoded inputs (per slash-command spec; do not parameterise)
// --------------------------------------------------------------------------

const RPC_URL = "https://api.devnet.solana.com";
const COMMITMENT = "confirmed" as const;
const PROGRAM_ID = new PublicKey("HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm");
const USDC_MINT = new PublicKey("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
const MERCHANT_KEYPAIR_PATH = path.join(
  os.homedir(),
  ".config",
  "solana",
  "id.json",
);
const ENV_PATH = path.resolve(__dirname, "../../../.env");
const IDL_PATH = path.resolve(
  __dirname,
  "../../../nakama/target/idl/nakama.json",
);

const EXPECTED_MERCHANT_PUBKEY = "BeNSGCbNZxeGjuMg1dSCQbiuEK4mSdUeG1vT3h31Ly2w";
const EXPECTED_SUBSCRIBER_PUBKEY = "EkCQAwbcH46VP7JvEPEnxy2Qqh1BNub7VwtpjXroXEDS";

// USDC base units (6 decimals).
const PRICE = 2_000_000n;       // 2 USDC
const PERIOD = 20;              // seconds
const PERIODS_TO_PREFUND = 2;   // → 4 USDC locked at subscribe
const TOP_UP_AMOUNT = 2_000_000n; // +1 period
const RESERVATION_CAP = 500_000n; // 0.5 USDC
const SETTLE_AMOUNT_1 = 100_000n; // 0.1 USDC
const SETTLE_AMOUNT_2 = 150_000n; // 0.15 USDC
const SLEEP_SECONDS = 25;
const REQUIRED_BALANCE = 6_000_000n; // subscribe (4) + top_up (2)

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

function fmtUsdc(amount: bigint): string {
  const sign = amount < 0n ? "-" : "";
  const abs = amount < 0n ? -amount : amount;
  const whole = abs / 1_000_000n;
  const frac = (abs % 1_000_000n).toString().padStart(6, "0");
  return `${sign}${whole}.${frac} USDC`;
}

function explorerUrl(sig: string): string {
  return `https://explorer.solana.com/tx/${sig}?cluster=devnet`;
}

function loadKeypairFromFile(filePath: string): Keypair {
  const raw = fs.readFileSync(filePath, "utf8");
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(raw) as number[]));
}

/**
 * Parse `PRIVATE_KEY=<value>` from `.env`. Auto-detects JSON byte-array
 * (Solana CLI) vs base58 string (Phantom/Backpack). Sanitises to longest
 * valid base58 prefix to tolerate trailing non-ASCII junk in hand-edited files.
 */
function loadSubscriberKeypairFromEnv(envPath: string): Keypair {
  if (!fs.existsSync(envPath)) {
    throw new Error(`subscriber .env not found at ${envPath}`);
  }
  const raw = fs.readFileSync(envPath, "utf8");
  const line = raw.split(/\r?\n/).find((l) => l.startsWith("PRIVATE_KEY="));
  if (!line) throw new Error(`PRIVATE_KEY= not found in ${envPath}`);
  let value = line.slice("PRIVATE_KEY=".length).trim();
  if (
    (value.startsWith('"') && value.endsWith('"')) ||
    (value.startsWith("'") && value.endsWith("'"))
  ) {
    value = value.slice(1, -1);
  }
  if (value.startsWith("[")) {
    return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(value) as number[]));
  }
  // base58 form — keep only valid base58 chars from the leading run to
  // tolerate trailing whitespace / non-ASCII junk in hand-edited .env files.
  const b58Alpha = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  let end = 0;
  while (end < value.length && b58Alpha.includes(value.charAt(end))) end++;
  const cleaned = value.slice(0, end);
  if (cleaned.length === 0) {
    throw new Error("PRIVATE_KEY value did not start with valid base58");
  }
  const bytes = Uint8Array.from(bs58.decode(cleaned));
  if (bytes.length !== 64) {
    throw new Error(`decoded PRIVATE_KEY is ${bytes.length} bytes; expected 64`);
  }
  return Keypair.fromSecretKey(bytes);
}

function loadIdl(): { idl: any; programId: PublicKey } {
  const idl = JSON.parse(fs.readFileSync(IDL_PATH, "utf8"));
  return { idl, programId: new PublicKey(idl.address) };
}

/** Anchor-error decoder (mirrors `05-top-up.ts`) — `Code (number)` form. */
function decodeAnchorError(err: unknown, idl: any): string {
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
      return `${knownNames.includes(name) ? name : `${name}?`} (${m[2]})`;
    }
  }
  return e?.message ?? String(err);
}

// --------------------------------------------------------------------------
// Pretty CLI output (TTY only). Auto-disables ANSI when piped to file.
// --------------------------------------------------------------------------

const TTY = process.stdout.isTTY === true;
const A = {
  reset: TTY ? "\x1b[0m" : "",
  bold: TTY ? "\x1b[1m" : "",
  dim: TTY ? "\x1b[2m" : "",
  underline: TTY ? "\x1b[4m" : "",
  red: TTY ? "\x1b[31m" : "",
  green: TTY ? "\x1b[32m" : "",
  yellow: TTY ? "\x1b[33m" : "",
  blue: TTY ? "\x1b[34m" : "",
  magenta: TTY ? "\x1b[35m" : "",
  cyan: TTY ? "\x1b[36m" : "",
  gray: TTY ? "\x1b[90m" : "",
};
function paint(code: string, s: string): string {
  return TTY ? `${code}${s}${A.reset}` : s;
}
/** Render a clickable URL. Plain URL text — every modern terminal (Terminal.app,
 *  iTerm2, VS Code, Wezterm, Hyper, gnome-terminal) makes complete URLs
 *  Cmd-click / Ctrl-click openable. OSC 8 hyperlinks are skipped — they break
 *  URL auto-detection in some bash terminals. */
function link(url: string, _label?: string): string {
  return paint(A.blue + A.underline, url);
}

/** Emit `[PHASE N] <ix>: <sig>` + explorer link + free-text + blank. Format
 *  is contractual — Phase 3 of `/demo-e2e` parses these lines verbatim. */
function printPhase(
  n: string,
  ixName: string,
  sig: string,
  notes: string[],
): void {
  const url = explorerUrl(sig);
  const phaseTag = paint(A.bold + A.cyan, `[PHASE ${n}]`);
  const ix = paint(A.bold + A.green, ixName);
  const sigShort = paint(A.gray, sig);
  console.log(`${phaseTag} ${ix}: ${sigShort}`);
  console.log(`  ${paint(A.dim, "link:")} ${link(url)}`);
  for (const note of notes) {
    console.log(`  ${paint(A.dim, note)}`);
  }
  console.log("");
}

// --------------------------------------------------------------------------
// main
// --------------------------------------------------------------------------

let lastSuccessfulPhase = "PRE";

async function main(): Promise<void> {
  // ── Setup ────────────────────────────────────────────────────────────────
  const merchant = loadKeypairFromFile(MERCHANT_KEYPAIR_PATH);
  const subscriber = loadSubscriberKeypairFromEnv(ENV_PATH);

  if (merchant.publicKey.toBase58() !== EXPECTED_MERCHANT_PUBKEY) {
    console.error(
      `merchant pubkey mismatch: got ${merchant.publicKey.toBase58()}, expected ${EXPECTED_MERCHANT_PUBKEY}`,
    );
    process.exit(1);
  }
  if (subscriber.publicKey.toBase58() !== EXPECTED_SUBSCRIBER_PUBKEY) {
    console.error(
      `subscriber pubkey mismatch: got ${subscriber.publicKey.toBase58()}, expected ${EXPECTED_SUBSCRIBER_PUBKEY}`,
    );
    process.exit(1);
  }

  const connection = new Connection(RPC_URL, COMMITMENT);
  const subscriberWallet = new Wallet(subscriber);
  const merchantWallet = new Wallet(merchant);
  const subscriberProvider = new AnchorProvider(connection, subscriberWallet, {
    commitment: COMMITMENT,
  });
  const merchantProvider = new AnchorProvider(connection, merchantWallet, {
    commitment: COMMITMENT,
  });

  const { idl, programId } = loadIdl();
  if (!programId.equals(PROGRAM_ID)) {
    console.error(
      `IDL program id ${programId.toBase58()} != hardcoded ${PROGRAM_ID.toBase58()}`,
    );
    process.exit(1);
  }
  const subscriberProgram = new Program(
    idl,
    subscriberProvider,
  ) as unknown as Program<Nakama>;
  const merchantProgram = new Program(
    idl,
    merchantProvider,
  ) as unknown as Program<Nakama>;

  const bar = "═".repeat(60);
  console.log(paint(A.bold + A.magenta, bar));
  console.log(paint(A.bold + A.magenta, " Nakama Protocol ") + paint(A.dim, "— full demo (devnet)"));
  console.log(paint(A.bold + A.magenta, bar));
  const programUrl = `https://explorer.solana.com/address/${programId.toBase58()}?cluster=devnet`;
  console.log(`${paint(A.dim, "Program:    ")} ${paint(A.cyan, programId.toBase58())}`);
  console.log(`${paint(A.dim, "Explorer:   ")} ${link(programUrl)}`);
  console.log(`${paint(A.dim, "USDC:       ")} ${paint(A.cyan, USDC_MINT.toBase58())}`);
  console.log(`${paint(A.dim, "Merchant:   ")} ${paint(A.cyan, merchant.publicKey.toBase58())}`);
  console.log(`${paint(A.dim, "Subscriber: ")} ${paint(A.cyan, subscriber.publicKey.toBase58())}\n`);

  // ── Pre-flight: subscriber USDC balance ──────────────────────────────────
  const subscriberAta = getAssociatedTokenAddressSync(
    USDC_MINT,
    subscriber.publicKey,
  );
  let subscriberBalance: bigint;
  try {
    const acc = await getAccount(connection, subscriberAta, COMMITMENT);
    subscriberBalance = acc.amount;
  } catch (err) {
    console.error(
      `subscriber USDC ATA not found at ${subscriberAta.toBase58()}.`,
    );
    console.error(
      `Fund it with at least ${fmtUsdc(REQUIRED_BALANCE)} (subscribe ${fmtUsdc(PRICE * BigInt(PERIODS_TO_PREFUND))} + top_up ${fmtUsdc(TOP_UP_AMOUNT)}).`,
    );
    console.error(`Underlying error: ${(err as Error).message}`);
    process.exit(1);
  }
  if (subscriberBalance < REQUIRED_BALANCE) {
    console.error(
      `subscriber USDC balance ${fmtUsdc(subscriberBalance)} < required ${fmtUsdc(REQUIRED_BALANCE)}`,
    );
    process.exit(1);
  }
  console.log(`Pre-flight: subscriber USDC = ${fmtUsdc(subscriberBalance)}`);

  // ── Pre-flight: merchant ATA exists (settle / charge destination) ────────
  const merchantAta = getAssociatedTokenAddressSync(
    USDC_MINT,
    merchant.publicKey,
  );
  let merchantAtaExists: boolean;
  try {
    await getAccount(connection, merchantAta, COMMITMENT);
    merchantAtaExists = true;
  } catch {
    merchantAtaExists = false;
  }
  if (!merchantAtaExists) {
    // Self-bootstrap: create merchant ATA so Plan / charge / settle have a
    // valid USDC destination on a fresh devnet wallet.
    const ix = createAssociatedTokenAccountInstruction(
      merchant.publicKey,
      merchantAta,
      merchant.publicKey,
      USDC_MINT,
      TOKEN_PROGRAM_ID,
      ASSOCIATED_TOKEN_PROGRAM_ID,
    );
    const tx = new Transaction().add(ix);
    let sig: string;
    try {
      sig = await merchantProvider.sendAndConfirm(tx, [merchant], {
        commitment: COMMITMENT,
      });
    } catch (err) {
      console.error(`merchant_ata init FAILED: ${decodeAnchorError(err, idl)}`);
      throw err;
    }
    console.log(`[PRE] merchant_ata_init: ${sig}`);
    console.log(`  link: ${explorerUrl(sig)}`);
    console.log(`  Created merchant USDC ATA ${merchantAta.toBase58()}.`);
    console.log("");
  } else {
    console.log(`Pre-flight: merchant_ata exists (${merchantAta.toBase58()})`);
    console.log("");
  }

  // ── PDAs ─────────────────────────────────────────────────────────────────
  const planId = new BN(Date.now());
  const [planPda] = derivePlanPda(programId, merchant.publicKey, planId);
  const [subscriptionPda] = deriveSubscriptionPda(
    programId,
    subscriber.publicKey,
    planPda,
  );
  const [vaultPda] = deriveVaultPda(programId, subscriptionPda);
  const [gracedPda] = deriveGracedSubscriptionPda(programId, subscriptionPda);
  const [pausedPda] = derivePausedSubscriptionPda(programId, subscriptionPda);

  // Random u64 session_id (collision probability 2^-64 — Q2 trade-off).
  const sessionId = new BN(crypto.randomBytes(7).toString("hex"), 16);
  const [paySessionPda] = derivePaySessionPda(
    programId,
    subscriptionPda,
    sessionId,
  );

  const subUrl = `https://explorer.solana.com/address/${subscriptionPda.toBase58()}?cluster=devnet`;
  console.log(paint(A.bold + A.yellow, "PDAs:"));
  console.log(`  ${paint(A.dim, "plan        ")} = ${paint(A.cyan, planPda.toBase58())} ${paint(A.gray, `(plan_id=${planId.toString()})`)}`);
  console.log(`  ${paint(A.dim, "subscription")} = ${paint(A.cyan, subscriptionPda.toBase58())} ${paint(A.gray, "← watch this account")}`);
  console.log(`  ${paint(A.dim, "  Explorer  ")} = ${link(subUrl)}`);
  console.log(`  ${paint(A.dim, "vault       ")} = ${paint(A.cyan, vaultPda.toBase58())}`);
  console.log(`  ${paint(A.dim, "pay_session ")} = ${paint(A.cyan, paySessionPda.toBase58())} ${paint(A.gray, `(session_id=${sessionId.toString()})`)}\n`);

  // ── Phase 1: create_plan (merchant) ──────────────────────────────────────
  {
    const ix = await (merchantProgram.methods as any)
      .createPlan(planId, new BN(PRICE.toString()), new BN(PERIOD))
      .accounts({
        merchant: merchant.publicKey,
        plan: planPda,
        tokenMint: USDC_MINT,
        merchantAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: PublicKey.default,
      })
      .instruction();
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "1";
    printPhase("1", "create_plan", sig, [
      `Plan PDA initialised. price = ${fmtUsdc(PRICE)}, period = ${PERIOD}s.`,
      `Merchant locked into snapshot — Subscription instances inherit price/period.`,
    ]);
  }

  // ── Phase 2: subscribe (subscriber, prefund 4 USDC) ──────────────────────
  {
    const ix = await (subscriberProgram.methods as any)
      .subscribe(PERIODS_TO_PREFUND)
      .accounts({
        subscriber: subscriber.publicKey,
        plan: planPda,
        tokenMint: USDC_MINT,
        subscription: subscriptionPda,
        vault: vaultPda,
        subscriberAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: PublicKey.default,
        rent: new PublicKey("SysvarRent111111111111111111111111111111111"),
      })
      .instruction();
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "2";
    const locked = PRICE * BigInt(PERIODS_TO_PREFUND);
    const ratePerSec = PRICE / BigInt(PERIOD);
    printPhase("2", "subscribe", sig, [
      `Subscriber locked ${fmtUsdc(locked)} (${PERIODS_TO_PREFUND} periods × ${fmtUsdc(PRICE)}) into vault PDA.`,
      `Subscription state = Active, rate_per_second = ${ratePerSec.toString()} µUSDC/s.`,
      `Vault: ${vaultPda.toBase58()}`,
    ]);
  }

  // ── Phase 3: top_up (subscriber, +2 USDC) ────────────────────────────────
  {
    // Post-Phase-2 state is Active → ADR-007 single-ix: pass `null` graced.
    const ix = await buildTopUpIx({
      program: subscriberProgram,
      subscriber: subscriber.publicKey,
      subscription: subscriptionPda,
      subscriberAta,
      vault: vaultPda,
      amount: TOP_UP_AMOUNT,
      state: SubscriptionState.Active,
    });
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "3";
    const total = (PRICE * BigInt(PERIODS_TO_PREFUND)) + TOP_UP_AMOUNT;
    printPhase("3", "top_up", sig, [
      `Subscriber added ${fmtUsdc(TOP_UP_AMOUNT)} → vault now holds ${fmtUsdc(total)} (3 periods of runway).`,
      `Stays in Active; deposited_amount += ${fmtUsdc(TOP_UP_AMOUNT)}.`,
    ]);
  }

  // ── Phase 4: sleep 65s (countdown) ───────────────────────────────────────
  {
    console.log(`${paint(A.bold + A.cyan, "[PHASE 4]")} ${paint(A.bold + A.yellow, "sleep")} ${paint(A.gray, `${SLEEP_SECONDS}s — letting streaming math accrue claimable balance...`)}`);
    const ratePerSec = PRICE / BigInt(PERIOD);
    const expectedClaimable = ratePerSec * BigInt(SLEEP_SECONDS);
    console.log(`  ${paint(A.dim, `expected claimable after sleep ≈ ${fmtUsdc(expectedClaimable)} (rate × elapsed)`)}`);
    for (let remaining = SLEEP_SECONDS; remaining > 0; remaining--) {
      // Color shifts cyan → yellow → red as the timer drains, signalling "almost done".
      const color = remaining > 30 ? A.cyan : remaining > 10 ? A.yellow : A.red;
      const tag = paint(A.bold + color, `T-${remaining.toString().padStart(2, "0")}s`);
      process.stdout.write(`  ${tag}    \r`);
      await new Promise((r) => setTimeout(r, 1000));
    }
    process.stdout.write("                       \r");
    lastSuccessfulPhase = "4";
    console.log(`  ${paint(A.green, "sleep complete.")}`);
    console.log("");
  }

  // ── Phase 5: charge (merchant=keeper) ────────────────────────────────────
  // ADR-004 §1: permissionless — any pubkey may sign as `payer`. ADR-007
  // BLK-007-MAJ-1 + production-keeper protocol: always attach graced PDA;
  // on-chain `init` fires only on the exhaustion-flip charge.
  {
    const ix = await (merchantProgram.methods as any)
      .charge()
      .accounts({
        subscription: subscriptionPda,
        plan: planPda,
        vault: vaultPda,
        merchantAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        payer: merchant.publicKey,
        gracedSubscription: gracedPda,
        systemProgram: PublicKey.default,
      })
      .instruction();
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "5";
    printPhase("5", "charge", sig, [
      `Permissionless trigger — merchant signed as keeper for demo simplicity (anyone could).`,
      `Vault → merchant_ata for the unlocked-but-unclaimed delta. State stays Active (no exhaustion).`,
    ]);
  }

  // ── Phase 6: open_session (subscriber) ───────────────────────────────────
  {
    // Demo: merchant doubles as facilitator. Production: separate off-chain
    // service authorised via this very `facilitator` field.
    const ix = await buildOpenSessionIx({
      program: subscriberProgram,
      subscriber: subscriber.publicKey,
      subscription: subscriptionPda,
      sessionId,
      facilitator: merchant.publicKey,
      reservationCap: new BN(RESERVATION_CAP.toString()),
    });
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "6";
    printPhase("6", "open_session", sig, [
      `PaySession opened with reservation_cap = ${fmtUsdc(RESERVATION_CAP)}.`,
      `Same parent vault as Phase 2 — ADR-x402-001: same escrow, two billing models.`,
      `Facilitator pubkey = ${merchant.publicKey.toBase58()} (merchant doubles as facilitator).`,
    ]);
  }

  // ── Phase 7: settle_usage #1 (merchant=facilitator) ──────────────────────
  // Wait 6s so streaming unlocks ≥ SETTLE_AMOUNT_1 since the Phase 5 charge
  // (which withdrew everything claimable up to that moment).
  console.log(`  pacing 6s before settle_usage #1 (waiting for streaming to unlock)...`);
  await new Promise((r) => setTimeout(r, 6000));
  {
    const ix = await buildSettleUsageIx({
      program: merchantProgram,
      facilitator: merchant.publicKey,
      subscription: subscriptionPda,
      sessionId,
      merchantAta,
      amount: new BN(SETTLE_AMOUNT_1.toString()),
    });
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "7";
    printPhase("7", "settle_usage", sig, [
      `Facilitator (merchant) settled ${fmtUsdc(SETTLE_AMOUNT_1)} for API call #1.`,
      `Shared accounting: parent.withdrawn_amount += ${fmtUsdc(SETTLE_AMOUNT_1)}.`,
    ]);
  }

  // ── Phase 8: settle_usage #2 ─────────────────────────────────────────────
  // Same pacing as Phase 7 — settle_usage #1 just consumed unlocked balance.
  console.log(`  pacing 6s before settle_usage #2...`);
  await new Promise((r) => setTimeout(r, 6000));
  {
    const ix = await buildSettleUsageIx({
      program: merchantProgram,
      facilitator: merchant.publicKey,
      subscription: subscriptionPda,
      sessionId,
      merchantAta,
      amount: new BN(SETTLE_AMOUNT_2.toString()),
    });
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "8";
    const totalSettled = SETTLE_AMOUNT_1 + SETTLE_AMOUNT_2;
    printPhase("8", "settle_usage", sig, [
      `Facilitator (merchant) settled ${fmtUsdc(SETTLE_AMOUNT_2)} for API call #2.`,
      `Cumulative session usage = ${fmtUsdc(totalSettled)} of ${fmtUsdc(RESERVATION_CAP)} cap.`,
    ]);
  }

  // ── Phase 9: close_session (subscriber) ──────────────────────────────────
  {
    const ix = await buildCloseSessionIx({
      program: subscriberProgram,
      subscriber: subscriber.publicKey,
      subscription: subscriptionPda,
      sessionId,
    });
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "9";
    printPhase("9", "close_session", sig, [
      `PaySession closed — rent returned to subscriber. Parent vault unaffected.`,
      `ADR-x402-001 R1 closure: close works regardless of parent FSM state.`,
    ]);
  }

  // ── Phase 10a: pause (merchant) ──────────────────────────────────────────
  // ADR-006: merchant is the pause authority. Verified pause.rs §`has_one =
  // merchant @ UnauthorizedPause` (line 30).
  {
    const ix = await buildPauseIx({
      program: merchantProgram,
      merchant: merchant.publicKey,
      subscription: subscriptionPda,
    });
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "10a";
    printPhase("10a", "pause", sig, [
      `Merchant paused subscription — streaming math freezes (charge refuses Paused).`,
      `PausedSubscription satellite created at ${pausedPda.toBase58()}.`,
    ]);
  }

  // ── Phase 10b: resume (merchant) ─────────────────────────────────────────
  {
    const ix = await buildResumeIx({
      program: merchantProgram,
      merchant: merchant.publicKey,
      subscription: subscriptionPda,
    });
    const sig = await merchantProvider.sendAndConfirm(
      new Transaction().add(ix),
      [merchant],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "10b";
    printPhase("10b", "resume", sig, [
      `Merchant resumed — stream_start shifted by pause_duration; subscriber loses no funds.`,
      `Satellite closed, rent → merchant (ADR-006 §"Symmetry").`,
    ]);
  }

  // ── Phase 11a: cancel (subscriber) ───────────────────────────────────────
  // ADR-009 polymorphic signer: subscriber cancels their own. Post-resume
  // state is Active → no satellites. cancel.rs declares BOTH gracedSubscription
  // AND pausedSubscription as trailing Option<Account>; the existing
  // buildCancelIx only emits gracedSubscription, so anchor TS auto-resolves
  // the missing pausedSubscription via IDL seeds and tries to load it
  // (AccountNotInitialized 3012). Explicit `pausedSubscription: null`
  // substitutes the program-id placeholder, which `allow-missing-optionals`
  // resolves to None on-chain.
  {
    const ix = await (subscriberProgram.methods as any)
      .cancel()
      .accounts({
        signer: subscriber.publicKey,
        subscription: subscriptionPda,
        subscriber: subscriber.publicKey,
        vault: vaultPda,
        merchantAta,
        subscriberAta,
        tokenProgram: TOKEN_PROGRAM_ID,
        gracedSubscription: null,
        pausedSubscription: null,
      })
      .instruction();
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "11a";
    printPhase("11a", "cancel", sig, [
      `Subscriber cancelled. Pro-rata settle to merchant + refund to subscriber, vault closed.`,
      `Subscription state = Cancelled (tombstone). Account preserved until cleanup.`,
    ]);
  }

  // ── Phase 11b: cleanup (subscriber) ──────────────────────────────────────
  // ADR-013 §Q1 — cleanup is subscriber-only (cleanup.rs §`has_one = subscriber`).
  // Spec said "merchant signs cleanup" but the handler disagrees; using subscriber.
  {
    const ix = await (subscriberProgram.methods as any)
      .cleanup()
      .accounts({
        subscription: subscriptionPda,
        subscriber: subscriber.publicKey,
      })
      .instruction();
    const sig = await subscriberProvider.sendAndConfirm(
      new Transaction().add(ix),
      [subscriber],
      { commitment: COMMITMENT },
    );
    lastSuccessfulPhase = "11b";
    printPhase("11b", "cleanup", sig, [
      `Subscription account closed; rent returned to subscriber (ADR-013 §Q1).`,
      `End-state: Plan persists, Subscription gone, vault closed, all PaySession satellites closed.`,
    ]);
  }

  const bar2 = "═".repeat(60);
  console.log(paint(A.bold + A.green, bar2));
  console.log(paint(A.bold + A.green, " ✓ Demo complete — all 11 phases landed on devnet."));
  console.log(paint(A.bold + A.green, bar2));
}

main().catch((err) => {
  const bar3 = "═".repeat(60);
  console.error("");
  console.error(paint(A.bold + A.red, bar3));
  console.error(paint(A.bold + A.red, ` ✗ DEMO FAILED at phase ${lastSuccessfulPhase} (last successful)`));
  console.error(paint(A.bold + A.red, bar3));
  // Reload IDL to decode error symbol — best-effort.
  let idlForDecode: any = null;
  try {
    idlForDecode = JSON.parse(fs.readFileSync(IDL_PATH, "utf8"));
  } catch {
    /* ignore */
  }
  if (idlForDecode) {
    console.error(`Anchor: ${decodeAnchorError(err, idlForDecode)}`);
  }
  console.error(err instanceof Error ? err.stack ?? err.message : String(err));
  const logs = (err as any)?.logs;
  if (Array.isArray(logs) && logs.length > 0) {
    console.error("Program logs:");
    for (const line of logs) console.error(`  ${line}`);
  }
  process.exit(1);
});