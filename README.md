# Collateral Vault — the Soroban contract

A credit engine on Stellar. It **holds collateral, keeps a debt ledger, and allows liquidation**.
Nothing else. Collateral is never lent back out, so a customer's withdrawal never depends on
anyone else's borrowing.

This document is complete on its own — no backend, no front end, no product context is needed to
read it or to audit what is here. The contract is one half of [Vault Card](https://github.com/vault-card-official);
the other half consumes it and is not needed to review this.

```
collateral-vault/
├── src/
│   ├── lib.rs          entry points, the write discipline, the state loader
│   ├── types.rs        constants, Config, Position, Reserve, Grant, storage keys, view types
│   ├── errors.rs       the frozen error table
│   ├── events.rs       the event catalogue
│   ├── storage.rs      typed accessors, durability classes, TTL
│   ├── math.rs         every multiply and divide in the contract
│   ├── health.rs       valuation, LTV, health, liquidation price, risk band
│   ├── interest.rs     the borrow index and position settlement
│   ├── oracle.rs       SEP-40 adapter, staleness policy, outage guard
│   ├── liquidation.rs  close factor, seize, insolvency
│   ├── grant.rs        grant gates and headroom accounting
│   ├── invariant.rs    the assertions run at the end of every write
│   ├── idempotency.rs  the replay guard
│   └── test/           19 modules, 227 tests
├── mock-oracle/        settable SEP-40 feed — testnet only
├── mock-token/         settable-decimals SEP-41 token — testnet only
└── scripts/            deploy.sh, demo.sh
```

---

## 1. The shape: one market, many customers

**One contract instance per `(deposit_token, debt_token)` pair.** BTC/USDC is one market; BTC/USDT
is a different deployment with its own reserve, its own interest clock and its own customers.

Every customer who deposits that collateral shares the instance. Each holds a `Position` keyed by
their own address, draws on a revolving credit line through an operator they authorise, and is
liquidated permissionlessly if the price falls far enough.

Why this shape and not one vault per customer: an indexer can follow one contract, not a million.
And liquidation is only meaningfully permissionless when a third party has **one address to watch**
rather than a registry they must trust someone to publish.

There is no `owner` field on the contract, because there is no single customer.

### There is no administrator

`Config` is frozen at construction — **every field, `oracle` included**. No address can change a
threshold, swap the price feed, or upgrade the code. The complete list of privileges anyone holds:

| Role | Can do |
|---|---|
| **lender** | fund and withdraw the reserve · `lender_liquidate` (under the same gates as everyone) · freeze a grant · pull a pause that expires by itself after 7 days |
| **customer (owner)** | deposit · withdraw · propose, revoke, freeze and unfreeze their own grants |
| **operator** | accept or reject a grant offered to them · draw against an accepted one |
| **anyone** | repay · accrue · liquidate · reap an expired grant · report the oracle's state |

The lender is **not** an administrator. They cannot change a parameter and cannot touch a position
except through `lender_liquidate`, which obeys the same threshold, grace window and price checks
that a stranger obeys.

**The price of freezing `oracle` is real and stated rather than hidden**: if the feed dies
permanently, `draw` and `liquidate` stop working in this market forever. What keeps that survivable
is that `repay` reads no price, and `withdraw` reads none once a debt is clear — so the market can
always wind itself down and every customer can get their collateral out. The lender carries the loss
on anyone who declines to repay. Recalibrating risk parameters means deploying a new market and
inviting customers to migrate, which they do by repaying and withdrawing.

---

## 2. Units and scales

Three scales meet in this contract, and mixing them is the easiest way to be wrong by a power of
ten.

| Scale | Where | Notes |
|---|---|---|
| **deposit-token units** | `Position.deposit`, `seized`, `min_deposit_out` | needs a price to become money |
| **debt-token units** | `Position.principal`, `Position.interest`, `Reserve.*`, `Grant.cap`, `Grant.drawn`, `per_tx_limit`, `repay_amount` | the debt token is assumed worth exactly 1 USD |
| **internal USD, 7 decimals** | every gate, `vault_cap`, `dust_threshold`, `max_repay`, `seize_value`, `Valuation` | `USD_DECIMALS = 7` |
| **oracle price scale** | the feed's `price`, and `PositionView.liq_price` | read live from `oracle.decimals()` |

`USD_DECIMALS = 7` is chosen because it matches Stellar's default and USDC's, so `debt_value_usd`
is the identity on the path that matters most — the one with the fewest chances to be wrong.

The conversion factor that ties deposit units to USD:

```
shift = deposit_decimals + price_decimals − USD_DECIMALS      (may be NEGATIVE)

value_usd(amount)  = amount × price / 10^shift
from_usd(value)    = value × 10^shift / price
```

The reference fixture and the testnet deploy both use an **8-decimal** collateral token, a
**7-decimal** debt token and a **14-decimal** feed — `shift = 15`. That combination is deliberate: a
market where collateral and debt both have 7 decimals exercises none of the rescaling, and the
rescale is exactly where a power-of-ten error hides.

### Rounding, and which direction

Every conversion leans toward the vault.

| Quantity | Direction | Why |
|---|---|---|
| `market` (collateral → USD) | down | understate what backs a loan |
| `eligible` (after haircut) | down | same |
| `debt` (debt units → USD) | **up** | overstate what is owed |
| `from_usd` (USD → collateral) | down | a liquidator receives one unit less |
| `ltv_bps` (display) | up | shows the worse number |
| `health_bps` (display) | down | shows the worse number |
| `liq_price` (display) | up | warns earlier |
| interest per settlement | down, **with the remainder carried** | see §6 |

**Gates never divide.** `within_ltv`, `is_liquidatable` and `at_or_above_full_liq` cross-multiply:

```
debt × BPS  ≤  eligible × ceiling_bps
```

Computing an LTV first and comparing it would round by up to a basis point — on a 5000 bps ceiling
against $57,200 of eligible value, roughly $5.72 of borrowing power. That is the coarsest rounding
effect available anywhere in the contract, and it is entirely avoidable by not dividing.

### The arithmetic rules

Two rules the whole codebase depends on, both grep-able:

1. **No bare `*` or `/` on the money path outside `math.rs`.** (Bare `+`/`−` is allowed only where a
   comment argues the operands are bounded — `Reserve::free`, `interest::split`, `cap_headroom`.)
2. **Every `a × b / c` uses a 256-bit intermediate.** `quantity × price` overflows `i128` well
   inside plausible balances once token and price decimals are both large. `liq_price`'s numerator
   reaches ~1e57 before any division happens.

`overflow-checks = true` in the release profile is the backstop, not the mechanism: an overflow
panic yields an opaque host trap, whereas `math.rs` returns `4 MathOverflow`.

---

## 3. State

### Storage layout and durability

| Key | Class | Scope | Contents |
|---|---|---|---|
| `Config` | Instance | market | every risk parameter, frozen. Written once, by the constructor |
| `Paused` | Instance | market | `Option<u64>` — when the brake was pulled |
| `BorrowIndex` | Persistent | market | `IndexState { index, last_accrual }` |
| `Reserve` | Persistent | market | `{ total, committed, outstanding, bad_debt }` |
| `OracleGuard` | Persistent | market | `{ outage_at, grace_until }` |
| `Position(owner)` | Persistent | **customer** | `{ deposit, principal, interest, last_index, accrual_rem }` |
| `Grant(owner, operator)` | Persistent | **customer × operator** | the spending authority |
| `GrantOperators(owner)` | Persistent | **customer** | bounded `Vec<Address>`, ≤ 8 |
| `DrawSeen(draw_id)` | **Temporary** | market | the replay guard, 30-day TTL |

`Paused` is split out of `Config` for one reason: `Config` is frozen and this is not.

`DrawSeen` is **not** keyed by owner. A `draw_id` is unique across the whole market, and scoping the
guard per customer would let the same id be replayed against a different position.

`DrawSeen` is Temporary because it accumulates with every capture; Persistent would grow without
bound and could never be cleaned.

### `Position`, and why its default is not `Position::default()`

No position is written at construction. One appears the first time an address deposits, which may be
years after the market opened. Every field starts at zero **except `last_index`**, which must start
at the index as it stands *now*:

```
inc = (principal × (index − last_index) + accrual_rem) / INDEX_SCALE
```

`last_index = 0` bills the newcomer for every hour of interest the market has ever accrued.
`last_index = INDEX_SCALE` does the same thing anywhere the index has already risen above 1.0 —
which is everywhere after the first accrual. `storage::position()` seeds it from the live index,
which is why that getter takes `&Env` instead of being a `Default` impl.

### `Reserve` — the lender's capital, shared by the whole market

```rust
total       // the lender's claim: funded − withdrawn + interest collected − principal written off
committed   // promised to accepted grants and not yet drawn, summed over ALL customers
outstanding // PRINCIPAL disbursed and unpaid, summed over all customers — never interest
bad_debt    // cumulative written-off principal, increase only
free()      // total − committed − outstanding
```

`total` is **not** the token balance; the balance is lower by `outstanding`.

`committed` is what makes the product's central promise true — *a card transaction is never declined
because somebody else borrowed first* — because a customer's line is funded out of the shared pool
the moment the operator accepts it.

`outstanding` deliberately excludes interest. Including it would break invariant I1 from the passage
of time alone, with nobody acting, and the whole market would seize up at once.

### TTL

Persistent entries archive when their TTL runs out, and an archived `Position` means that customer
cannot withdraw. The failure needs no bug and no attacker: deposit, borrow, repay in full, walk away
for eight months.

```
TTL_THRESHOLD =  30 days of ledgers   extend only when it drops below this
TTL_EXTEND    = 120 days of ledgers   extend to this
DAY           = 17_280 ledgers        at ~5s each
```

`storage::bump(owner)` is step 10 of every write that touches a position. It extends the market's
shared state **plus the one customer this call named** — never every position. A market with ten
thousand positions doing ten thousand extensions per write would cost more as the customer base grew
until nobody could afford to transact. Every other position is refreshed by its own owner's next
call, and `accrue(owner)` is permissionless partly so anyone can do it for them cheaply.

`DrawSeen` is deliberately excluded: it is Temporary and gets its TTL once, at write time.

---

## 4. Configuration

Every field is set by `__constructor` and **there is no setter for any of them**.

```rust
pub struct Config {
    lender: Address,           // funds the reserve. NOT an administrator
    deposit_token: Address,    // SEP-41 collateral
    deposit_decimals: u32,     // READ FROM THE TOKEN CONTRACT, never trusted from the caller
    deposit_feed: Asset,       // SEP-40 lookup key — Other("BTC"), NOT the token address
    debt_token: Address,       // SEP-41 borrowed asset, assumed worth exactly 1 USD
    debt_decimals: u32,        // READ FROM THE TOKEN CONTRACT
    oracle: Address,

    haircut_bps: u32,          // discount on market value before anything may be borrowed
    max_ltv_bps: u32,          // protocol borrowing ceiling
    liq_threshold_bps: u32,    // at or above this LTV the position is liquidatable
    full_liq_ltv_bps: u32,     // at or above this the close factor is dropped
    liq_bonus_bps: u32,        // the liquidator's premium
    close_factor_bps: u32,     // the most of a debt one ordinary liquidation may repay
    price_max_age: u64,        // seconds

    rate_bps_year: u32,        // simple annual interest
    vault_cap: i128,           // market-wide ceiling on committed + outstanding. USD SCALE
    dust_threshold: i128,      // below this debt, a full close is always allowed. USD SCALE
}
```

**`vault_cap` and `dust_threshold` are USD figures**, while `Reserve` counts debt-token units. The
two coincide only when `debt_decimals == 7`. `accept_grant` rescales the exposure before comparing;
comparing them raw moved a market's ceiling by a power of ten for any other token, silently, because
the books stay internally consistent either way.

There is deliberately **no per-customer ceiling**. How the market's capacity is allocated between
customers is off-chain policy, and a limit the contract cannot see the reason for is a limit it will
eventually enforce wrongly.

### What the constructor validates

All of it runs once, and it is the last chance to catch a configuration that would hurt every
customer in the market. Any failure is `3 InvalidConfig`.

```
deposit_token ≠ debt_token
haircut_bps < 10000
max_ltv_bps > 0
max_ltv_bps < liq_threshold_bps < full_liq_ltv_bps < 10000
liq_bonus_bps < 10000
0 < close_factor_bps ≤ 10000
rate_bps_year ≤ 10000
price_max_age ≥ 60
vault_cap > 0
dust_threshold ≥ 0

full_liq_ltv_bps × (10000 + liq_bonus_bps) ≤ 10000²        ← the break-even constraint
```

**The break-even constraint** is the integer form of `full_liq_ltv ≤ 1/(1 + bonus)`. Above that
ratio, a partial liquidation *raises* LTV instead of lowering it and the position spirals — every
by-the-book liquidation pushing it closer to total loss while the liquidator collects the bonus
each time. Worked through in §8. Checking it at deploy stops a bad configuration before anyone loses
money rather than after.

The constructor then **overwrites `deposit_decimals` and `debt_decimals`** by calling `decimals()`
on the two token contracts. Trusting a caller-supplied value means one wrong digit makes every
valuation off by ten, and nothing downstream catches it because everything stays internally
consistent. `scripts/deploy.sh` passes `99` for both on purpose and asserts the stored values
afterwards.

**Deployment and construction happen in one transaction.** `__constructor` runs inside the deploy,
so there is no window in which somebody else can initialise the contract and name themselves the
lender.

### The reference parameter set

Used by the test fixture and `scripts/deploy.sh` — a bridged-BTC profile:

| Parameter | Value | Meaning |
|---|---|---|
| `haircut_bps` | 1200 | 12% discount on market value |
| `max_ltv_bps` | 5000 | borrow up to 50% of eligible value |
| `liq_threshold_bps` | 7000 | liquidatable at 70% |
| `full_liq_ltv_bps` | 9000 | whole debt closable at 90% |
| `liq_bonus_bps` | 800 | 8% premium |
| `close_factor_bps` | 5000 | half the debt per ordinary liquidation |
| `price_max_age` | 300 | five minutes |
| `rate_bps_year` | 1000 | 10% simple annual |
| `vault_cap` | $1,000,000 | market-wide |
| `dust_threshold` | $10 | |

Check: `9000 × 10800 = 97,200,000 ≤ 100,000,000`. ✓

These three — haircut, max LTV, liquidation threshold — are estimates until recalibrated against
real volatility data. They are written out explicitly rather than defaulted, because a market is
deployed with them frozen.

### Code constants — not configurable, deliberately

| Constant | Value | Why it is not in `Config` |
|---|---|---|
| `LIQUIDATION_GRACE` | 1800 s | so nobody can set it to zero after deployment |
| `MAX_PAUSE_SECONDS` | 7 days | bounds an unrenewed pause |
| `MAX_GRANT_SECONDS` | 90 days | a grant must not outlive its own storage entry |
| `MAX_GRANTS_PER_OWNER` | 8 | insolvency releases every grant in one call; unbounded makes that cost the customer's choice |
| `FUTURE_PRICE_SLACK` | 60 s | tolerated clock skew between feed and ledger |
| `DRAW_SEEN_TTL` | 30 days | the replay window |
| `USD_DECIMALS` | 7 | the internal scale |
| `INDEX_SCALE` | 1e18 | at 10%/yr the index moves ~3.17e9 units per second, far above the rounding unit |

`MAX_GRANT_SECONDS` is in **seconds** and `TTL_EXTEND` is a **ledger count**. They only look
compatible because of the ~5 s/ledger factor; the relationship being relied on is *90 days of
seconds < 120 days of ledgers*.

`MAX_GRANTS_PER_OWNER = 8` is enough for electricity, water, gas, telecoms, a card, a public-services
portal and two spare. A customer who needs a ninth revokes one.

---

## 5. The write discipline

Every write function follows the same ten steps, and three of them are load-bearing.

```
 1  require_auth on the correct role
 2  validate parameters
 3  accrue()                     must not read the price
 4  load state                   NAMING whose position
 5  guards, cheap → expensive    storage reads before cross-contract calls
 6  apply changes
 7  assert invariants
 8  emit event                   naming whose position
 9  token transfer               ALWAYS SECOND-TO-LAST
10  bump TTL                     ALWAYS LAST, and BOUNDED
```

**Step 9 second-to-last is the reentrancy mitigation.** A hostile or reentrant token sees a fully
consistent, fully committed state — there is no window in which the books say one thing and the
balances another.

**Step 5 ordering saves real money.** In `draw`, every grant gate is a storage read and the replay
check is a Temporary lookup, so a rejected draw never pays for accrual or a cross-contract oracle
round trip. The consequence is that `draw` runs `accrue` *after* those gates rather than at step 3,
and therefore takes care to run the LTV check against the **post-accrual** position.

**Step 10 must be bounded.** It touches the shared keys, the one position this call named, and that
customer's ≤ 8 grants — never every position in the market.

Two functions are deliberate exceptions, both for the same reason: they must work when everything
else is failing.

- **`pause`** asserts no invariants, reads no price, makes no external call and does not bump. With
  upgrades disabled it is the only escape from a discovered bug, so asserting here would make the
  brake fail exactly when it is needed.
- **`report_oracle_state`** likewise. Giving it a way to fail would mean the outage goes unrecorded
  precisely during an incident.

### Every entry point returns `Result`

`try_*` on the generated client yields `Err(Ok(Error::X))` only for functions declared to return
`Result`. With `panic_with_error!` a test can assert nothing more precise than "it failed", and the
backend branches on the exact number — a test that cannot tell `33` from `35` is not testing the
thing that matters.

---

## 6. Interest

### The index adds a rate; it does not compound a factor

```
advance:  index ← index + INDEX_SCALE × rate_bps × dt / (BPS × SECONDS_PER_YEAR)
settle:   inc   = (principal × (index − last_index) + accrual_rem) / INDEX_SCALE
```

The divisor is the **constant** `INDEX_SCALE`, not the position's own `last_index`. That makes the
sum telescope: the total over any span is exactly `principal × rate × elapsed`, **independent of how
often anybody calls anything**.

This matters because the vault is a market. Another customer's transaction advances the index
without settling my position, so my next settlement spans several index steps:

```
two steps, settled once:   P·((1 + r·dt₁)(1 + r·dt₂) − 1) = P·r·(dt₁+dt₂) + P·r²·dt₁·dt₂
two steps, settled twice:  P·r·dt₁ + P·r·dt₂
```

With a multiplicative index the cross term is real: two identical positions over identical spans owe
different amounts depending on how often each happened to settle — and since settling is
permissionless and free, a borrower could shave their own bill by calling `accrue` in a loop. An
additive index removes the product, so cadence cannot matter.

The trade is that this is **simple interest at an annual rate, not continuous compounding**, and
**unpaid interest never becomes principal**, so it does not itself accrue.

### Reference table — 1, 30 and 365 days

$10,000 of principal at 10%/yr, the reference parameter set. Every figure was computed out of band
and is asserted to the smallest unit, with no approximate comparison anywhere:

| Elapsed | Index | Interest owed |
|---|---|---|
| 1 day | `1.000273972602739726` | `27_397_260` = **2.7397260 USDC** |
| 30 days | `1.008219178082191780` | `821_917_808` = **82.1917808 USDC** |
| 365 days | `1.100000000000000000` | `10_000_000_000` = **1,000.0000000 USDC** |

The last row is the sanity anchor: 10% of $10,000 for one year is exactly $1,000.00, to the cent,
with no drift accumulated over 365 days of integer arithmetic. `principal` stays at $10,000 in all
three rows — interest is never folded in, which is what makes the 365-day figure the simple one.

Verify it with `cargo test exact_interest_at_one_thirty_and_three_hundred_sixty_five_days`, or read
it in [`src/test/interest_tests.rs`](src/test/interest_tests.rs). The same figures are restated
against the deck's sprint criterion in `deck_conformance::s2_interest_table_1_30_365`, so the two
have to agree.

### The clock does not move when the index does not

Integer division truncates. If `last_accrual` advanced on a call that produced no index movement,
that slice of time would be gone for good and anyone could hold the index still by calling in a
tight loop. `advance` leaves `last_accrual` alone unless `index` actually rose; the index is its own
memory.

### The carried remainder

`accrue` is permissionless. Without carrying the remainder of the settlement division, every call
throws away its fraction: on a $50,000 position accruing ~1585.7 units per second, rounding down
loses ~0.7 units per call, and a year of per-second calls suppresses ~0.044% of the lender's revenue
for the price of network fees. Carrying it bounds the cumulative error at **one smallest unit for
the life of the vault**.

`accrual_rem` is never discarded — except when `principal == 0`, where the position is moved forward
to the current index and the remainder cleared, so the first draw afterwards is not charged against
an ancient index.

### Payment order

`interest::split` pays **interest first, then principal**. It matches industry convention, drives
`interest` to zero leaving clean state, and makes "pay everything off" a single call of
`principal + interest`.

**Only the principal portion reopens a grant's headroom.** `cap` is a ceiling on principal, and a
line that shrank as interest accrued would be unpredictable for the customer.

Interest enters the books (`reserve.total += interest_paid`) only when actually paid — accrued-
but-unpaid interest is unrealised revenue, not capital, and needs no backing.

### When `Accrued` is emitted

When **anything the accrual clock owns actually changed**: the market index, or this position's
`last_index`, `accrual_rem` or `interest`. Two narrower conditions were tried and both left state
that could not be rebuilt from the event stream alone — `inc > 0` misses a position with no
principal whose clock still advances, and `moved || inc > 0` additionally misses the settlement that
clears `accrual_rem` when a debt is repaid in full inside the same ledger as the previous accrual.
The condition still skips the case worth skipping: a repeated call inside one ledger, on a position
already settled, moves nothing and emits nothing.

---

## 7. Credit math

```
market    = deposit × price / 10^shift                    round down
eligible  = market × (BPS − haircut_bps) / BPS            round down
debt      = (principal + interest) rescaled to USD        round UP

LTV       = debt / eligible                               display only, rounds up
health    = eligible × liq_threshold_bps / debt           display only, 10000 = the boundary exactly
```

Note the health formula has **no `BPS` in the denominator**. The liquidation boundary is
`debt × BPS == eligible × liq_thr`; substituting gives `eligible × liq_thr / debt == BPS == 10000`,
which is the definition. Dividing by `debt × BPS` instead yields `1` at the boundary — the wrong
form looks symmetrical and reads plausibly, which is why it is worth stating.

### Liquidation price

The oracle price at which the position becomes liquidatable, returned in **oracle price scale** so
it is directly comparable to what the feed reports:

```
P = debt × BPS² × 10^shift / (deposit × (BPS − haircut) × liq_thr)
```

Inverting the valuation reintroduces the `10^shift` factor that `value_usd` divided out; without it
the number is in no unit at all. Evaluated in three rounded-up steps, so it can overstate by a couple
of units — the safe direction, because a higher liquidation price warns earlier.

### Risk band — advisory only

| Band | Condition |
|---|---|
| `Liquidatable` | `ltv ≥ liq_threshold_bps` |
| `Warning` | `ltv > max_ltv_bps` |
| `Watch` | `ltv ≥ 0.9 × max_ltv_bps` |
| `Healthy` | otherwise |

**The `max_ltv` boundary belongs to `Healthy`**, not to `Warning`. The withdrawal gate permits
`ltv_after ≤ max_ltv`, so a band that said `Warning` at exactly `max_ltv` would tell the interface to
block something the contract allows.

### Worked example — the reference parameters

1 BTC deposited at $60,000:

```
market    = $60,000
eligible  = $60,000 × 0.88          = $52,800
borrowing = $52,800 × 0.50          = $26,400        the LTV ceiling
```

After drawing $10,000:

```
ltv_bps    = ceil(10,000 × 10000 / 52,800)   = 1894 bps
health_bps = 52,800 × 7000 / 10,000          = 36,960 bps
liq_price  ≈ $16,234                         market value $16,234 → eligible $14,286 → LTV 7000
```

At $16,000 the position is liquidatable: `eligible = $14,080`, `ltv = ceil(10^8/14,080) = 7103 bps`,
past the 7000 threshold and nowhere near the 9000 full-close point.

---

## 8. Liquidation

### The two routes

|  | `liquidate` | `lender_liquidate` |
|---|---|---|
| caller | anyone | lender only |
| close factor | enforced | **none** |
| bonus | `liq_bonus_bps` | **none** — seizes at market price |
| debt tokens in | yes, from the liquidator | **no** — the lender nets internally |
| `reserve.total` | `+= interest_paid` | `−= principal_paid`, **interest not credited** |
| insolvent position | handled, via the wipe-out branch | **rejected** with `50 PositionHealthy` |

Both routes pass the identical gates: debt exists, price is strictly valid, not inside the grace
window, `ltv ≥ liq_threshold`.

**Why the lender's `total` falls.** They receive collateral and write down their own claim by the
same amount. If `total` did not fall, the lender would take the collateral *and* keep the full
claim, collecting twice. Invariant I2 would catch it, but only after the fact.

**Why interest is not credited on the lender's route.** Every other route books
`total += interest_paid` because the payer's debt tokens actually arrived. Here nothing arrives: the
lender collected the whole `repay_amount`, interest included, as **collateral**. Crediting the
interest to the pool as well would claim backing that does not exist, and I2 —
`balance ≥ total − outstanding` — would fail by exactly `interest_paid`.

**Two bounds the original specification lacked**, both on the lender's route. Dropping the close
factor removes the only thing limiting `repay_amount`, and `total −= principal_part` would then
drive the claim negative — surfacing as `5 InvariantViolated`, a code that is supposed to mean
*there is a bug*. So the repayment is clamped to the debt. And insolvent positions are rejected,
because this route nets: there are no incoming tokens to measure a recovery against, so the
write-down would never be booked. Rejecting forces the lender through ordinary `liquidate`, where
`bad_debt` is recorded properly.

### The close factor and the toxic band

```
max_repay_usd = debt                                    if ltv ≥ full_liq_ltv_bps
              = debt                                    if debt < dust_threshold
              = debt × close_factor_bps / BPS           otherwise

seize_usd     = repay_usd × (BPS + liq_bonus_bps) / BPS         round down
seize         = min(from_usd(seize_usd), position.deposit)
```

A liquidation removes more collateral than the debt it clears, by exactly the bonus. Below a
threshold that still pulls LTV down; above it, LTV goes **up**:

```
eligible 10,000, debt 9,500 → LTV 95%
repay 50% = 4,750, seize 4,750 × 1.08 = 5,130
after: eligible 4,870, debt 4,750 → LTV 97.5%          ← worse
```

The break-even is `LTV = 1/(1 + bonus)` — 92.6% at an 8% bonus. Above it, every by-the-book
liquidation pushes the position closer to total loss while the liquidator collects another 8% each
time. `full_liq_ltv_bps` is set **below** break-even and the constructor enforces the relationship,
so a configuration that spirals cannot be deployed.

**The dust escape.** Below `dust_threshold` a full close is allowed regardless of LTV. Otherwise a
50% close factor leaves $3 remainders that no liquidator will bother to clear, and positions hang
open forever.

### Insolvency — the branch that must never revert

```
is_insolvent  ⟺  debt > 0 AND market < debt
```

**Market value, not eligible value.** The haircut is a buffer for *lending* decisions, not for
*recovery*. Triggering on `eligible` would declare insolvency while up to `haircut_bps` of collateral
remains — possibly enough to repay the debt in full — and write off principal that was never lost.

The branch takes everything and values it, capped at the debt:

```
seized     = position.deposit                        all of it
recovered  = min(market_value(seized), debt)
lost       = principal − principal_paid

position.deposit = principal = interest = 0
reserve.total       += interest_paid
reserve.outstanding −= principal_paid
reserve.total       −= lost                          ← the line the original sketch omits
reserve.outstanding −= lost
reserve.bad_debt    += lost
every grant released, then deleted
```

Three things are load-bearing:

1. **`total −= lost`.** Without it the books still record a lender claim on money that is genuinely
   gone. I1 still passes — `outstanding` fell too — so **only I2 sees the error**. That is precisely
   why I2 exists.
2. **Headroom is restored before the grants are revoked**, for the whole principal, both the
   recovered part and the written-off part. Revoking first would strand it and make `released`
   report a figure that never matched the books.
3. **Every grant this customer holds is released in one call.** This is why `MAX_GRANTS_PER_OWNER`
   exists — the walk is bounded, so the cost of liquidating a position cannot be inflated by its
   owner. Leaving any grant behind would strand its `committed`: capital promised to a line whose
   position no longer exists, which nobody could ever release.

If this branch reverted for any reason internal to the contract — a subtraction below zero, a failed
assert, an unchecked panic — the position would be stuck forever: nobody could close it, the
collateral could not be withdrawn, the debt could not be cleared. It is the single most serious
mistake available in this contract, which is why every clamp here is explicit rather than inherited.

The only revert it does have is `52 SlippageExceeded`, from the caller's own `min_deposit_out`, and
that is safe because anyone else can call with zero.

### No headroom is restored by either liquidation route

A liquidator is a stranger to the customer's arrangements and cannot say which of several
authorities should reopen — and a forced repayment reopening somebody's spending power is not
obviously right even if it could. Both `committed` and `outstanding` stay put, so I1 is unaffected.

The `Liquidated` event has no `headroom_restored` field at all, rather than one pinned at zero,
because a number that can only be zero is a number somebody will eventually read as meaningful.

---

## 9. The oracle

SEP-40. `oracle.rs` is the **only** module allowed to construct an `OracleClient` — that is how
"read the price at most once per invocation" is enforced by a module boundary rather than by a
comment someone will eventually ignore.

Why it matters: `liquidate` needs a price in two places, the threshold check and the seize
computation. The oracle is an external contract with no guarantee of determinism inside one
transaction, so two independent reads can return two different values. The gate would pass on one
price and the seize would settle on another, and nothing would catch it, because each step is
individually correct. The mock oracle has a **drift mode** for exactly this test: every `lastprice`
moves the stored price *after* answering, so a second read within one invocation returns a different
number.

### The freshness predicate — one definition, four checks

```
price ≤ 0                                        → 41 PriceInvalid
timestamp > now + FUTURE_PRICE_SLACK (60 s)      → 41 PriceInvalid
now − timestamp > price_max_age                  → 40 PriceStale      (boundary inclusive: == is valid)
lastprice returned None                          → 41 PriceInvalid
```

`report_oracle_state` uses the **same four**. If the guard used a looser test — say presence and age
only — a feed running ten minutes fast would count as fresh there and be rejected here. The guard
would never record an outage, the grace window would never open, and liquidation would stay blocked
with no way to clear it.

### Strict and non-strict

| Path | Callers | Bad price |
|---|---|---|
| **strict** | `withdraw` (only with debt), `draw`, `liquidate`, `lender_liquidate` | the call reverts with `40` or `41` |
| **non-strict** | `get_position`, `quote_draw`, `quote_liquidation` | zeroed valuation, `price_stale = true`, `age` reported |

**Views never panic.** A screen has to render during an outage. The non-strict path collapses "no
price at all" and "price rejected" into one `stale` flag, which is why a quote reports `40` even
where the strict path would have raised `41`.

### The outage guard and the grace window

```rust
OracleGuard { outage_at, grace_until }
```

`report_oracle_state` is permissionless and never fails:

- feed unusable and `outage_at == 0` → record `outage_at = now`, emit `OracleOutage`
- feed usable and `outage_at != 0` → `grace_until = now + 1800`, clear `outage_at`, emit
  `OracleRecovered`
- otherwise → nothing

Both liquidation routes then reject with `53 LiquidationGrace` while `now < grace_until`.

**Why it must be its own entry point** rather than a side effect of the ordinary price path: that
path *reverts* on a bad price, and the state write recording the outage would be rolled back along
with the failed call.

**Why the window exists at all.** A stale price blocks liquidation, but nothing said what happens
when the feed comes back. Without it: the oracle dies for four hours, the market moves 40%, and the
instant it recovers every over-threshold position in the market is liquidated at a price nobody had
a chance to react to.

---

## 10. Grants — the spending authority

A grant is one customer's authority for one operator to create debt against their position. A
customer may hold up to **eight at once** — an electricity supplier, a card issuer, a
public-services portal — each with its own cap, per-transaction limit, expiry and settlement
address. The key is the **pair** `(owner, operator)`.

```rust
pub struct Grant {
    operator: Address,
    settlement: Address,   // PINNED at grant time. There is no setter
    cap: i128,             // ceiling on principal created under this authority
    drawn: i128,           // created and still outstanding. Falls as principal is repaid
    per_tx_limit: i128,
    expiry: u64,           // unix seconds, ≤ 90 days out
    max_ltv_bps: u32,      // the customer's own ceiling, ≤ the market's
    frozen: bool,
    accepted: bool,
}
```

### Two-party, because two things are unknowable on chain

A grant is created **pending**. The customer names an operator, but naming somebody is not the same
as them agreeing to be named, and two things only the operator knows make the difference:

- **The blast radius of their own key.** The sum of caps granted to an operator is what moves if that
  key is stolen. Letting customers enlarge it unilaterally lets them enlarge the target on somebody
  else's key.
- **`settlement` is chosen by the owner**, and the contract can only check that it is not the
  operator themselves. Whether it is an address the operator actually *controls* is unknowable here
  — there is no registry on chain and there cannot be one without governance. An operator that draws
  against a grant settling somewhere they do not control has delivered goods and been paid to a
  stranger.

So the operator checks the terms off chain and calls `accept_grant`, and **only then is capital
committed from the reserve**. A pending grant reserves nothing, blocks nothing, and can be revoked by
the owner or rejected by the operator at any time.

### `settlement` is pinned, and `draw` has no destination parameter

`draw` reads `grant.settlement` instead of taking a destination. That turns a business rule into a
property of the type: **there is no way to write the call such that funds reach anywhere else.** A
compromised operator key can still only move money to the addresses each customer signed for, and
only up to each customer's own cap. Changing it means revoking and re-granting.

`settlement == operator` is rejected at grant time — otherwise a compromised operator simply pays
itself and the entire guarantee is void.

### An existing grant is rejected, never overwritten

`Grant.drawn` lives inside the struct. Overwriting resets it to zero:

```
cap 50,000, drawn 45,000, headroom 5,000
grant_authority(same terms) → drawn 0, headroom 50,000
total principal creatable: 95,000 against a 50,000 cap
```

Changing terms therefore takes two transactions — revoke, then grant. Slower, but there is no path
by which `drawn` disappears.

### Where each rule is checked, and why

| Rule | Checked at | Why there |
|---|---|---|
| R4 `grant.max_ltv ≤ config.max_ltv` | `grant_authority` (`46`) | depends on nothing shared — a customer proposing impossible terms learns it immediately |
| R2 `reserve.free ≥ cap` | `accept_grant` (`20`) | a proposal commits no capital; the reserve can drain between proposal and acceptance |
| R5 `committed + outstanding + cap ≤ vault_cap` | `accept_grant` (`22`) | same |
| R1 grant LTV ceiling | `draw` (`35`) | needs a fresh price |
| R3 protocol LTV ceiling | `draw` (`11`) | same |

`grant_authority` reads **no price**. Whether a cap is sensible against current collateral is
off-chain judgement, and the LTV gate at `draw` enforces the real limit against a fresh price
anyway. The consequences are deliberate: a line can be granted while the feed is down, and a
customer can authorise spending before depositing.

### Ending a grant

| Call | Caller | Effect |
|---|---|---|
| `reject_grant` | operator | deletes a **pending** proposal. Commits and releases nothing |
| `revoke_authority` | owner | deletes any grant, releases `cap − drawn` from `committed` |
| `reap_expired` | **anyone** | same, once `now ≥ expiry` |
| `freeze_grant` | owner **or lender** | blocks `draw` without destroying the grant |
| `unfreeze_grant` | owner | reverses it. Blocked by pause |

`reject_grant` exists for one reason: it tells the customer **why** nothing happened. "Declined" and
"still waiting" look identical from the outside, and a customer who cannot tell them apart waits for
something that is never coming. Once accepted, an operator has no unilateral exit — capital is
committed and the customer may have drawn on it. That is the customer's to end.

`revoke_authority` is the fire alarm: it reads no price, is not blocked by pause, and has no failure
condition beyond "no grant". *A fire alarm a dead oracle can disable is not a fire alarm.* Calling it
twice is safe — the entry is **deleted** rather than flagged, so the second call finds nothing and
releases nothing a second time.

`reap_expired` is permissionless because without it an expired grant still locks `committed` —
nothing runs when time passes — and only that customer can revoke. If they disappear, the lender's
capital is locked forever, and in a market vault it is capital that would otherwise back somebody
else's line. Permissionless is safe because the precondition is objective.

`freeze_grant` admits the lender because they bear the risk too and may spot an anomaly first. Revoke
is the blunt alternative: it destroys `cap`, `drawn` and `settlement` and forces the customer to
re-sign from scratch, and a user made to do that over false alarms stops responding to alarms. One
customer has no say whatever over another's grant.

A **pending** grant releases nothing on revocation, because `accept_grant` is what commits and it
never ran. Releasing `cap − drawn` for one would subtract capital that was never added and drive
`committed` below what the live lines actually promise.

### Headroom, and why the payer names the grant

`repay(owner, from, amount, operator: Option<Address>)`.

`None` repays the debt and reopens nothing — right for a third party who has no view of the
customer's arrangements, and for principal whose grant has already been revoked.

Naming it is how several grants become possible without a policy. The alternative is deciding,
inside the contract, whether a repayment reopens the oldest line or the largest or the one drawn most
recently — and every answer needs a loop over the customer's grants on the money path.

`restore_headroom` clamps to `min(principal_paid, grant.drawn)`, because `position.principal` can
exceed the current grant's `drawn`: grant A draws 30,000 and is revoked, grant B draws 10,000,
principal is 40,000 but `grant_B.drawn` is 10,000. Without the clamp, `drawn` goes negative.

**A named grant that no longer exists reopens nothing, and the repayment still lands.** The tempting
alternative is `30 GrantNotFound`, so a payer expecting a line back learns it did not come back — but
it is the wrong trade. Repayment is the risk-reducing action and is never blocked. Reverting a real
repayment because an *authority* vanished leaves a customer owing money they tried to pay: a standing
instruction repaying nightly would simply start failing the day the customer revoked, and the
position would drift toward liquidation. The payer is still told — `headroom_restored` is 0 in the
`Repaid` event.

### The replay guard

`draw` takes a `draw_id: BytesN<32>`. Card systems retry; a blockchain does not deduplicate. Without
this, one network timeout creates the debt twice — and the guard has to live where the final state
lives, because a `UNIQUE` constraint in an off-chain database cannot see a transaction whose outcome
that database never learned.

Written once with a **30-day** TTL and never extended. Thirty rather than twenty-four hours because
an operations queue can have an `unknown` state whose only exit is a human, and humans have weekends;
a guard that expires before the operator gets to the queue is a guard that lets the duplicate
through.

Checked **before** the price read: a Temporary lookup is far cheaper than a cross-contract call, and
a duplicate is not worth paying oracle gas to discover. A repeated `draw_id` returns `36
DuplicateDrawId`, which callers should treat as success, not failure.

---

## 11. Invariants

| | Statement | Asserted |
|---|---|---|
| **I1** | `total ≥ committed + outstanding` | every write, in `invariant::check` |
| **I2** | `debt_token.balance(contract) ≥ total − outstanding` | on chain in `withdraw_reserve`; in tests after every scenario |
| **I3′** | `Δoutstanding == Δprincipal` for the call | every write |
| **I4** | no field negative; `index ≥ INDEX_SCALE` | every write |

**Why I3 became I3′.** When this vault served one customer, I3 was an equality anyone could check in
one line: `reserve.outstanding == position.principal`. It was the strongest cheap check in the
design, and it was true **only** because the state was singular. With one position per customer it
becomes `outstanding == Σ principal`, and no single call can evaluate that sum — the contract does
not know who its customers are, by design, because enumerating them on chain is what makes a market
vault stop scaling.

The replacement is the same statement in **differential** form, which every call *can* evaluate. If
every call preserves the delta and the market starts at `0 == 0`, the sum is preserved by induction.
That catches the identical class of bug at the identical place — a path that moves one side without
the other — and it is what accounting mistakes in this contract have actually looked like. What it
cannot catch is drift introduced outside a call, which nothing on chain could catch anyway. The
absolute form moves into the test suite, where `check_books` sums `principal` over every owner a
scenario created and compares it with `outstanding`.

**Why I2 lives outside `invariant.rs`.** It needs a cross-contract balance call, so it sits in
`withdraw_reserve` — the one function where a discrepancy turns directly into a loss. It is also the
only invariant that can see a **missing transfer**: the internal checks cannot see balances, and that
is exactly where a missing transfer hides, with the accounting staying perfectly self-consistent
while the tokens are somewhere else.

A `draw` preserves I1 as an **equality**: `committed` falls by exactly what `outstanding` rises by.
Capital changes drawer without leaving the market.

`5 InvariantViolated` **must never appear**. If it does: halt and investigate, do not retry.
Returning a named code rather than letting an arithmetic overflow panic is the point — an opaque host
trap carries none of that meaning.

---

## 12. Entry points

### Mutating

| Function | Auth | Pause | Price | Notes |
|---|---|---|---|---|
| `__constructor(config)` | deploy tx | — | — | runs inside the deploy. Reads decimals from the tokens |
| `deposit(owner, amount)` | owner | **no** | none | risk-reducing, so never blocked |
| `withdraw(owner, amount)` | owner | yes | strict, **only if debt > 0** | post-checks LTV |
| `grant_authority(owner, operator, settlement, cap, per_tx_limit, expiry, max_ltv_bps)` | owner | yes | none | creates a **pending** grant |
| `accept_grant(owner, operator)` | **operator** | no | none | **commits capital**. R2 and R5 here |
| `reject_grant(owner, operator)` | **operator** | no | none | pending only |
| `revoke_authority(owner, operator)` | owner | no | none | the fire alarm |
| `freeze_grant(owner, operator, by)` | owner **or lender** | no | none | accepted grants only |
| `unfreeze_grant(owner, operator)` | owner | yes | none | accepted grants only |
| `reap_expired(owner, operator)` | **anyone** | no | none | after `expiry` |
| `fund_reserve(amount)` | lender | no | none | |
| `withdraw_reserve(amount)` | lender | no | none | **asserts I2 on chain** |
| `draw(owner, operator, draw_id, amount)` | **operator** | yes | strict | the only function that creates debt |
| `repay(owner, from, amount, operator?)` | `from` — **anyone** | no | none | interest first, then principal |
| `accrue(owner)` | **anyone** | no | **never** | also the cheap TTL keep-alive |
| `liquidate(owner, liquidator, repay_amount, min_deposit_out) -> i128` | **anyone** | no | strict | returns collateral seized |
| `lender_liquidate(owner, repay_amount) -> i128` | lender | no | strict | no bonus, no debt tokens in |
| `pause()` | lender | — | none | lapses by itself after 7 days |
| `unpause()` | lender | — | none | |
| `report_oracle_state() -> bool` | **anyone** | no | `is_fresh` | never fails |

### Views — none of them panic on a bad price

| Function | Returns |
|---|---|
| `get_config()` | the frozen `Config` |
| `is_paused()` | `bool` — `false` once a pause has lapsed, with nobody calling `unpause` |
| `paused_at()` | `Option<u64>` |
| `get_grant(owner, operator)` | `Option<Grant>` |
| `get_grant_operators(owner)` | `Vec<Address>`, ≤ 8 |
| `get_reserve()` | `ReserveView { total, committed, outstanding, free, bad_debt }` |
| `get_position(owner)` | `PositionView` — everything one position screen needs, in one call |
| `quote_draw(owner, operator, amount)` | `DrawQuote` |
| `quote_liquidation(owner)` | `LiquidationQuote` |

`get_position`, `quote_draw` and `quote_liquidation` advance and settle a **copy** of the position
first, so the figures they report are current rather than as-of-the-last-write.

### `draw` — the gate order is the specification

The backend branches on which code comes back, so a call violating several conditions must return a
deterministic one. Both `draw` and `quote_draw` check them in this order, and any divergence is a bug
in the quote:

```
  1  amount ≤ 0                        → 12 InvalidAmount
  2  paused                            → 13 Paused
  3  no grant for (owner, operator)    → 30 GrantNotFound
  4  grant.operator ≠ caller           → 30 GrantNotFound
  5  not accepted                      → 48 GrantNotAccepted
  6  frozen                            → 39 GrantFrozen
  7  now ≥ expiry                      → 31 GrantExpired
  8  drawn + amount > cap              → 33 CapExceeded
  9  amount > per_tx_limit             → 34 PerTxLimitExceeded
 10  draw_id already seen              → 36 DuplicateDrawId      (draw only — a quote takes no id)
 11  price stale / invalid             → 40 / 41
 12  ltv_after > grant.max_ltv         → 35 LtvCeilingExceeded
 13  ltv_after > config.max_ltv        → 11 HealthTooLow          defensive; should be unreachable
 14  reserve.committed < amount        → 20 ReserveUnderfunded    defensive; should be unreachable
```

Steps 3–9 are storage reads already in hand. Everything expensive — accrual, the cross-contract price
call, the LTV comparison — comes after, so a draw rejected on one of those never pays for an oracle
round trip.

Steps 13 and 14 are backstops. `grant.max_ltv ≤ config.max_ltv` always holds because `46` enforces it
at grant time, and `committed` was credited exactly `cap` at acceptance while the cap gate already
bounds `drawn + amount`. If either fires there is an accounting bug.

### `quote_draw` — which ceiling is biting

Returns **both** ceilings rather than one number:

```rust
DrawQuote {
    allowed: bool,
    blocked_reason: Option<u32>,   // the exact code a real call would hit
    cap_headroom: i128,            // cap − drawn, in debt-token units
    ltv_headroom: i128,            // what the current price allows
    binding: Binding,              // Cap | Ltv | None
    ltv_after_bps: u32,
    price_age: u64,
}
```

The two move independently — `cap_headroom` changes only when this customer draws or repays,
`ltv_headroom` changes with every price tick — and the remedies are **opposite**: repay versus
deposit more. An interface that reports one number cannot tell the customer which to do.

`36 DuplicateDrawId` can never appear here, because `quote_draw` takes no `draw_id`. A caller that has
already submitted an id must track that itself.

---

## 13. Error codes

**A code number never changes after the first public testnet deployment.** New codes take unused
numbers; retired codes leave their number **permanently vacant**. Reassigning a code's *meaning* is
more dangerous than reassigning its number, because an old client keeps the old interpretation.

| # | Name | Meaning |
|---|---|---|
| 1 | `NotInitialized` | a call reached an address that is not an initialised vault |
| 2 | `Unauthorized` | wrong role on a manual role check (`freeze_grant`) |
| 3 | `InvalidConfig` | the constructor rejected the configuration |
| 4 | `MathOverflow` | exceeded `i128`, or a negative operand, or a divisor ≤ 0 |
| 5 | `InvariantViolated` | **must never appear.** Halt and investigate |
| 10 | `InsufficientDeposit` | withdrawing more than the position holds |
| 11 | `HealthTooLow` | exceeds the **protocol** LTV ceiling |
| 12 | `InvalidAmount` | `amount ≤ 0`, repayment above principal, lender liquidation above debt |
| 13 | `Paused` | a risk-increasing function while paused |
| 14 | `NoDebt` | liquidating or repaying a position with no debt |
| 20 | `ReserveUnderfunded` | `accept_grant` beyond `reserve.free` — rule R2 |
| 21 | `ReserveLocked` | the lender tried to withdraw below `committed + outstanding` |
| 22 | `ExposureCapExceeded` | beyond `vault_cap` — rule R5 |
| 30 | `GrantNotFound` | no grant, or the caller does not match `grant.operator` |
| 31 | `GrantExpired` | `now ≥ grant.expiry` |
| 33 | `CapExceeded` | `drawn + amount > cap` |
| 34 | `PerTxLimitExceeded` | `amount > per_tx_limit` |
| 35 | `LtvCeilingExceeded` | a **draw** would exceed the **grant's own** LTV ceiling |
| 36 | `DuplicateDrawId` | `draw_id` already used. Treat as success, not failure |
| 37 | `InvalidGrantParams` | bad cap / per-tx / expiry, or `settlement == operator` |
| 38 | `GrantExists` | granting while a live grant exists — closes the `drawn`-reset hole |
| 39 | `GrantFrozen` | |
| 40 | `PriceStale` | `now − timestamp > price_max_age` |
| 41 | `PriceInvalid` | no price, `price ≤ 0`, or a timestamp too far in the future |
| 42 | `GrantNotExpired` | `reap_expired` before expiry |
| 46 | `GrantCeilingTooHigh` | the **customer** proposed `max_ltv_bps` above the market's |
| 47 | `TooManyGrants` | more than `MAX_GRANTS_PER_OWNER` live grants |
| 48 | `GrantNotAccepted` | acting on a grant the operator has not taken up |
| 49 | `GrantAlreadyAccepted` | accepting or rejecting twice. Acceptance is not idempotent — it commits capital |
| 50 | `PositionHealthy` | `ltv < liq_threshold`, or (for `lender_liquidate`) the position is insolvent |
| 51 | `CloseFactorExceeded` | `repay > max_repay` |
| 52 | `SlippageExceeded` | `seize < min_deposit_out` |
| 53 | `LiquidationGrace` | inside the window after oracle recovery |

**Vacant:** 6–9, 15–19, 23–29, 43–45, 54–59.
**Permanently retired, never reuse:** `32` (was `GrantRevoked` — revocation deletes the entry, so a
draw afterwards hits `30`), `43` `44` `45` (the governance paths a market vault cannot have).

Three pairs are easy to confuse and are split on purpose:

- **11 vs 35** — the protocol ceiling versus the grant's own. The backend reacts differently, so the
  more specific code wins.
- **35 vs 46** — a *draw* exceeding terms the customer set, versus the *customer* proposing terms
  above what the market allows. Opposite comparison, different actor.
- **30 vs 48** — `30` means the customer never authorised you (or revoked); `48` means they did and
  you have not taken it up. The remedy for one is to ask the customer, for the other to call
  `accept_grant`.

---

## 14. Events

21 events, topic 0 always `"vault"`.

The architectural rule the whole system rests on is *wipe the database, rebuild from ledger 0, get
the same result.* That reads like an off-chain requirement; it is a hard requirement on `events.rs`.
Every state change must emit, and every event must carry enough to reproduce it — one write path
without an event, or one event missing a field, and the rebuild can never be made to work from the
outside.

So every event carries both the **delta** and the **resulting value**. `Repaid { amount }` alone
would force a consumer to reimplement the allocation rule, and two implementations of the same rule
diverge on the day the reconciliation report goes red.

| Topic | Event | Key fields |
|---|---|---|
| `initialized` | `Initialized` | the **whole** `Config` |
| `paused` | `Paused` | `by`, `at`, `until` |
| `unpaused` | `Unpaused` | `at` |
| `deposited` | `Deposited` | `owner`, `amount`, `deposit_after` |
| `withdrawn` | `Withdrawn` | `owner`, `to`, `amount`, `deposit_after`, `ltv_after_bps` |
| `grant_prop` | `GrantProposed` | full terms. **No `committed_after`** — nothing moved |
| `grant_acc` | `GrantAccepted` | full terms again, `committed_after` |
| `grant_rej` | `GrantRejected` | `cap`, `committed_after` (unchanged, carried anyway) |
| `grant_rvk` | `GrantRevoked` | `cap`, `drawn`, `released`, `committed_after` |
| `grant_reap` | `GrantReaped` | same shape |
| `grant_frz` | `GrantFrozen` | `by` |
| `grant_unfz` | `GrantUnfrozen` | |
| `drawn` | `Drawn` | `draw_id`, `operator`, `settlement`, `amount`, and four `_after` figures |
| `repaid` | `Repaid` | `from`, `operator?`, `interest_paid`, `principal_paid`, `headroom_restored`, … |
| `accrued` | `Accrued` | `amount`, `interest_after`, `index`, `last_accrual`, `accrual_rem` |
| `res_fund` | `ReserveFunded` | `amount`, `total_after` |
| `res_wdraw` | `ReserveWithdrawn` | `amount`, `total_after`, `free_after` |
| `liquidated` | `Liquidated` | `by_lender` flag, `ltv_before_bps`, `seized`, … |
| `bad_debt` | `BadDebtRealised` | `lost_principal`, `recovered`, `bad_debt_total`, `total_after` |
| `orc_out` | `OracleOutage` | `at` |
| `orc_recov` | `OracleRecovered` | `grace_until` |

Rules:

- **Never change the fields of a deployed event.** Same rule as error codes. Need more data? Add a
  new event.
- **Emit at step 8** — before the transfer and before `bump`. Emitting after a transfer would let a
  hostile token interleave its own events and corrupt ordering.
- **Amounts are in the token's smallest unit**, never converted to USD. A USD value can be
  recomputed from a price; a token amount converted away cannot be recovered.
- **Every position-scoped event names its `owner`.** An event saying a thousand dollars of principal
  now exists, without saying whose, is not a projection of anything. It is also how an indexer learns
  who the customers are — nothing enumerates them on chain, deliberately.

`GrantRejected` and `GrantReaped` are separate from `GrantRevoked` despite deleting the same entry,
because an operator saying no, a customer changing their mind, and an expiry being tidied away are
three different facts. One of them is worth showing the customer as a reason; one is worth alerting
on; one is housekeeping.

`Liquidated` carries `ltv_before_bps` as **audit evidence** — it proves the position genuinely
exceeded the threshold at the moment of liquidation. Without it a dispute has no on-chain answer.

**There are no configuration events**, because `Config` is frozen at construction. Their absence is
the interface saying that no address can change the terms of a position here.

---

## 15. Security properties

### What is structurally impossible

| Property | Mechanism |
|---|---|
| Funds reaching an address the customer did not sign for | `draw` has **no destination parameter**; it reads the pinned `grant.settlement` |
| Any address changing a risk parameter or the oracle | `Config` is frozen; there is no setter and no upgrade path |
| One customer touching another's position or grant | every accessor for `Position` and `Grant` takes the owner's address |
| A grant's `drawn` counter being reset | `38 GrantExists` — grants are never overwritten, only revoked and re-created |
| Liquidating cheaply during an oracle gap | strict price on both routes, plus the 30-minute grace window |
| Committing capital the customer alone asked for | `accept_grant` is signed by the **operator** |
| Reading two different prices in one invocation | only `oracle.rs` may construct an `OracleClient`, and it reads once |
| The interest bill depending on settlement cadence | the index is **additive**; settlement divides by a constant |
| Suppressing the lender's revenue by spamming `accrue` | the remainder is carried; `last_accrual` does not move if `index` does not |
| Duplicate debt from a retried request | the `draw_id` replay guard, on chain, for 30 days |
| An unbounded loop whose length a customer chooses | `MAX_GRANTS_PER_OWNER = 8`, enforced at grant time |

### Reentrancy

Every token transfer is step 9 of 10 — after the state write, after the invariant assertion, after
the event. A hostile or reentrant token sees a fully committed, fully consistent state.

`liquidate` makes two transfers, debt in then collateral out, both after the write. `wipe_out` does
the same. `lender_liquidate` makes exactly one, and no debt token moves at all.

### Risk-reducing actions are never blocked

This is a deliberate, tested class. `deposit`, `repay`, `accrue`, `revoke_authority`,
`freeze_grant`, `reap_expired`, `reject_grant` and `withdraw_reserve` are blocked by neither a pause
nor a dead oracle. `withdraw` is blocked by a pause but reads no price once the debt is clear —
which is what lets a market whose feed has died permanently still wind itself down.

`liquidate` and `lender_liquidate` are also **not** blocked by pause: a pause is normally triggered
by an operational incident, and letting it block liquidation would convert that into capital risk for
the lender across every customer at once.

### The pause, and what it does not bound

`pause` is lender-only and lapses by itself after seven days. In a single-customer vault the brake
was safe because it was asymmetric — either party could pull it, both had to agree to release it — so
neither could hold the other's collateral hostage. A market vault has no such pair; thousands of
customers cannot co-sign.

Seven days turns an unbounded freeze into an incident window, but it bounds **inattention, not the
lender**: calling `pause` again restarts the clock, so a lender who means to keep the market shut can
renew weekly. That is deliberate for the case it was designed for — an incident still live deserves
an explicit renewal, on the record, rather than silent continuation — and it does mean the expiry is
not a ceiling on the lender's power. **Accepted and recorded**; a cooldown between pauses is the fix
if it is ever wanted.

Blocked by a pause: `withdraw`, `grant_authority`, `draw`, `unfreeze_grant`. Nothing else.

### Known limitations and accepted risks

1. **A permanently dead oracle ends new borrowing in this market, forever.** `oracle` is frozen. The
   wind-down path (`repay`, then `withdraw`) always works. Accepted as the price of having no
   privileged address.
2. **The grace window only opens if somebody called `report_oracle_state` during the outage.**
   `grace_until` is set on recovery only when `outage_at` was recorded. A liquidator has no incentive
   to record an outage, so the customer's side must. Operationally this means something friendly has
   to poll it.
3. **A pause can be renewed indefinitely by an attentive lender** — see above.
4. **Closing an insolvent position is exactly break-even for the liquidator.** They pay
   `min(market_value, debt)` and receive collateral worth the same; there is no bonus, because the
   debt exceeds the collateral and there is nothing to pay one out of. So no third party has a reason
   to call it, and in practice the lender clears these themselves through `liquidate` (their own
   route rejects insolvent positions by design). The mechanism is sound; the incentive is not there.
5. **Interest is simple, not compounding**, and unpaid interest never becomes principal. Matches the
   specification; a market wanting compounding needs a different debt representation.
6. **`accrue(owner)` is permissionless and writes a position entry**, so anyone can create a zeroed
   `Position` for an arbitrary address at their own expense. Harmless, but it is storage anyone can
   create.
7. **A grant's `max_ltv_bps` is not required to be non-zero.** A grant created with `0` is well
   formed and every draw against it fails with `35`.
8. **`price_max_age` has a floor of 60 s but no ceiling.** A market deployed with a very long
   staleness tolerance is a valid market, frozen that way.
9. **`DrawSeen` is Temporary**, so a `draw_id` becomes replayable 30 days after first use.

---

## 16. Tests

227 tests across 19 modules. Three rules govern all of them:

1. **Assert exact error codes** — `assert_eq!(err, Error::CapExceeded)`, never `assert!(is_err())`.
   The specification requires the number, because that is what a client branches on.
2. **Assert exact numbers, computed out of band.** "Approximately right" cannot catch a rounding
   direction that is backwards.
3. **Assert the negative.** After a `draw`, assert the operator's balance is *zero*, not merely that
   settlement received something.

Every scenario ends with `check_books`, which asserts I1–I4 **and both token balances**. The
invariant checks inside the contract cannot see balances; that is where a missing transfer hides,
with the accounting staying perfectly self-consistent while the tokens are somewhere else.

| Module | Covers |
|---|---|
| `harness` | the fixture — **three** customers, so nothing passes with the address ignored |
| `acceptance_tests` | the end-to-end scenario |
| `demo_replay` | `scripts/demo.sh`'s own amounts, replayed in memory — which code each step produces |
| `deck_conformance` | one test per line of the original specification |
| `market_tests` | isolation between customers, random multi-customer traffic |
| `multi_grant_tests` | several providers per customer |
| `draw_tests`, `grant_tests`, `headroom_tests` | the authority path |
| `liquidation_tests` | both routes, close factor, and the insolvency branch |
| `interest_tests` | path independence, the carried remainder |
| `oracle_tests` | staleness, the outage guard, the grace window, **price drift within one call** |
| `decimals_tests` | 7/8-decimal collateral, 6/7-decimal debt, varying feed precision |
| `math_tests` | rounding direction on every helper, both branches of `scale` |
| `property_tests` | invariants under random traffic; risk-reducing actions under hostile conditions |
| `rebuild_test` | **the state rebuilds from the event stream alone**, field by field |
| `config_tests`, `governance_tests`, `position_tests`, `reserve_tests` | the rest |

`rebuild_test` is the one that constrains `events.rs`: it replays the emitted events into a fresh
copy of the state and compares every field, including `accrual_rem` and `last_accrual`. Any write
path that forgets to emit, or emits without enough detail, fails there rather than in production.

The fixture uses an 8-decimal collateral token against a 7-decimal debt token and a 14-decimal feed
— `shift = 15` — because a market where the decimals happen to cancel exercises none of the
rescaling.

---

## 17. Build, test, deploy

A step-by-step walkthrough for a reviewer running this from scratch — prerequisites, what each of
the 26 demo steps proves, how to check the evidence, and what the demo does *not* prove — is in
[DEMO.md](DEMO.md). This section is the summary.

```bash
make            # fmt, clippy -D warnings, test, build
make build      # --workspace, wasm32v1-none, release
make test       # cargo test --workspace
make lint       # clippy, warnings are errors
```

`make build` uses `--workspace` deliberately: `mock-oracle` is only a **dev**-dependency of the
vault, so a plain `cargo build` never produces its wasm — and `scripts/demo.sh` needs it.

Toolchain is pinned in `rust-toolchain.toml` (1.97.1, target `wasm32v1-none`). The release profile is
`opt-level = "z"`, `lto = true`, `codegen-units = 1`, `panic = "abort"`, symbols stripped, and
**`overflow-checks = true`** — kept on in release on purpose, as the backstop behind `math.rs`.

### Deploying

```bash
make deploy                          # KEY defaults to vc_deployer, created and funded on testnet
KEY=<funded-identity> make deploy    # or pay with an identity you already hold
```

Deploys mock tokens and a mock oracle if none are given, then the market — **construction inside the
deploy transaction**, so there is no window in which somebody else can initialise it. Overridable:
`ORACLE`, `COLLATERAL`, `DEBT`, `LENDER_ADDR`, `DEPOSIT_FEED`, and every risk parameter.

Afterwards it re-reads `get_config` and asserts the stored `deposit_decimals` and `debt_decimals` are
the token's values (8 and 7), not the `99` it passed — proving the constructor did not trust its
caller. It writes `.deploy/env`, including `OPENED_LEDGER`, read **before** the deploy so an indexer
replaying from it cannot miss the constructor's own `Initialized` event.

```bash
make verify-oracle ORACLE=C… KEY=…   # decimals, resolution, lastprice — verified on chain, never from docs
make demo                            # the acceptance scenario end to end, ~35 minutes
```

`make demo` takes about 35 minutes because liquidation is blocked for 30 minutes after the feed
recovers, and on testnet that wait is wall clock. It writes `.deploy/evidence.md`: a transaction hash
for every step that reached the ledger, and a `simulation` label for every rejection, which by
construction never does. Every rejection step greps for its exact error code and fails the script on
a mismatch — without that, a step failing for the *wrong* reason still passes and the evidence
package becomes a set of transactions nobody verified.

### The two mock contracts

**Neither may ever reach a production network.**

`mock-oracle` is a SEP-40 feed with a settable price, a `clear_price` (so `lastprice` returns `None`,
which is distinct from stale and must not be mistaken for a price of zero), and a **drift mode**:
every `lastprice` call moves the stored price *after* answering. Drift is the only way to prove the
vault's read-the-price-once rule — without it the rule is untestable, since two reads of a constant
feed agree.

`mock-token` is a SEP-41 token with settable decimals and **no authorisation on `mint`**. The Stellar
Asset Contract is the right answer for a real deployment and the wrong one here for one reason: it is
fixed at 7 decimals, and a market whose collateral is also 7 exercises none of the rescaling.
