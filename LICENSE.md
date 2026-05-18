# Nakama Protocol — Licensing

This monorepo uses different licenses for different components.

## On-chain program — Apache License 2.0

Path: `nakama/programs/`
License: see [`LICENSE`](./LICENSE) at the repository root.

The Anchor program is open source under the Apache License 2.0.
You may fork, modify, audit, and deploy your own version freely,
subject to the Apache 2.0 terms (attribution, patent grant, no
trademark use).

## Off-chain services — Business Source License 1.1

Paths: `crates/nakama-client/`, `crates/nakama-x402-facilitator/`
License: see each crate's `LICENSE.bsl` file.

Off-chain services (the keeper bot and the x402 facilitator) are
source-available under BSL 1.1.

What this means:
- You may read, fork, and modify the source.
- You may use the software for non-commercial purposes, internal
  business operations, testing, and research.
- You may NOT offer the software (or substantial portions of it) as a
  competing commercial service.
- On the Change Date (2030-05-18), the BSL-licensed components
  automatically convert to the Apache License 2.0.

For commercial licensing inquiries before the Change Date,
contact the Licensor (see each `LICENSE.bsl` file).

## TypeScript SDK — MIT License

Path: `clients/ts/`
License: see [`clients/ts/LICENSE`](./clients/ts/LICENSE).

The TypeScript SDK is MIT-licensed for maximum adoption. Use it
freely in any commercial or non-commercial context.

## Trademark

"Nakama", "Nakama Protocol", "Nakama Labs", and any associated logos
are trademarks of Nakama Labs. The licenses above do not grant
permission to use these marks, except as required by attribution
clauses in the underlying licenses. You may not use them in a way
that implies endorsement or affiliation without prior written consent.

## Why dual licensing

- **Apache 2.0 on the on-chain program**: the program is deployed to a
  public blockchain — the bytecode and IDL are publicly observable
  on-chain regardless of license. Open licensing is the honest signal,
  enables ecosystem trust, and satisfies grant requirements.
- **BSL 1.1 on off-chain services**: keeper resilience, x402
  facilitator logic, and operational tooling are competitive
  differentiators. BSL allows full transparency for users and
  auditors while preventing direct commercial cloning, and
  auto-converts to Apache 2.0 after four years.
- **MIT on the SDK**: client SDKs benefit from maximum permissiveness.
  Adoption is the goal; restrictions only hurt integration velocity.
