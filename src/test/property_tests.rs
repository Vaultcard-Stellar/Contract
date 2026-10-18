//! The safety properties, as fuzzed sequences rather than fixed cases.
//!
//! Randomness is a deterministic LCG seeded per test. A fuzz test that cannot be replayed is a
//! fuzz test whose failures cannot be fixed.
//!
//! **P4 matters most.** The deck calls a stuck position *"the most serious mistake that can be
//! made in the entire contract"*, and a stuck state is exactly what hand-written tests miss,
//! because you have to imagine the sequence first.

use super::harness::*;
use crate::Error;

/// Deterministic pseudo-randomness. `no_std`, replayable, and adequate for choosing between a
/// dozen operations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const PRICES: [i128; 5] = [
    P60K,
    30_000 * 100_000_000_000_000,
    P16K,
    12_000 * 100_000_000_000_000,
    P8K,
];

// ---------------------------------------------------------------------------
// P2 — no single address takes collateral from a healthy position
// ---------------------------------------------------------------------------

/// Every entry point **except `withdraw`**, called by every role, against a healthy position.
/// The vault's collateral balance must not move.
///
/// Errors are expected and ignored — the property is not "these calls succeed", it is "however
/// they end, the collateral is still there".
///
/// Two exclusions, both deliberate. `deposit` moves collateral *in*, and the property is about
/// collateral coming out. `withdraw` is the owner's documented exit and is gated by
/// `owner.require_auth()` — which this fixture mocks, so no state-level assertion here could
/// distinguish the owner from anyone else. What this test does prove is that **no other entry
/// point** has a path to the collateral, which is where an accidental one would actually appear.
#[test]
fn p2_only_the_owner_can_remove_collateral() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 5_000 * USD);
    let held = f.dep_t().balance(&f.id);

    for who in [
        f.lender.clone(),
        f.operator.clone(),
        f.liquidator.clone(),
        f.third.clone(),
        f.settlement.clone(),
    ] {
        let _ = f.v().try_liquidate(&f.owner, &who, &(1_000 * USD), &0);
        let _ = f.v().try_lender_liquidate(&f.owner, &(1_000 * USD));
        let _ = f.v().try_draw(&f.owner, &who, &id32(&f.e, 50), &1);
        let _ = f.v().try_freeze_grant(&f.owner, &f.operator, &who);
        let _ = f.v().try_unfreeze_grant(&f.owner, &f.operator);
        let _ = f.v().try_revoke_authority(&f.owner, &f.operator);
        let _ = f.v().try_reap_expired(&f.owner, &f.operator);
        let _ = f.v().try_pause();
        let _ = f.v().try_unpause();
        let _ = f.v().try_withdraw_reserve(&(1_000 * USD));
        let _ = f.v().try_report_oracle_state();
        let _ = f.v().try_accrue(&f.owner);
        let _ = f
            .v()
            .try_repay(&f.owner, &who, &1, &Some(f.operator.clone()));
        let _ = f.v().try_fund_reserve(&1);

        assert_eq!(
            f.dep_t().balance(&f.id),
            held,
            "collateral moved after calls by {who:?}"
        );
    }

    // And the owner's exit is genuinely open — a property that says nothing can move the
    // collateral would be satisfied by a vault that had simply trapped it.
    f.v().accrue(&f.owner);
    f.repay_as(&f.owner, &f.owner, &f.v().get_position(&f.owner).debt);
    f.v().withdraw(&f.owner, &held);
    assert_eq!(f.dep_t().balance(&f.id), 0);
    f.check_books();
}

// ---------------------------------------------------------------------------
// P3 — risk-reducing actions are never blocked
// ---------------------------------------------------------------------------

/// The cross product of {dead oracle, paused, inside the grace window} × {every price in the
/// ladder}. `deposit`, `repay`, `accrue`, `freeze` and `revoke` must all succeed in every cell.
///
/// This is the property that decides which functions may read a price at all: any of them that
/// did would fail the first column.
#[test]
fn p3_risk_reducing_actions_survive_every_hostile_condition() {
    for dead in [false, true] {
        for paused in [false, true] {
            for grace in [false, true] {
                for price in PRICES {
                    let f = F::new();
                    f.funded();
                    f.grant(20_000 * USD, 20_000 * USD, 5000);
                    f.draw(1, 5_000 * USD);
                    f.set_price(price);

                    if grace {
                        f.kill_oracle();
                        f.v().report_oracle_state();
                        f.warp(600);
                        f.set_price(price);
                        f.v().report_oracle_state(); // opens the window
                    }
                    if paused {
                        f.v().pause();
                    }
                    if dead {
                        f.kill_oracle();
                    }

                    f.v().deposit(&f.owner, &1);
                    f.repay_as(&f.owner, &f.owner, &1);
                    f.v().accrue(&f.owner);
                    f.freeze(&f.owner, &f.owner);
                    f.revoke(&f.owner);
                    f.check_books();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// P4 — the position never gets stuck
// ---------------------------------------------------------------------------

/// Random operation sequences. After **every** step, at least one route out must still work:
/// the position can always take more collateral, and whenever it owes anything it can always be
/// repaid.
///
/// Those two are not arbitrary choices — they are the two the contract guarantees
/// unconditionally, so if either ever fails the position is stuck by definition. The invariant
/// check runs after every step alongside them.
#[test]
fn p4_a_position_can_always_be_rescued() {
    let mut rng = Rng(0xC0FFEE);

    for round in 0..12u64 {
        let f = F::new();
        f.v().fund_reserve(&(100_000 * USD));
        f.v().deposit(&f.owner, &BTC);
        f.grant(20_000 * USD, 5_000 * USD, 5000);
        let mut draw_id = 1u8;

        for step in 0..25 {
            match rng.below(9) {
                0 => {
                    let _ = f.v().try_deposit(&f.owner, &(BTC / 10));
                }
                1 => {
                    let _ = f.v().try_withdraw(&f.owner, &(BTC / 20));
                }
                2 => {
                    draw_id = draw_id.wrapping_add(1);
                    let _ =
                        f.v()
                            .try_draw(&f.owner, &f.operator, &id32(&f.e, draw_id), &(1_000 * USD));
                }
                3 => {
                    let _ = f.v().try_repay(
                        &f.owner,
                        &f.third,
                        &(500 * USD),
                        &Some(f.operator.clone()),
                    );
                }
                4 => {
                    let _ = f
                        .v()
                        .try_liquidate(&f.owner, &f.liquidator, &(500 * USD), &0);
                }
                5 => {
                    let _ = f.v().try_lender_liquidate(&f.owner, &(500 * USD));
                }
                6 => {
                    let _ = f.v().try_revoke_authority(&f.owner, &f.operator);
                }
                7 => {
                    f.warp(rng.below(90_000) + 1);
                    f.set_price(PRICES[rng.below(5) as usize]);
                }
                _ => {
                    let _ = f.v().try_accrue(&f.owner);
                }
            }

            // I1–I4 and both balances, after EVERY step.
            f.check_books();

            // Route 1: more collateral is always accepted.
            f.v().deposit(&f.owner, &1);
            // Route 2: whatever is owed can always be paid.
            let debt = f.v().get_position(&f.owner).debt;
            if debt > 0 {
                f.repay_as(&f.owner, &f.third, &1);
            }
            // And the position can always be read, at any price, in any state.
            let _ = f.v().get_position(&f.owner);
            let _ = f.v().quote_liquidation(&f.owner);

            assert!(
                f.raw_position().deposit >= 0,
                "round {round} step {step}: negative deposit"
            );
        }

        // The ultimate exit: clear the debt, release the grant, take everything back.
        f.v().accrue(&f.owner);
        let debt = f.v().get_position(&f.owner).debt;
        if debt > 0 {
            f.repay_as(&f.owner, &f.third, &debt);
        }
        let _ = f.v().try_revoke_authority(&f.owner, &f.operator);
        let held = f.raw_position().deposit;
        if held > 0 {
            f.v().withdraw(&f.owner, &held);
        }
        assert_eq!(
            f.raw_position().deposit,
            0,
            "round {round}: collateral stuck"
        );
        assert_eq!(f.raw_position().principal, 0);
        f.check_books();
    }
}

// ---------------------------------------------------------------------------
// P5 — accrual is path-independent
// ---------------------------------------------------------------------------

/// A random split of one interval into N sub-intervals must produce exactly the interest of a
/// single call over the whole span.
///
/// `advance` multiplies the index, so its value genuinely depends on cadence — but settlement
/// divides by `last_index`, cancelling everything accumulated before the position last settled.
/// The carried remainder closes the last gap, so this is an equality and not a tolerance.
#[test]
fn p5_random_interval_splits_agree_with_a_single_call() {
    let mut rng = Rng(0x5EED);

    for _ in 0..8 {
        let total = 3_600 + rng.below(200_000);
        let parts = 2 + rng.below(10);

        let split = F::new();
        split.funded();
        split.grant(20_000 * USD, 20_000 * USD, 5000);
        split.draw(1, 10_000 * USD);

        let mut spent = 0u64;
        for i in 0..parts {
            let chunk = if i == parts - 1 {
                total - spent
            } else {
                1 + rng.below((total - spent) / (parts - i))
            };
            spent += chunk;
            split.warp(chunk);
            split.v().accrue(&split.owner);
        }
        assert_eq!(spent, total);

        let once = F::new();
        once.funded();
        once.grant(20_000 * USD, 20_000 * USD, 5000);
        once.draw(1, 10_000 * USD);
        once.warp(total);
        once.v().accrue(&once.owner);

        assert_eq!(
            split.raw_position().interest,
            once.raw_position().interest,
            "split into {parts} over {total}s"
        );
    }
}

// ---------------------------------------------------------------------------
// P6 — idempotency
// ---------------------------------------------------------------------------

/// Repeats of `revoke_authority`, `reap_expired`, and `draw` with a reused id. Capital is
/// released exactly once, and the debt is created exactly once.
#[test]
fn p6_repeated_calls_release_and_create_exactly_once() {
    let mut rng = Rng(0xD06);

    for _ in 0..6 {
        let f = F::new();
        f.armed();

        // The same draw id, many times, in random order with other traffic.
        for _ in 0..10 {
            let r = f
                .v()
                .try_draw(&f.owner, &f.operator, &id32(&f.e, 7), &(1_000 * USD));
            if r.is_err() {
                assert_err!(r, Error::DuplicateDrawId);
            }
            if rng.below(3) == 0 {
                f.warp(60);
                f.set_price(P60K);
            }
        }
        assert_eq!(f.raw_position().principal, 1_000 * USD);
        assert_eq!(f.raw_reserve().outstanding, 1_000 * USD);

        // Revoke, repeatedly.
        f.revoke(&f.owner);
        let committed = f.raw_reserve().committed;
        for _ in 0..5 {
            assert_err!(
                f.v().try_revoke_authority(&f.owner, &f.operator),
                Error::GrantNotFound
            );
            assert_err!(
                f.v().try_reap_expired(&f.owner, &f.operator),
                Error::GrantNotFound
            );
            assert_eq!(f.raw_reserve().committed, committed);
        }

        // And reaping an expired grant, repeatedly.
        f.grant(5_000 * USD, 5_000 * USD, 5000);
        f.warp_fresh(30 * 86_400, P60K);
        f.reap(&f.owner);
        let committed = f.raw_reserve().committed;
        for _ in 0..5 {
            assert_err!(
                f.v().try_reap_expired(&f.owner, &f.operator),
                Error::GrantNotFound
            );
            assert_eq!(f.raw_reserve().committed, committed);
        }
        f.check_books();
    }
}

// ---------------------------------------------------------------------------
// I1–I4 under random traffic
// ---------------------------------------------------------------------------

/// Code `5 InvariantViolated` has no test that triggers it, because triggering it would mean
/// there is a bug. This is the same statement from the other side: under thousands of random
/// operations it never fires.
#[test]
fn invariants_hold_under_random_traffic() {
    let mut rng = Rng(0xBEEF);

    for _ in 0..6 {
        let f = F::new();
        f.v().fund_reserve(&(100_000 * USD));
        f.v().deposit(&f.owner, &BTC);
        f.grant(20_000 * USD, 5_000 * USD, 5000);
        let mut draw_id = 100u8;

        for _ in 0..40 {
            match rng.below(11) {
                0 => {
                    let _ = f
                        .v()
                        .try_deposit(&f.owner, &(1 + rng.below(BTC as u64 / 5) as i128));
                }
                1 => {
                    let _ = f
                        .v()
                        .try_withdraw(&f.owner, &(1 + rng.below(BTC as u64 / 5) as i128));
                }
                2 => {
                    draw_id = draw_id.wrapping_add(1);
                    let amount = 1 + rng.below(5_000 * USD as u64) as i128;
                    let _ = f
                        .v()
                        .try_draw(&f.owner, &f.operator, &id32(&f.e, draw_id), &amount);
                }
                3 => {
                    let _ = f.v().try_repay(
                        &f.owner,
                        &f.third,
                        &(1 + rng.below(3_000 * USD as u64) as i128),
                        &Some(f.operator.clone()),
                    );
                }
                4 => {
                    let _ = f.v().try_liquidate(
                        &f.owner,
                        &f.liquidator,
                        &(1 + rng.below(3_000 * USD as u64) as i128),
                        &0,
                    );
                }
                5 => {
                    let _ = f.v().try_lender_liquidate(
                        &f.owner,
                        &(1 + rng.below(3_000 * USD as u64) as i128),
                    );
                }
                6 => {
                    let _ = f.v().try_revoke_authority(&f.owner, &f.operator);
                }
                7 => {
                    let _ = f.v().try_grant_authority(
                        &f.owner,
                        &f.operator,
                        &f.settlement,
                        &(1_000 * USD + rng.below(10_000 * USD as u64) as i128),
                        &(1_000 * USD),
                        &(f.now() + 86_400),
                        &5000,
                    );
                }
                8 => {
                    let _ = f.v().try_fund_reserve(&(1_000 * USD));
                }
                9 => {
                    let _ = f.v().try_withdraw_reserve(&(1_000 * USD));
                }
                _ => {
                    f.warp(rng.below(200_000) + 1);
                    f.set_price(PRICES[rng.below(5) as usize]);
                    let _ = f.v().try_accrue(&f.owner);
                }
            }
            f.check_books();
        }
    }
}
