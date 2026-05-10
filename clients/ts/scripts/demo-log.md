# Nakama Protocol — devnet demo run

- Program: `HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm`
- Cluster: devnet
- Run timestamp: 2026-05-10T15:24Z (UTC)
- Merchant / facilitator: `BeNSGCbNZxeGjuMg1dSCQbiuEK4mSdUeG1vT3h31Ly2w`
- Subscriber / keeper-as-permissionless: `EkCQAwbcH46VP7JvEPEnxy2Qqh1BNub7VwtpjXroXEDS`
- Plan params: price = 2 USDC, period = 60 s, prefund = 2 periods (4 USDC), top_up = +1 period
- Asset: devnet USDC (`4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU`)

## PDAs created during this run

| Name | Address |
|---|---|
| Plan | `CkHN5B7qu6dYJTLRyRfsgeKNrN9Rr9NMx2agECbkaBBA` (`plan_id = 1778426567831`) |
| Subscription | `2YVkPbCKFvdLzeq17dts7YFA2Hevdp78TEyeFBKpgaim` (closed in Phase 11b) |
| Vault | `HYecPCUoXCEUccHo1Jja3NMmSu9Mzvt1upwKkSM8JJgX` (closed in Phase 11a) |
| PaySession | `8ZWR8eBet8JwyJC7hDT2u9NHo9pT8dowxhecwsXqJMs7` (closed in Phase 9; `session_id = 17989379109023096`) |
| PausedSubscription satellite | `4JXe79MZZ82MY6MvRZSGKZPQYkPzuTPtcTcbTpmfs1ia` (created in 10a, closed in 10b) |

## Flow

| # | Instruction | Signer | What it proves | Tx |
|---|---|---|---|---|
| 1 | `create_plan` | merchant | Plan PDA initialised; price/period snapshot for inheritance by Subscriptions (ADR-014) | [`q3a7XU3...`](https://explorer.solana.com/tx/q3a7XU3rvX5XoQ8pguWSDXbi6fGPdZRdWyPf7NjeNcjQZhbreLDhdnabht1EUAY8ZyXHws2sHxMUnFnJNqaJohA?cluster=devnet) |
| 2 | `subscribe` | subscriber | Single signature locks 4 USDC into vault PDA; rate snapshot 33 333 µUSDC/s; state = Active (ADR-002) | [`2yuaaa6...`](https://explorer.solana.com/tx/2yuaaa6abSLy889Pr7MBSrrgXfGodNbyUnkTE3ajXux946xrFT3CJiJGkZhNG2dZKAJQ3iB6sLkWjRPxujT2phFU?cluster=devnet) |
| 3 | `top_up` | subscriber | Vault extended by +2 USDC (one period); state stays Active (ADR-007) | [`3a9Rg4r...`](https://explorer.solana.com/tx/3a9Rg4rXbs4gJknrWkJQTqGcumoea5qEbCJuatwkNiHbJecpkP5Mxnx15aKQxeCKX6FfMYULXDYd8ZDN5qTGSzuk?cluster=devnet) |
| 4 | _sleep 65 s_ | — | Streaming math accrues claimable balance (≈ 2.166 USDC) without any tx | — |
| 5 | `charge` | merchant (permissionless) | Vault → merchant_ata for unlocked-but-unclaimed delta; monotonic `withdrawn_amount` (ADR-004) | [`F4cwbpZ...`](https://explorer.solana.com/tx/F4cwbpZiLyYiKJCkuVZcrpXN6dZ4VmFmUSjKz2pMBP54SEJvLGyiH82DgEk2MBoZ99ia6VNTHaDkg2iKVy61QC6?cluster=devnet) |
| 6 | `open_session` | subscriber | PaySession satellite created with reservation_cap = 0.5 USDC; **same vault as Phase 2** (ADR-x402-001) | [`28URXbr...`](https://explorer.solana.com/tx/28URXbrWGVhDFabti9Vi49nHe8ZHwUhzwgBXN5dqbzHh34F472tvHA39afgdcYyASvXWAQ1Dx7yAJWK4DkkMjteJ?cluster=devnet) |
| 7 | `settle_usage` #1 | merchant (facilitator) | 0.1 USDC charged for API call #1; `parent.withdrawn_amount` mutated by x402 layer | [`3DEqDjW...`](https://explorer.solana.com/tx/3DEqDjWQ9Mkq5JugkYTVFjUC9c9AH2WipwFnqaP4zNXjdM5UZc7JYzQEXpF8nvx4FnAzgtJFogZpVKRsvGDDztBi?cluster=devnet) |
| 8 | `settle_usage` #2 | merchant (facilitator) | 0.15 USDC charged for API call #2; cumulative session usage 0.25 / 0.5 USDC cap | [`NpyFth7...`](https://explorer.solana.com/tx/NpyFth74c2VRZoGhH29EACg7wHruMcmTzGcVA5wRHdzAYVZD23obZxukc78zUeHwDzuC8GFPqkr4AVFLxQCrNhF?cluster=devnet) |
| 9 | `close_session` | subscriber | PaySession satellite closed; rent → subscriber; parent vault unaffected | [`4mRp1kq...`](https://explorer.solana.com/tx/4mRp1kq4sdy9x8RPKwqoLPbcaiUFkHBVwqSq7wrPvGV6V1ptBgfiiU24Yq6NdT8QXw97mtV6uijpMNeysEPHje2J?cluster=devnet) |
| 10a | `pause` | merchant | Time-frozen pause: PausedSubscription satellite created; charge refuses Paused (ADR-006) | [`uoqs62Z...`](https://explorer.solana.com/tx/uoqs62ZJMcNwEQaTiFZSiGcQuPCFc2zbtEsUomrjSsCBgPsMYfTqqFqKQvMctHtycWdDYvjzpCuDrshVfrscEx5?cluster=devnet) |
| 10b | `resume` | merchant | `stream_start += pause_duration`; subscriber loses no funds; satellite closed (ADR-006 §Symmetry) | [`2ArUX6o...`](https://explorer.solana.com/tx/2ArUX6o8jLkF1DwFnVfzmkDiEsD67eX3ASewqXLaWRJ9xHY1nf2tp2ccapht6mNhrYY29oi2kpGV7nrzvSNLqti4?cluster=devnet) |
| 11a | `cancel` | subscriber | Pro-rata final settle to merchant + refund of remainder to subscriber; vault closed; state → Cancelled (ADR-013) | [`4TXz68r...`](https://explorer.solana.com/tx/4TXz68r6Y2a2hjvhNYk7Aud3hz5hLGXhYDpyKvfSC7yEBvUQfKfyowm4NYPwqWUBDBtpQZ9NkmzsxKmxuW8R4Uxi?cluster=devnet) |
| 11b | `cleanup` | subscriber | Subscription account closed; rent → subscriber (ADR-013 §Q1) | [`4KTqjSu...`](https://explorer.solana.com/tx/4KTqjSuCq19b1VuS6C1yfJVpf1bJf2KJEve2XwD8itfhrKjhGMREAA9Q5DbMMTJgDo5hNwQ1mp99FjchPuq6mmfC?cluster=devnet) |

## Balance verification

- Subscriber USDC: pre-run 8.000000 → post-run **5.200028** (Δ = −2.799972). Spent 6.0 to vault, refunded 3.20 on cancel, net −2.80.
- Merchant USDC: pre-run 24.716622 → post-run **27.516594** (Δ = +2.799972). Received via charge (~2.166) + settle_usage ×2 (0.25) + cancel-time pro-rata (~0.384), totals match.
- Sum of subscriber loss = sum of merchant gain ⇒ vault is the single source of truth (ADR-002 invariant: `withdrawn_amount ≤ deposited_amount`).

## Notes for Loom

- Phases **2 → 5** (subscribe → wait → charge) are the **core subscription story** — single deposit, time-driven streaming withdrawal.
- Phases **6 → 9** (open_session → settle ×2 → close_session) are the **x402 layer** — same parent escrow, second writer, no double-deposit.
- Phases **10a/10b** (pause / resume) demonstrate **FSM completeness** with merchant-side pause authority and time-frozen continuity.
- Phases **11a/11b** demonstrate the **cancel decomposition** (settle+refund vs rent reclaim) introduced by ADR-013.
- For a tight 3-minute cut: phases 1, 2, 5, 6, 7, 8, 11a are the punch line; pause/resume can be omitted if pacing is tight.

## Run artifacts

- Raw stdout: `clients/ts/scripts/.demo-stdout.log`
- Driver script: `clients/ts/scripts/00-full-demo.ts`
