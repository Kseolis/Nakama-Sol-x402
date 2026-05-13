# Security Audit Report — Nakama Protocol

**Date**: 2026-05-13
**Scope**: nakama/ (yarn + cargo), clients/ts (npm), root workspace (cargo)
**Status**: Fixed (npm/yarn). Cargo findings deferred — only unmaintained warnings + 1 dev-transitive `unsound`, no CVE.

---

## nakama/ — yarn (TypeScript Anchor workspace)

Pre-fix: **10 vulnerabilities** (1 Low, 5 Moderate, 4 High).

Force-upgraded mocha transitive deps via `yarn` `resolutions` (mocha 9.x cannot
be bumped without breaking anchor scaffolding; resolutions are the surgical
path):

| Package              | Forced         | CVEs closed                                  |
|----------------------|----------------|-----------------------------------------------|
| `minimatch`          | `>=4.2.5`      | 3× ReDoS (high)                              |
| `serialize-javascript` | `>=7.0.5`    | RCE via `RegExp.flags` (high), DoS, XSS (mod) |
| `js-yaml`            | `>=4.1.1`     | prototype pollution in merge (mod)            |
| `nanoid`             | `>=3.3.8`     | predictable output on non-integer input (mod) |
| `uuid`               | `>=11.1.1`    | missing buffer bounds check v3/v5/v6 (mod)    |
| `diff`               | `>=5.2.2`     | DoS in parsePatch/applyPatch (low)            |

Resolved versions installed: minimatch 10.2.5, serialize-javascript 7.0.5,
js-yaml 4.1.1, nanoid 5.1.11, uuid 14.0.0, diff 9.0.0.

Post-fix: `yarn audit` → **0 vulnerabilities** (168 packages audited).

Smoke: `node_modules/.bin/mocha` (9.2.2) still loads and runs a trivial spec —
resolutions did not break runner.

## clients/ts — npm (SDK)

Pre-fix: `npm audit` already reported **0 vulnerabilities**. No action taken.

(Repo already contains separate working-tree changes to
`clients/ts/package.json` downgrading `@solana/spl-token` 0.4.9 → 0.1.8 —
unrelated to this audit and left intact.)

## Cargo — root + nakama/

`cargo audit` against both `Cargo.lock` files returned **zero CVE
vulnerabilities**. Only unmaintained-dependency warnings, all transitive
through Solana SDK 3.1.x / Anchor 1.0 / SPL token stack — not fixable at this
crate boundary.

### Root workspace (`/Cargo.lock`)
| Kind          | Crate     | Version | Advisory                |
|---------------|-----------|---------|--------------------------|
| unmaintained  | `bincode` | 1.3.3   | RUSTSEC-2025-0141        |

### Anchor workspace (`nakama/Cargo.lock`)
| Kind          | Crate          | Version | Advisory          |
|---------------|----------------|---------|-------------------|
| unmaintained  | `ansi_term`    | 0.12.1  | RUSTSEC-2021-0139 |
| unmaintained  | `bincode`      | 1.3.3   | RUSTSEC-2025-0141 |
| unmaintained  | `derivative`   | 2.2.0   | RUSTSEC-2024-0388 |
| unmaintained  | `libsecp256k1` | 0.6.0   | RUSTSEC-2025-0161 |
| unmaintained  | `paste`        | 1.0.15  | RUSTSEC-2024-0436 |
| unsound       | `rand`         | 0.7.3   | RUSTSEC-2026-0097 |

All six are transitive (Solana / Anchor / SPL). The `rand 0.7.3` soundness
issue is dev-only (LiteSVM test fixtures) and not on any production code
path. **Decision**: defer to post-MVP — they cannot be upgraded without
upstream releases from anza-xyz / coral-xyz.

## Verification

- `yarn audit --level moderate` → 0 vulnerabilities (nakama/)
- `npm audit --level moderate` → 0 vulnerabilities (clients/ts/)
- `cargo audit` → 0 CVE vulnerabilities (both workspaces), only unmaintained warnings deferred
- `cargo build` → green (both workspaces)
- Mocha 9.2.2 smoke test passes with forced resolutions

## Future work

- Track Solana SDK upgrades that drop `bincode 1.x`, `rand 0.7.x`, and
  `libsecp256k1 0.6.x` (likely SDK v4 cycle).
- Re-run this audit pre-demo (day 15) to catch any new advisories.
