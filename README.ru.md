# Nakama Protocol

*Один escrow, две модели биллинга на Solana.*

> Заявка на Solana Frontier hackathon · Colosseum · Track: Payments & Remittance · [English version](README.md)

Anchor-программа на Solana, где USDC-escrow финансируется один раз в момент подписки, а дальше из одного и того же родительского аккаунта параллельно списывают два независимых биллинговых слоя — recurring streaming subscriptions и x402 per-call micropayments — без double-spend. Одна подпись, один депозит, один источник истины.

---

## Что это

Nakama — это Anchor 1.0.1 программа для мерчантов, которым нужен одновременно предсказуемый месячный биллинг **и** оплата за вызов API из того же кошелька подписчика. Подписчик префандит N периодов USDC в `Subscription` PDA. Дальше два независимых писателя — permissionless keeper, дёргающий `charge` (Sablier-style streaming), и off-chain facilitator, дёргающий `settle_usage` (x402 micropayments) — оба обновляют одно и то же поле `withdrawn_amount`. Программа держит один инвариант: total withdrawn ≤ total deposited. Никакого второго approval кошелька, никакого второго escrow, никакой гонки между двумя биллинговыми поверхностями.

Целевой пользователь — мерчант хостед-API, который продаёт тариф $20/месяц **и** метрический overage в одном продукте.

## Зачем это существует

Streaming-subscription эскроу (Sablier, Superfluid) и x402 per-request micropayments (Coinbase x402, Solana facilitator stack) сегодня живут в разных контрактах. Мерчанту, которому нужны оба, приходится либо вести два on-chain соглашения с двумя отдельными approvals, либо строить custodial off-chain реконсилер. Проверено против публичной x402 spec (x402.org), референсной facilitator-реализации Coinbase и Sablier v2-Solana форка: ни одна задеплоенная Solana-программа не переиспользует один зафанденный escrow как source of truth для обеих моделей. Nakama — минимальная корректная версия этого примитива: одна PDA, два писателя, без double-spend.

Killer-line для питча: *одна Subscription PDA — зафандена один раз при подписке — кормит оба слоя: streaming subscriptions (Sablier-style, списываются keeper-ом) для предсказуемого месячного биллинга и x402 micropayments (per-API-call), которые сеттлит facilitator. Оба писателя обновляют один и тот же `parent.withdrawn_amount`. Никакого double-spend, никакого второго депозита, никакого второго approval кошелька.*

## Обзор архитектуры

```
                  Кошелёк подписчика (подписывает один раз — на subscribe)
                                  │
                                  ▼ префанд N × price USDC
       ┌────────────────────────────────────────────────────────┐
       │  Subscription PDA  (seeds: ["sub", subscriber, plan])  │
       │  ─────────────────────────────────────────────────────  │
       │  state · stream_start · prefunded_until                │
       │  withdrawn_amount   ← единый source of truth           │
       │  deposited_amount   ← растёт на subscribe + top_up     │
       └────────────────────────────────────────────────────────┘
                  ▲                                ▲
        charge_handler (keeper)          settle_usage (facilitator)
        ADR-004 streaming math           ADR-x402-001 reservation cap
        seconds-since-start × rate       PaySession satellite PDA
```

Оба писателя CPI-ят в один и тот же vault TokenAccount и атомарно бампают `parent.withdrawn_amount`. x402-слой добавляет `PaySession` сателлит — PDA на каждую активную сессию, ограничивающую полномочия facilitator-а — но средства там не лежат.

## Один инвариант

Обе биллинговые поверхности подчиняются одному правилу, которое проверяется на каждом пути списания (`charge`, `settle_usage`):

```
parent.withdrawn_amount + amount ≤ parent.deposited_amount
```

`deposited_amount` растёт только на `subscribe` и `top_up` (subscriber-signed). `withdrawn_amount` растёт только на `charge` (keeper) и `settle_usage` (facilitator). Оба писателя делают checked-add по одному и тому же полю; порядок между ними нерелевантен — каждая транзакция сеттлится против последнего закоммиченного состояния. Поэтому один escrow обслуживает две модели биллинга без координатора: координатор — это сама родительская PDA. Формальный аргумент развёрнут в ADR-x402-001 §"Composability with charge", а `tests/x402_settle_composability.rs` принудительно проверяет это под чередующимися транзакциями keeper-а и facilitator-а.

## Что собрано

| Поверхность | Статус | Файл |
|---|---|---|
| 11 инструкций | shipped | `nakama/programs/nakama/src/instructions/` |
| Subscription FSM (Active → GracePeriod → Cancelled / Exhausted, + Paused) | shipped | ADR-003, ADR-006 |
| Streaming charge с переходом в grace | shipped | ADR-004 |
| Top-up из Active и из GracePeriod | shipped | ADR-007 |
| Cancel подписчиком и мерчантом | shipped | ADR-002, ADR-009, ADR-013 |
| Pause / Resume с time-frozen continuity | shipped | ADR-006 |
| x402 PaySession (open, settle, close) поверх того же escrow | shipped | ADR-x402-001 |
| LiteSVM integration tests | 30+ файлов, 100+ тестов, зелёные | `nakama/programs/nakama/tests/` |
| TypeScript SDK (PDAs, computed status, instruction builders) | shipped | `clients/ts/src/` |
| Off-chain Rust клиент (account decoding, computed status) | shipped | `crates/nakama-client/` |
| x402 facilitator HTTP harness (axum) | shipped, demo-grade | `crates/nakama-x402-facilitator/` |
| Devnet live demo run (12 подтверждённых tx, все 11 инструкций) | shipped 2026-05-10 | `clients/ts/scripts/demo-log.md` |

Инструкции: `create_plan`, `subscribe`, `charge`, `cancel`, `cleanup`, `top_up`, `pause`, `resume`, `open_session`, `settle_usage`, `close_session`.

## Чем это **не является**

Чтобы снять очевидные вопросы заранее: это не generic streaming-payments контракт, конкурирующий с Sablier — у Sablier богаче streaming-поверхность, но он не хостит per-call биллинговый слой поверх того же escrow. Это и не drop-in замена x402 facilitator-у — Coinbase-овский facilitator agnostic к asset-у и мерчанту, а Nakama — мнениевая: USDC, devnet сегодня, один мерчант на `Plan`, подписка как предусловие открытия сессии. Контрибьюшн — в *композиции*, и она enforced on-chain.

## Demo flow

3-минутный Loom проводит одного подписчика через все FSM-переходы на одной и той же `Subscription`. Demo-числа (`price = 2 USDC`, `period = 60 s`, `prefund = 2 → 4 USDC`) намеренно маленькие, чтобы полный лайфцикл влез в три минуты; протокольная семантика идентична на production-масштабе (например, 10 USDC × 30 days).

1. **Мерчант** вызывает `create_plan` — регистрирует `price = 2 USDC`, `period = 60 s`, mint whitelist = devnet USDC.
2. **Подписчик** вызывает `subscribe` с `periods_to_prefund = 2` — 4 USDC переезжают wallet → vault, одна подпись; state = Active, `rate_per_second = 33 333 µUSDC/s`.
3. **Подписчик** вызывает `top_up` (+1 период) — vault теперь держит 6 USDC runway; state остаётся Active (ADR-007).
4. **Wait 65 s** — streaming-математика накапливает claimable balance (~2.166 USDC) без единой tx.
5. **Keeper** (любой signer) вызывает `charge` — vault → merchant_ata на unlocked-but-unclaimed дельту; монотонный `withdrawn_amount` (ADR-004).
6. **Подписчик** вызывает `open_session(facilitator, reservation_cap = 0.5 USDC)` — создаётся PaySession сателлит, средства не двигаются.
7. **Facilitator** вызывает `settle_usage(0.10 USDC)` — первое per-API-call списание; **тот же** writer `parent.withdrawn_amount`, что и в Phase 5 (ADR-x402-001).
8. **Facilitator** вызывает `settle_usage(0.15 USDC)` — второе списание; cumulative session usage 0.25 / 0.5 USDC cap.
9. **Подписчик** вызывает `close_session` — сателлит закрыт, рента возвращена; подписка живёт дальше.
10. **Мерчант** вызывает `pause`, затем `resume` — time-frozen pause-then-shift (ADR-006); подписчик не теряет средства.
11. **Подписчик** вызывает `cancel`, затем `cleanup` — pro-rata финальный settle мерчанту + refund остатка подписчику; vault и Subscription аккаунты закрыты (ADR-013).

Demo-драйвер: `clients/ts/scripts/00-full-demo.ts` (≤ 700 строк, один файл, без retry-логики). Детерминированный LiteSVM-эквивалент шагов 1–9 живёт в `nakama/programs/nakama/tests/x402_e2e_flow.rs`.

Соседние FSM-переходы покрыты вместе с happy path: pause+resume в середине стрима (`adr006_pause_resume.rs`), top-up из grace (`top_up_grace.rs`), cancel из любого состояния FSM (`cancel_*.rs`), passive grace-period expiry без вмешательства (`passive_grace_expiry.rs`), adversarial signer/ATA spoofing (`adversarial.rs`, `top_up_signer_guards.rs`), x402 parent-state guards (`x402_parent_state_guards.rs`).

## Live demo run

Прогон на devnet 2026-05-10. **Все 11 инструкций, 12 подтверждённых транзакций**, обе биллинговые поверхности работают с одним и тем же родительским vault. Роли: мерчант + facilitator = `BeNSGCbNZxeGjuMg1dSCQbiuEK4mSdUeG1vT3h31Ly2w`, подписчик = `EkCQAwbcH46VP7JvEPEnxy2Qqh1BNub7VwtpjXroXEDS`.

Две репрезентативные tx (полная таблица со всеми 12 sigs — в [`clients/ts/scripts/demo-log.md`](clients/ts/scripts/demo-log.md)):

- **Phase 2 — `subscribe`** (одна подпись, 4 USDC в vault): [`2yuaaa6...`](https://explorer.solana.com/tx/2yuaaa6abSLy889Pr7MBSrrgXfGodNbyUnkTE3ajXux946xrFT3CJiJGkZhNG2dZKAJQ3iB6sLkWjRPxujT2phFU?cluster=devnet)
- **Phase 7 — `settle_usage`** (x402-слой пишет в тот же vault, что streaming `charge`): [`3DEqDjW...`](https://explorer.solana.com/tx/3DEqDjWQ9Mkq5JugkYTVFjUC9c9AH2WipwFnqaP4zNXjdM5UZc7JYzQEXpF8nvx4FnAzgtJFogZpVKRsvGDDztBi?cluster=devnet)

Zero-sum инвариант проверен: подписчик потерял 2.7999 USDC, мерчант получил 2.7999 USDC за прогон. Родительский vault — единственный источник истины для обеих биллинговых поверхностей (ADR-002 §"Single escrow").

## Инженерные принципы

Кодбейз следует явному порядку приоритета: **GRASP → KISS → DRY → YAGNI → FSM-first.** Stateful-поверхности (Subscription, PaySession) смоделированы как enum-based FSM с матрицей переходов, проверяемой в тестах. Stateless-утилиты (PDA derivation, instruction builders, status reads) остаются чистыми функциями и не затаскиваются в FSM-церемонию. Каждое архитектурное решение сначала пишется как ADR, ревьювится отдельным architect-агентом, потом — security-auditor агентом, и только после этого мёржится. Дрейф между кодом и ADR — блокер, не follow-up.

Тесты — только LiteSVM: in-process, детерминированные, без подъёма валидатора. У каждой инструкции есть happy-path файл + invariants файл + adversarial файл. Матрица FSM-переходов enforced через name-mapped тестовые файлы (например, `cancel_from_grace.rs`, `adr006_cancel_from_paused.rs`).

## Попробуй сам

```bash
# Toolchain
rustup install 1.89.0
cargo install --git https://github.com/coral-xyz/anchor --tag v1.0.1 anchor-cli --locked
solana --version   # 2.x или новее

# Клон + билд программы
git clone <REPO_URL>
cd Nakama-Sol-x402/nakama
anchor build

# LiteSVM integration suite (валидатор не нужен)
cd programs/nakama
cargo test --release
# ожидается: 100+ passed; 0 failed

# Посмотреть задеплоенную devnet программу
solana program show HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm \
  --url https://api.devnet.solana.com

# Установить TS SDK (репо везёт package-lock.json — используй npm, не yarn)
cd ../../../clients/ts
npm install && npm run typecheck
```

Воспроизвести live e2e demo против задеплоенной devnet-программы:

```bash
# Из корня репо: положить ключ подписчика в .env (base58 строка ИЛИ JSON-массив байтов)
echo 'PRIVATE_KEY=<keypair-подписчика>' > .env

# Затем прогнать все 11 инструкций end-to-end (~2.5 мин wall-clock)
cd clients/ts
./node_modules/.bin/ts-node scripts/00-full-demo.ts
# ожидается: 12 подтверждённых devnet tx, balance check в конце
```

Подпись мерчанта берётся из дефолтного Solana CLI keypair-а (`~/.config/solana/id.json`), он же должен быть upgrade authority программы. Оба кошелька требуют devnet SOL (≥ 0.05) и ~6 devnet USDC на стороне подписчика; мерчант накапливает USDC за прогон.

Program ID (devnet): `HSbykjMFKgX4HhPBdBzDwMBrRVugatiCXrQEC1J9Ccfm` (последний upgrade в slot 461407181, 2026-05-10).
Asset: devnet USDC (`4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU`). Кластер по умолчанию: devnet.

## Структура проекта

```
Nakama-Sol-x402/
├── nakama/                          Anchor workspace
│   └── programs/nakama/
│       ├── src/
│       │   ├── lib.rs               #[program] поверхность — 11 инструкций
│       │   ├── state.rs             Subscription, PaySession, сателлиты, FSM enums
│       │   ├── error.rs             #[error_code] enum, никакого unwrap() вне #[cfg(test)]
│       │   ├── constants.rs         seeds, USDC mint, reservation/grace границы
│       │   └── instructions/        по файлу на handler (create_plan.rs, charge.rs, …)
│       └── tests/                   LiteSVM integration тесты (по файлу на поверхность)
├── crates/
│   ├── nakama-client/               off-chain Rust: PDA derivation, account decoding,
│   │                                computed_status (FSM read-replica для индексеров)
│   └── nakama-x402-facilitator/     axum HTTP harness — POST /settle, /open, /close
└── clients/ts/                      TypeScript SDK: pdas, instruction builders, types
```

## Tech stack

- Rust 1.89.0 (зафиксирован через `rust-toolchain.toml`)
- Anchor 1.0.1 (не 0.30.x — другой API)
- `anchor-spl` 1.0.1 для Token CPI
- LiteSVM 0.10.0 + granular Solana SDK v3 (`solana-message` 3.0.1, `solana-transaction` 3.0.2, `solana-signer` 3.0.0, `solana-keypair` 3.0.1)
- TypeScript 5.7.3, `@anchor-lang/core` 1.0.1 (Anchor TS bindings), `@solana/web3.js` 1.95+, `@solana/spl-token` 0.4.9, mocha 9 + ts-mocha 10 + chai 4
- Off-chain Rust сервисы: `solana-rpc-client` 3.1.x, `tokio` LTS, `axum` (facilitator harness)
- Workspace release профиль: `lto = "fat"`, `codegen-units = 1`, `overflow-checks = true`

AI dev tooling: Claude Code (Opus + Sonnet) с командой из 7 субагентов (architect, reviewer, anchor-engineer, off-chain Rust, SDK, test-engineer, security-auditor) и ADR pipeline, который пропускает каждое изменение через architect-reviewer + security-auditor до мёржа.

## Future work

Отложено из MVP, по одной строке причины на пункт:

- **Mainnet deployment.** Требует audited build, signer rotation и реального keeper-а — вне scope хакатона.
- **Token-2022 freeze handling.** Замороженные vault-ы детектятся, но flow восстановления не shipped; MVP опирается на vanilla SPL Token.
- **IDL versioning strategy.** Off-chain ридеры пинят одну версию IDL; forward-compat dispatch table набросан в ADR-001, но не реализован.
- **`getProgramAccounts` пагинация после 10k подписок.** Индексер рассчитан на маленький датасет; production требует Geyser-backed индексер.
- **Refund flow при merchant cancel-and-disappear.** Merchant cancel сейчас идёт по тому же settle/refund пути, что и subscriber cancel; adversarial кейсы задокументированы, но не stress-тестированы.
- **x402 facilitator HA.** Facilitator harness — single axum процесс; production требует N-of-M signing или stateless facilitator поверх очереди.
- **Variable-rate планы.** Математика расписана в ADR-005; instruction-поверхность отложена в post-MVP.
- **Mitigation Clock-drift.** Streaming-математика читает `Clock::unix_timestamp` напрямую; для Solana допустимо, но в production стоит харднить.
- **TS SDK `buildCancelIx` не emit-ит слот `pausedSubscription`.** Всплыло на e2e-демо: Anchor TS auto-resolve-ит trailing optional через IDL seeds и падает с `AccountNotInitialized 3012`, когда `PausedSubscription` отсутствует. Driver (`clients/ts/scripts/00-full-demo.ts`) обходит это прямым вызовом `program.methods.cancel()` с явным `pausedSubscription: null`. Фикс — однофайловое расширение SDK; отложен на post-hackathon, чтобы не размывать submission-day diff.

## Лицензия

MIT. См. `LICENSE`.

Заявка подана на Solana Frontier (Colosseum), трек Payments & Remittance, май 2026. Контакт — через форму submission на Colosseum.
