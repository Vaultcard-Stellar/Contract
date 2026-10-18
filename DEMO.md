# Running the demo

A step-by-step guide for a reviewer with a clean machine and no prior context. Two things are
being demonstrated, and they are separable:

- **The suite** — 227 tests over the contract's own source, offline, in about 11 seconds.
- **The acceptance scenario** — a market deployed to Stellar testnet and driven through 26 steps,
  ten of which are *rejections asserted by exact error code*. Produces `.deploy/evidence.md`, a
  transaction hash per step that reached the ledger.

If you have fifteen minutes, run §2. If you are signing off on the deliverable, run §4 as well —
it takes about 35 minutes, most of it a real wall-clock wait that the contract enforces and no
flag can shorten honestly.

---

## 1. Prerequisites

Four tools. Everything else the scripts install or generate themselves.

| Tool | Version used | Install |
|---|---|---|
| Rust | 1.97.1 | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| `wasm32v1-none` target | — | `rustup target add wasm32v1-none` |
| Stellar CLI | 27.1.0 | `cargo install --locked stellar-cli` or `brew install stellar-cli` |
| `jq` | 1.7+ | `brew install jq` / `apt install jq` |

The toolchain version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml), so `rustup` will
fetch 1.97.1 on first build whatever your default is. You do not need to switch it by hand.

Verify all four at once:

```bash
rustc --version && cargo --version && stellar --version && jq --version
rustup target list --installed | grep wasm32v1-none
```

`scripts/lib.sh`'s `preflight` checks for `stellar` and `jq` and stops with an installation hint
rather than failing three calls later, so a missing tool is never a confusing error.

---

## 2. Offline: build, lint, test

No network, no keys, no testnet account. This is the fastest way to establish that the contract
does what its documentation says.

```bash
make            # fmt → clippy -D warnings → test → build
```

Or individually:

```bash
make build      # --workspace, wasm32v1-none, release; prints the wasm size
make test       # cargo test --workspace — 227 tests across 19 modules, ~11s
make lint       # clippy, warnings are errors
```

**`make build` uses `--workspace` on purpose.** `mock-oracle` is only a *dev*-dependency of the
vault, so a plain `cargo build` never produces its wasm — and `scripts/demo.sh` needs a
controllable price feed to make a price stale and then crash it. A plain build fails at the
oracle step of the deploy with a missing-file error that looks nothing like the real problem.

### Coverage (optional)

```bash
cargo install cargo-llvm-cov --locked   # one-time
make coverage
```

Writes `.deploy/coverage.txt` and `.deploy/lcov.info`. The measurement **excludes** `src/test/`,
`mock-oracle/` and `mock-token/` — counting test modules inflates the figure with code that is by
definition executed, and the mocks are testnet scaffolding. The last recorded run reads 92.03%
line / 97.27% region over the contract's own source.

### What to read alongside

The test module names in [`src/test/`](src/test/) are the map. `demo_replay` is worth singling
out: it replays `scripts/demo.sh`'s own amounts in memory and asserts which code each step
produces — so the scenario in §4 is pinned by a unit test, not only by the live run.

---

## 3. Testnet identities — nothing to do

**You can skip this section.** On testnet and futurenet the scripts create and fund every identity
they need, including the one that pays for the deploy. Running `make demo` on a machine with no
Stellar keys at all works.

The paying identity defaults to **`vc_deployer`**, created and funded on first use. The scenario
then creates six more for the cast — `vc_lender`, `vc_owner`, `vc_operator`, `vc_operator2`,
`vc_settlement`, `vc_liquidator`. All seven carry the `vc_` prefix so you can see at a glance
which keys this repository made.

Identities that already exist are reused and topped up rather than recreated: friendbot is not
always instant, and an account drained by a previous run produces an error that looks nothing like
the real problem.

To pay with an identity you already hold instead:

```bash
export KEY=my-identity          # or: make demo KEY=my-identity
```

On a network without a friendbot (mainnet), a missing identity is a hard error rather than a key
that exists and can pay for nothing — the script tells you exactly what to run.

**Nothing secret is ever written to the repository.** Identities live in the Stellar CLI's own
keystore; `.deploy/env` holds addresses only.

---

## 4. Deploy a market

```bash
make deploy
```

Runs [`scripts/deploy.sh`](scripts/deploy.sh), which:

1. Builds the workspace and checks all three wasm files exist, each by name.
2. Deploys a **mock collateral token at 8 decimals** and a **mock debt token at 7** — deliberately
   unlike each other. A market where both are 7 exercises none of the decimal rescaling, and the
   rescale is exactly where a power-of-ten error hides.
3. Deploys a **mock SEP-40 oracle at 14 decimals**. Real markets point at Reflector; the scenario
   needs a feed it can make stale, restore and crash on demand, which Reflector will not do on request.
4. Reads the current ledger from RPC **before** deploying, so an indexer replaying from
   `OPENED_LEDGER` cannot miss the constructor's own `Initialized` event.
5. Deploys the vault with `__constructor` arguments **in the same transaction**. There is no
   separate `initialize`, and there must not be: a two-step deploy leaves a window in which
   somebody else can initialise the contract and name themselves the lender.

### The check worth watching for

The deploy passes **`99`** for both `deposit_decimals` and `debt_decimals`, then re-reads
`get_config` and asserts the stored values are `8` and `7`. That is the constructor proving it
called `decimals()` on the token contracts rather than believing its caller. A deploy that passed
the true values would demonstrate nothing. You will see:

```
  ✓ decimals came from the tokens (8 / 7), not from the caller
  ✓ risk parameters frozen as configured
```

### Output

`.deploy/env` — sourced by `demo.sh`:

```
NETWORK, VAULT, COLLATERAL, DEBT, ORACLE, DEPLOYER, LENDER_ADDR,
DEPOSIT_FEED, PRICE_MAX_AGE, OPENED_LEDGER
```

### Deploying against real contracts

Every dependency is overridable, so the same script points at production feeds and tokens:

```bash
KEY=my-identity ORACLE=C… COLLATERAL=C… DEBT=C… LENDER_ADDR=G… make deploy
```

---

## 5. The acceptance scenario

```bash
make demo
```

Runs [`scripts/demo.sh`](scripts/demo.sh) end to end: it creates and funds six identities
(`vc_lender`, `vc_owner`, `vc_operator`, `vc_operator2`, `vc_settlement`, `vc_liquidator`),
calls `deploy.sh` for a **fresh** market, mints balances, and drives 26 steps. It needs no
arguments and no pre-existing keys.

Six roles rather than one identity reused: sharing one would let a gate pass because the caller
happened to be authorised for something else, which is precisely the confusion the roles exist to
prevent.

### Expect ~35 minutes, and expect it to sit still

Step 22 waits out `LIQUIDATION_GRACE` — **30 minutes** during which the contract refuses
liquidation after a price feed recovers, so an owner can react to a gap they could not have seen
coming. It is a **code constant, not configuration**; nobody can set it to zero after deployment,
and on testnet the ledger clock is wall clock. You will see a live countdown:

```
Waiting out the grace window (30 minutes on chain)
  14:45 remaining
```

`SKIP_GRACE_WAIT=1` exists for iterating on the script and the script says so in its own output:
**a run using it does not demonstrate the grace window, and its evidence file is not acceptance
evidence.** Use it to smoke-test your setup, not to sign anything off.

### The property that makes it evidence rather than theatre

**Every rejection step greps for its exact error code and fails the script on a mismatch.** Look
at `call` in [`scripts/lib.sh:74`](scripts/lib.sh#L74): a step expecting `Error(Contract, #33)`
that gets any other error — or that *succeeds* — kills the run with a message saying so.

Without that, a step failing for the *wrong* reason still goes green and the evidence file fills
up with transactions nobody verified. That is worse than no evidence, because it looks like
evidence.

The script also refuses to call the run a success if it ever produces fewer than **10 distinct
error codes** — a demo that quietly stops exercising a gate is how a regression ships.

### It re-derives its own preconditions from the chain

The price points are computed from the haircut, the liquidation threshold and the deposit — all
three settable by environment variable. When one moves, a hand-computed price silently stops
triggering what it was chosen to trigger. So before it acts the script asks the *contract*:

- `assert_liquidatable` reads `quote_liquidation` and checks **both** `liquidatable` **and**
  `blocked_reason` — a position can be liquidatable and still refused with `53`, because `liquidate`
  checks the grace window before it checks health.
- `assert_insolvent` reads `get_position` and checks market value is below the debt. Insolvency
  triggers on **market** value, not eligible: the haircut is a buffer for lending decisions, not
  for recovery.
- After the withdrawal it asserts the position holds exactly 0.5 BTC, because every price point
  below is derived from that figure.

---

## 6. The 26 steps

Bold codes are **rejections**: the contract refused, and the script verified the number.

| # | Step | Code |
|---|---|---|
| 1 | Deploy tokens, oracle and market — constructor inline, one transaction | — |
| 2 | Lender funds the reserve with 500,000 | — |
| 3 | Customer deposits 0.6 BTC of collateral | — |
| 4 | Customer withdraws 0.1 BTC — no debt, so no price is read | — |
| 5 | Grant with an LTV ceiling above the market's | **46** |
| 6 | Grant spending authority: 20,000 cap, 13,000 per transaction | — |
| 7 | Grant a second authority to the same operator | **38** |
| 8 | Operator accepts the line — 20,000 moves free → committed | — |
| 9 | Customer proposes a 600,000 line to a second provider | — |
| 10 | Second provider accepts — the free reserve cannot back it | **20** |
| 11 | Operator draws 5,000 to the pinned settlement address | — |
| 12 | Resubmit the same draw_id | **36** |
| 13 | Draw above the per-transaction limit | **34** |
| 14 | Draw beyond the cumulative cap | **33** |
| 15 | Draw past the grant's loan-to-value ceiling | **35** |
| 16 | Draw a further 7,000 — debt 12,000 against 26,400 eligible | — |
| 17 | Draw against a stale price | **40** |
| 18 | Record the oracle outage (permissionless) | — |
| 19 | Record the recovery, opening the grace window | — |
| 20 | Liquidate inside the grace window | **53** |
| 21 | Customer repays 1,000 — interest first, then principal | — |
| 22 | Third party liquidates and earns the bonus | — |
| 23 | Lender liquidates by netting — no debt token moves | — |
| 24 | Customer revokes the spending authority | — |
| 25 | Draw after revocation (code 30, not the retired 32) | **30** |
| 26 | Liquidate an insolvent position — the branch that must not revert | — |

Ten distinct codes: **46, 38, 20, 36, 34, 33, 35, 40, 53, 30**. Their meanings are in
[README §13](README.md#L1023).

### Six steps a reviewer should understand before reading the hashes

**Step 4 — withdrawal reads no price at all.** The point is not that a number went down. It is
that withdrawal depends on nothing but this customer's own debt: no pool utilisation, no other
borrower, and while the debt is zero, not even the oracle. A pool design cannot make that promise.

**Steps 9–10 — the reserve gate fires on *acceptance*, not proposal.** A proposal commits nothing,
so it cannot be refused for a thin reserve. Acceptance moves `reserve.free` into
`reserve.committed`, and that is the moment the capital behind a line has to actually exist. A
600,000 line against a 500,000 reserve is refused with **20** *at the moment it becomes usable* —
which is what makes "a card transaction is never declined because somebody else borrowed first"
true rather than aspirational. It needs a *second* operator: the same one would be refused with
**38** long before the reserve is ever consulted.

**Step 11 — the settlement pin, asserted in the negative.** After the draw the script checks the
settlement address received exactly 5,000 **and that the operator holds nothing** — neither debt
token nor collateral. Checking only that settlement was paid passes even when the operator was
paid too, and the operator being payable is the entire threat the pinned address removes. `draw`
has no destination parameter at all.

**Steps 13–15 — each rejection is tuned so the *intended* gate is the one that fires.** Step 13
draws 14,000 against a 13,000 per-transaction limit with 5,000 already drawn on a 20,000 cap: the
cap gate runs first and 19,000 passes it, so **34** is genuinely what fires. Ask for more and
**33** would pre-empt it, and the step would pass while testing the wrong gate. Step 15 uses
13,000 — inside the cap, exactly at the per-tx limit — so the only thing left to refuse it is the
price, which is the whole point.

**Step 23 — the lender route nets.** The script records the vault's debt-token balance before and
after and asserts it is **unchanged**. The lender is not paying themselves; they are taking
collateral against a claim they already hold.

**Step 26 — the branch that must not revert.** Both paths through `wipe_out` are covered by the
unit suite. What no unit test can show is the property this step exists for: on a real ledger,
against a position whose market value has fallen below its debt, the call **returns rather than
reverting** and leaves the position closeable. The script then asserts `bad_debt` is non-zero — if
it were zero the insolvency branch never ran and the step proved nothing.

Note the ordering: revocation (24) comes **before** insolvency (26). The reverse cannot run — the
insolvency branch calls `grant::release`, which deletes the grant, so a later `revoke_authority`
returns **30** and the step fails. Found by running it.

---

## 7. What you get, and how to check it

| File | Contents |
|---|---|
| `.deploy/evidence.md` | the step table, with a transaction hash per ledger-reaching step |
| `.deploy/env` | addresses of the vault, tokens, oracle, and `OPENED_LEDGER` |
| `.deploy/position.json` | the owner's final position |
| `.deploy/reserve.json` | final `total` / `committed` / `outstanding` / `bad_debt` |
| `.deploy/config.json` | the frozen config as actually stored on chain |

To verify independently, open each hash on Stellar Expert:

```
https://stellar.expert/explorer/testnet/tx/<hash>
```

### Why some rows say `simulation` instead of a hash

**Rejections do not get transaction hashes, and inventing one would be forgery.** The CLI
simulates before submitting, so a call the contract will refuse fails locally and never reaches
the ledger. Those rows are labelled `simulation`. A fabricated hash is the one artefact in the
package a reviewer would have no way to catch, so the scripts do not produce one.

The hash is parsed from the CLI's own `Signing transaction:` label rather than by scanning for 64
hex characters — a `draw_id` argument is *also* 64 hex characters, and a loose match once recorded
one of those as a transaction hash. An evidence file full of draw ids would look entirely
plausible and open to nothing.

---

## 8. Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `stellar is not installed` / `jq is not installed` | `preflight` stopping early. Install per §1. |
| `'X' is not a known identity, and 'N' has no friendbot` | you pointed at a non-funding network. Create and fund the identity yourself, then re-run. |
| `could not create and fund 'vc_deployer'` | friendbot rate-limited or down. Wait a minute and re-run. |
| `…was not produced. Run 'make build'` | a plain `cargo build` skipped the mocks. `make build` uses `--workspace`. |
| `could not read the current ledger` | testnet RPC unreachable. Override with `RPC_URL=…`. |
| `deploy.sh failed; run it directly to see why` | `demo.sh` hides deploy output. Run `KEY=… ./scripts/deploy.sh` for the real error. |
| `step N expected error X and the call SUCCEEDED` | **a real finding.** A gate that should have refused did not. |
| `step N expected error X but got something else` | **a real finding**, or a risk parameter was overridden and a price point no longer triggers what it was chosen to trigger. |
| `the position is not liquidatable at this price` | the haircut / threshold / deposit changed; the price points in `demo.sh` need recomputing. The message prints the full quote. |
| `expected at least 10 distinct error codes` | a gate stopped being exercised. Treat as a regression. |
| Transaction fails with `txInsufficientBalance` | an identity ran dry mid-run. Re-running tops every identity up. |

Re-running `make demo` is always safe: it deploys a **fresh** market every time and never reuses
the previous one. It does, however, **overwrite `.deploy/evidence.md`** — copy the old one aside
first if it matters.

---

## 9. Environment variables

Read by both scripts. All optional — `make demo` with no environment at all is a valid run.

| Variable | Default | Effect |
|---|---|---|
| `KEY` | `vc_deployer` | the identity that pays. Created and funded automatically on testnet/futurenet |
| `DEFAULT_KEY` | `vc_deployer` | rename the auto-created paying identity |
| `NETWORK` | `testnet` | target network |
| `RPC_URL` | per network | used for `getLatestLedger` and tx lookups |
| `DEPLOY_DIR` | `.deploy` | where artefacts are written |
| `ORACLE` `COLLATERAL` `DEBT` | deploy mocks | point at real contracts instead |
| `LENDER_ADDR` | the deployer | who holds the lender role |
| `DEPOSIT_FEED` | `BTC` | the SEP-40 asset symbol |
| `SKIP_GRACE_WAIT` | unset | **invalidates the evidence.** Iteration only |

Risk parameters, all frozen at construction — changing one means deploying a new market:

| Variable | Default | |
|---|---|---|
| `HAIRCUT_BPS` | `1200` | 12% |
| `MAX_LTV_BPS` | `5000` | 50% |
| `LIQ_THRESHOLD_BPS` | `7000` | 70% |
| `FULL_LIQ_LTV_BPS` | `9000` | 90% |
| `LIQ_BONUS_BPS` | `800` | 8% |
| `CLOSE_FACTOR_BPS` | `5000` | 50% |
| `PRICE_MAX_AGE` | `300` | seconds |
| `RATE_BPS_YEAR` | `1000` | 10% |
| `VAULT_CAP` | `10000000000000` | $1,000,000 at 7 dp |
| `DUST_THRESHOLD` | `100000000` | $10 at 7 dp |

Change any of the first three and the price points in `demo.sh` stop triggering what they were
chosen to trigger — `assert_liquidatable` will say so in one clear sentence rather than letting a
later step fail confusingly.

---

## 10. Checking the oracle independently

Three numbers that must be verified **on chain, never taken from documentation**, before a market
is frozen against a real feed:

```bash
make verify-oracle ORACLE=C… KEY=vc_deployer
```

Prints `decimals`, `resolution` and `lastprice`. Every gate in the contract is scaled by
`decimals`, and `oracle` is frozen at construction — a wrong reading here cannot be corrected
later by anyone.

---

## 11. What this demo does not prove

Stated plainly, because a demo that implies more than it shows is the failure mode this whole
package is built against.

- **The mocks are not production components.** `mock-token` has **no authorisation on `mint`** and
  `mock-oracle` has a settable price. Neither may ever reach a production network. They exist
  because the Stellar Asset Contract is fixed at 7 decimals — a market whose collateral is also 7
  exercises none of the rescaling — and because Reflector will not go stale on request.
- **Interest over long horizons is not shown live.** The run is ~35 minutes; the index behaviour at
  1, 30 and 365 days is covered by the suite
  (`cargo test exact_interest_at_one_thirty_and_three_hundred_sixty_five_days`).
- **The risk parameters are estimates.** Haircut, max LTV and liquidation threshold are the deck's
  bridged-BTC profile and remain estimates until recalibrated against real volatility data. They
  are written out in `deploy.sh` rather than defaulted somewhere precisely so this is visible.
- **A run with `SKIP_GRACE_WAIT` proves nothing about the grace window,** and the script says so in
  its own output.
- **`.deploy/evidence.md` in the repository is from a previous run** (2026-08-19). Your run
  overwrites it. Do not review a hash you did not generate.
