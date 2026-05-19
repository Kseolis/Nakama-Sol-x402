/**
 * F5-mirror (ADR-015 §F5): off-chain owner check + discriminator
 * dispatch helpers.
 *
 * The on-chain program is the only legitimate writer of accounts under
 * its program ID. RPC responses are untrusted by default (a hostile
 * proxy, a wrong-cluster connection, or a stale account that was
 * closed and reused under a different owner can all serve bytes that
 * Borsh-decode cleanly but mean nothing). Per ADR-015 §F5 every SDK
 * call site that pulls program-owned accounts MUST:
 *
 *   1. assert `account.owner.equals(programId)` BEFORE deserialization,
 *   2. validate the 8-byte Anchor account discriminator matches the
 *      expected account type.
 *
 * The Rust mirror lives at
 * `crates/nakama-client/src/accounts.rs::fetch_program_owned<T>` (built
 * by offchain-rust-dev in the same ADR cycle). Both implementations
 * share the same trust-boundary contract — owner first, discriminator
 * second, decode last.
 *
 * GRASP roles:
 *  - `decodeProgramOwnedAccount` — Pure Fabrication (a trust gate that
 *    composes with whichever Borsh/Anchor decoder the caller supplies).
 *  - `fetchProgramOwnedAccount` — Controller (knows how to ask the
 *    `Connection` for an account, dispatches to the pure gate).
 *
 * `@coral-xyz/anchor` (and the `@anchor-lang/core` shim in use here)
 * **does** validate the discriminator inside `program.coder.accounts.
 * decode("TypeName", data)` — but it does NOT validate `account.owner`.
 * That is the gap this helper closes.
 *
 * @see ADR-015 §F5 "Off-chain decoders trust RPC bytes without owner check"
 * @see ADR-x402-001 §"Facilitator role" (revised trust boundary)
 * @see crates/nakama-client/src/accounts.rs (Rust mirror)
 */

import { AccountInfo, Connection, PublicKey } from "@solana/web3.js";

/**
 * Anchor account discriminator length — `sha256("account:<Name>")[..8]`.
 * Mirror of `crates/nakama-client/src/constants.rs::ACCOUNT_DISCRIMINATOR_LEN`.
 */
export const ANCHOR_DISCRIMINATOR_LEN = 8;

/**
 * Failure surface for the F5-mirror trust boundary. Distinct variants
 * let callers log a precise reason and route differently (e.g. a
 * `WrongAccountOwner` on a known-good PDA usually means cluster
 * misconfig or a hostile RPC; an `AccountNotFound` is just lifecycle).
 */
export class AccountFetchError extends Error {
  constructor(
    message: string,
    public readonly kind:
      | "NotFound"
      | "WrongAccountOwner"
      | "WrongDiscriminator"
      | "TooShort"
      | "DecodeFailed",
  ) {
    super(message);
    this.name = "AccountFetchError";
  }
}

/**
 * Verify an `AccountInfo` is program-owned and (optionally) carries the
 * expected Anchor discriminator, then run the supplied decoder.
 *
 * Throws `AccountFetchError` with a precise `kind` on any failure.
 * Never returns a partially-validated value.
 *
 * The `expectedDiscriminator` is OPTIONAL because some call sites
 * (`@coral-xyz/anchor` `program.coder.accounts.decode(...)`) already
 * validate the discriminator inside their decoder. In that case pass
 * `null` and rely on the decoder. For hand-rolled / Borsh callsites,
 * pass the 8-byte discriminator buffer.
 *
 * @example Anchor decoder path — discriminator validated by decoder:
 * ```ts
 * const info = await connection.getAccountInfo(subPda);
 * const sub = decodeProgramOwnedAccount(
 *   info,
 *   PROGRAM_ID,
 *   null,
 *   (data) => program.coder.accounts.decode("Subscription", data),
 * );
 * ```
 *
 * @example Hand-rolled path — discriminator validated here:
 * ```ts
 * const info = await connection.getAccountInfo(paySessionPda);
 * const session = decodeProgramOwnedAccount(
 *   info,
 *   PROGRAM_ID,
 *   PAY_SESSION_DISCRIMINATOR,
 *   (data) => decodePaySessionBorsh(data.slice(8)),
 * );
 * ```
 */
export function decodeProgramOwnedAccount<T>(
  account: AccountInfo<Buffer> | null,
  expectedOwner: PublicKey,
  expectedDiscriminator: Buffer | null,
  decoder: (data: Buffer) => T,
): T {
  if (account === null) {
    throw new AccountFetchError(
      `Account not found (RPC returned null).`,
      "NotFound",
    );
  }
  if (!account.owner.equals(expectedOwner)) {
    // F5: owner check is the load-bearing assertion. Surfaces hostile
    // proxies, cluster misconfig, and closed-and-reused PDAs.
    throw new AccountFetchError(
      `Wrong account owner: expected ${expectedOwner.toBase58()}, ` +
        `got ${account.owner.toBase58()}. Refusing to deserialize.`,
      "WrongAccountOwner",
    );
  }
  if (expectedDiscriminator !== null) {
    if (account.data.length < ANCHOR_DISCRIMINATOR_LEN) {
      throw new AccountFetchError(
        `Account data shorter than 8-byte Anchor discriminator ` +
          `(got ${account.data.length}B).`,
        "TooShort",
      );
    }
    if (expectedDiscriminator.length !== ANCHOR_DISCRIMINATOR_LEN) {
      throw new AccountFetchError(
        `Expected discriminator must be exactly 8 bytes ` +
          `(got ${expectedDiscriminator.length}B). Programmer error.`,
        "WrongDiscriminator",
      );
    }
    const actualDisc = account.data.subarray(0, ANCHOR_DISCRIMINATOR_LEN);
    if (!actualDisc.equals(expectedDiscriminator)) {
      throw new AccountFetchError(
        `Account discriminator mismatch: expected ` +
          `${expectedDiscriminator.toString("hex")}, ` +
          `got ${actualDisc.toString("hex")}. Account is owned by the ` +
          `program but is not the expected type.`,
        "WrongDiscriminator",
      );
    }
  }
  try {
    return decoder(account.data);
  } catch (err) {
    // Wrap decoder exceptions in our typed error so callers don't need
    // to special-case Borsh / Anchor exception shapes.
    const reason = err instanceof Error ? err.message : String(err);
    throw new AccountFetchError(
      `Decoder failed after owner + discriminator validation: ${reason}`,
      "DecodeFailed",
    );
  }
}

/**
 * Fetch a program-owned account from RPC and validate
 * owner + discriminator before decoding. Wraps
 * `connection.getAccountInfo` + `decodeProgramOwnedAccount` for the
 * common single-account read.
 *
 * Returns the decoded value OR throws `AccountFetchError`. Use
 * `fetchProgramOwnedAccountNullable` if absence is a legitimate
 * outcome (e.g. checking for a satellite PDA).
 *
 * @example
 * ```ts
 * const sub = await fetchProgramOwnedAccount(
 *   connection,
 *   subPda,
 *   PROGRAM_ID,
 *   null, // anchor decoder validates discriminator
 *   (data) => program.coder.accounts.decode("Subscription", data),
 * );
 * ```
 */
export async function fetchProgramOwnedAccount<T>(
  connection: Connection,
  address: PublicKey,
  expectedOwner: PublicKey,
  expectedDiscriminator: Buffer | null,
  decoder: (data: Buffer) => T,
): Promise<T> {
  const info = await connection.getAccountInfo(address, "confirmed");
  return decodeProgramOwnedAccount(
    info,
    expectedOwner,
    expectedDiscriminator,
    decoder,
  );
}

/**
 * Like `fetchProgramOwnedAccount` but returns `null` when the account
 * does not exist (lifecycle absence). Owner / discriminator mismatches
 * still throw — those are *not* legitimate absences, they are RPC or
 * configuration faults.
 *
 * @example Checking for an optional satellite:
 * ```ts
 * const graced = await fetchProgramOwnedAccountNullable(
 *   connection,
 *   gracedPda,
 *   PROGRAM_ID,
 *   null,
 *   (data) => program.coder.accounts.decode("GracedSubscription", data),
 * );
 * if (graced !== null) { ... }
 * ```
 */
export async function fetchProgramOwnedAccountNullable<T>(
  connection: Connection,
  address: PublicKey,
  expectedOwner: PublicKey,
  expectedDiscriminator: Buffer | null,
  decoder: (data: Buffer) => T,
): Promise<T | null> {
  const info = await connection.getAccountInfo(address, "confirmed");
  if (info === null) return null;
  return decodeProgramOwnedAccount(
    info,
    expectedOwner,
    expectedDiscriminator,
    decoder,
  );
}
