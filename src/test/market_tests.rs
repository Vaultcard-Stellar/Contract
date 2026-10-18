//! The tests that only exist because the vault is a market.
//!
//! Every other file in this suite would pass unchanged against a single-customer contract with an
//! address parameter it ignored. These are the ones that would not: they put two or three
//! customers in the same market and check that the shared things are shared, the private things
//! are private, and the boundary between them holds under load.
//!
//! Three properties carry most of the weight:
//!
//! - **Isolation.** Nothing one customer does may change another customer's position, by so much
//!   as one unit. Asserted by comparing whole `Position` structs before and after.
//! - **`outstanding == Σ principal`.** Invariant I3 is only differential on chain now; this is
//!   where its absolute form is checked, over every customer at once.
//! - **One clock, many settlements.** The borrow index is market-wide, and each position settles
//!   against it from wherever it left off. A customer who joins today must not be billed for
//!   interest the market accrued before they arrived.

use super::harness::*;
use crate::{DataKey, Error};
use soroban_sdk::testutils::storage::Persistent as _;
use soroban_sdk::testutils::Ledger;

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

/// A's whole lifecycle leaves B's position byte-identical.
///
/// The assertion is on the **entire struct**, not on selected fields. A bug that leaked one
/// customer's accrual into another's `accrual_rem` would pass a field-by-field check written by
/// somebody who did not think of that field.
#[test]
fn one_customers_activity_never_touches_another() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 5_000 * USD);
    let b_before = f.raw_position_of(&f.owner_b);

    // A does everything a customer can do.
    f.v().deposit(&f.owner, &BTC);
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f.repay_as(&f.owner, &f.third, &(2_000 * USD));
    f.freeze(&f.owner, &f.owner);
    f.unfreeze(&f.owner);
    // $12 000/BTC against $8 000 of debt: LTV 7576 bps, past the 7000 threshold.
    f.set_price(12_000 * 100_000_000_000_000);
    f.v().liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0);
    f.set_price(P60K);
    f.revoke(&f.owner);
    f.v().withdraw(&f.owner, &(BTC / 4));

    assert_eq!(
        f.raw_position_of(&f.owner_b),
        b_before,
        "B's position moved while A was transacting"
    );
    assert_eq!(f.grant_of(&f.owner_b).unwrap().drawn, 5_000 * USD);
    f.check_books();
}

/// Liquidating A does not make B liquidatable, and B cannot be liquidated by pointing the call at
/// them instead.
#[test]
fn liquidating_one_customer_leaves_the_others_alone() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    // A borrows to the ceiling; B borrows a tenth of it against the same collateral.
    f.arm_for(&f.owner, BTC, 30_000 * USD, 1, 26_400 * USD);
    f.arm_for(&f.owner_b, BTC, 30_000 * USD, 60, 2_000 * USD);

    f.set_price(P16K); // A is far past the threshold, B is nowhere near it
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, true);
    assert_eq!(f.v().quote_liquidation(&f.owner_b).liquidatable, false);
    assert_err!(
        f.v()
            .try_liquidate(&f.owner_b, &f.liquidator, &(100 * USD), &0),
        Error::PositionHealthy
    );

    let b_before = f.raw_position_of(&f.owner_b);
    f.v().liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &0);
    assert_eq!(f.raw_position_of(&f.owner_b), b_before);
    f.check_books();
}

/// A's insolvency writes the market's claim down and leaves everyone else's line intact.
///
/// This is the case where a market vault genuinely differs from a set of separate ones: the loss
/// lands on the shared pool. What must **not** happen is that it lands on another customer's
/// position.
#[test]
fn one_customers_insolvency_is_absorbed_by_the_shared_pool() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 10_000 * USD);
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 5_000 * USD);

    let b_before = f.raw_position_of(&f.owner_b);
    let total_before = f.raw_reserve().total;

    f.set_price(P8K); // A: $8 000 of collateral against $10 000 of debt
    f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    let r = f.raw_reserve();
    assert_eq!(r.bad_debt, 2_000 * USD);
    assert_eq!(r.total, total_before - 2_000 * USD); // the pool absorbs it
    assert_eq!(f.raw_position_of(&f.owner_b), b_before); // B is untouched
    assert_eq!(f.grant_of(&f.owner_b).unwrap().cap, 20_000 * USD);
    // And B can still borrow: the loss reduced the pool, not their line.
    f.set_price(P60K);
    f.draw_for(&f.owner_b, 61, 1_000 * USD);
    f.check_books();
}

/// One operator key serving several customers can only reach each one up to their own cap, and
/// only into the address each one pinned.
///
/// This is the blast radius that multi-tenancy widens, so it is worth stating exactly. A
/// compromised operator can touch every customer who authorised it — but each customer's cap,
/// per-transaction limit and settlement address bound it independently.
#[test]
fn a_shared_operator_is_still_bounded_by_each_customers_own_terms() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.v().deposit(&f.owner, &BTC);
    f.v().deposit(&f.owner_b, &BTC);
    f.grant_for(&f.owner, 5_000 * USD, 5_000 * USD, 5000);
    f.grant_for(&f.owner_b, 20_000 * USD, 20_000 * USD, 5000);

    // The same operator, on both lines.
    f.draw_for(&f.owner, 1, 5_000 * USD);
    f.draw_for(&f.owner_b, 2, 20_000 * USD);

    // A's smaller cap binds A and not B.
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 3), &1),
        Error::CapExceeded
    );

    // And the money went to two different pinned addresses, not one.
    assert_eq!(f.debt_t().balance(&f.settlement), 5_000 * USD);
    assert_eq!(f.debt_t().balance(&f.settlement_b), 20_000 * USD);
    assert_eq!(f.debt_t().balance(&f.operator), 0);
    f.check_books();
}

/// A customer cannot act on somebody else's position by naming it. Authorisation is checked
/// against the address in the call, so the only thing a wrong caller can do is fail.
#[test]
fn a_customer_cannot_reach_another_customers_position() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 5_000 * USD);
    let b_before = f.raw_position_of(&f.owner_b);

    // With authorisation mocked, the state gates are what is under test: `freeze_grant` is the
    // one path with a manual role comparison, and it refuses a stranger.
    assert_err!(
        f.v().try_freeze_grant(&f.owner_b, &f.operator, &f.owner),
        Error::Unauthorized
    );
    assert_eq!(f.raw_position_of(&f.owner_b), b_before);
    assert_eq!(f.grant_of(&f.owner_b).unwrap().frozen, false);

    // The lender may, because they carry the risk.
    f.freeze(&f.owner_b, &f.lender);
    assert_eq!(f.grant_of(&f.owner_b).unwrap().frozen, true);
    f.check_books();
}

// ---------------------------------------------------------------------------
// The shared pool
// ---------------------------------------------------------------------------

/// `reserve.free` is one pool. A's grant is capital B can no longer be promised — which is the
/// whole point of reserving at grant time, and the cost of the deck's promise that a card
/// transaction is never declined because somebody else borrowed first.
#[test]
fn one_customers_grant_consumes_capital_the_others_could_have_had() {
    let f = F::new();
    f.v().fund_reserve(&(30_000 * USD));
    f.v().deposit(&f.owner, &BTC);
    f.v().deposit(&f.owner_b, &BTC);

    assert_eq!(f.v().get_reserve().free, 30_000 * USD);
    f.grant_for(&f.owner, 25_000 * USD, 25_000 * USD, 5000);
    assert_eq!(f.v().get_reserve().free, 5_000 * USD);

    // B's own collateral still supports $26 400 — the price has not moved. The pool is what
    // refuses, and the two ceilings are reported separately precisely so the difference is
    // visible: B needs the lender to fund more, not to deposit more.
    let q = f.v().quote_draw(&f.owner_b, &f.operator, &0);
    assert_eq!(q.ltv_headroom, 26_400 * USD);
    assert_eq!(q.cap_headroom, 0); // no grant, because the pool would not back one
                                   // B may still *propose* — a proposal reserves nothing, and refusing one because the pool is
                                   // thin right now would deny a line the operator might take up after the lender tops up. It
                                   // is the acceptance that finds the pool empty.
    f.propose_for(&f.owner_b, 5_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        f.v().try_accept_grant(&f.owner_b, &f.operator),
        Error::ReserveUnderfunded
    );
    f.v().revoke_authority(&f.owner_b, &f.operator);

    // A gives it back, and B can have it.
    f.revoke(&f.owner);
    assert_eq!(f.v().get_reserve().free, 30_000 * USD);
    f.grant_for(&f.owner_b, 25_000 * USD, 25_000 * USD, 5000);
    f.check_books();
}

/// `vault_cap` bounds the **market**, not each customer. Two customers who each fit under it can
/// still breach it together — which is the shape the deck's `program_cap` was reaching for and
/// that a single-customer vault could not express.
#[test]
fn the_exposure_cap_is_market_wide_not_per_customer() {
    let f = F::with(|mut c| {
        c.vault_cap = 30_000 * USD;
        c
    });
    f.v().fund_reserve(&(200_000 * USD));
    f.v().deposit(&f.owner, &BTC);
    f.v().deposit(&f.owner_b, &BTC);

    f.grant_for(&f.owner, 20_000 * USD, 20_000 * USD, 5000);
    // B alone would fit; B on top of A does not — and again it is the acceptance that says so,
    // because that is where the exposure is actually taken on.
    f.propose_for(&f.owner_b, 10_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        f.v().try_accept_grant(&f.owner_b, &f.operator),
        Error::ExposureCapExceeded
    );
    f.v().revoke_authority(&f.owner_b, &f.operator);
    f.grant_for(&f.owner_b, 10_000 * USD, 10_000 * USD, 5000);
    assert_eq!(f.v().get_reserve().committed, 30_000 * USD);
    f.check_books();
}

/// Invariant I3 in its absolute form, under a mixed sequence across three customers.
///
/// On chain it is now differential — each call asserts it moved `outstanding` by exactly what it
/// moved that customer's `principal`. If the differential form were right at every step and the
/// sum still drifted, this is the assertion that would catch it, and nothing else would.
#[test]
fn outstanding_equals_the_sum_of_every_principal() {
    let f = F::new();
    f.v().fund_reserve(&(300_000 * USD));
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 10_000 * USD);
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 7_000 * USD);
    f.arm_for(&f.owner_c, BTC, 20_000 * USD, 90, 3_000 * USD);

    assert_eq!(f.raw_reserve().outstanding, 20_000 * USD);
    f.check_books();

    f.repay_as(&f.owner_b, &f.third, &(2_000 * USD));
    f.draw_for(&f.owner_c, 91, 5_000 * USD);
    f.warp_fresh(30 * 86_400, P60K);
    for who in f.owners() {
        f.v().accrue(&who);
    }
    // Interest changed the debt of all three and `outstanding` of none — it is principal only.
    assert_eq!(f.raw_reserve().outstanding, 23_000 * USD);
    f.check_books();

    f.set_price(P16K); // A is past the threshold; B and C are not
    f.v().liquidate(&f.owner, &f.liquidator, &(3_000 * USD), &0);
    f.check_books();

    f.set_price(12_000 * 100_000_000_000_000); // now C is too
    f.v().lender_liquidate(&f.owner_c, &(1_000 * USD));
    f.check_books();
}

// ---------------------------------------------------------------------------
// One clock, many settlements
// ---------------------------------------------------------------------------

/// A customer who joins a year late owes interest on **their own** elapsed time.
///
/// This is the bug the `Position` default exists to prevent. A default `last_index` of
/// `INDEX_SCALE` would bill the newcomer for every hour the market accrued before they arrived —
/// here, a whole year of it — and the books would still balance, so nothing else would notice.
#[test]
fn a_late_joiner_is_not_billed_for_the_markets_history() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));

    // A borrows at t = 0 and the market runs for a year.
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 10_000 * USD);
    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner);
    assert_eq!(f.raw_position_of(&f.owner).interest, 1_000 * USD);
    assert!(f.raw_index().index > crate::INDEX_SCALE);

    // B joins now and borrows the same amount for one more year.
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 10_000 * USD);
    f.warp_fresh(365 * 86_400, P60K);
    f.v().accrue(&f.owner_b);

    // Exactly one year of interest, not two.
    assert_eq!(f.raw_position_of(&f.owner_b).interest, 1_000 * USD);
    f.check_books();
}

/// Two customers who borrow different amounts over different spans each owe their own figure, off
/// one shared index.
#[test]
fn the_shared_clock_settles_each_position_separately() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 10_000 * USD);

    f.warp_fresh(182 * 86_400, P60K); // half a year
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 20_000 * USD);
    f.warp_fresh(183 * 86_400, P60K); // and the other half

    f.v().accrue(&f.owner);
    f.v().accrue(&f.owner_b);

    // A: $10 000 for 365 days at 10 %. B: $20 000 for 183 days.
    //
    // A's figure is **one unit** under the round $1 000.00 that a single 365-day advance
    // produces, and that is the whole cost of the index being integer: each advance floors
    // `INDEX_SCALE × rate × dt / D`, and B's arrival split the year into two advances. The loss
    // is bounded by `principal / INDEX_SCALE` per advance — on this position, 10⁻⁷ of a debt
    // unit — and it rounds in the borrower's favour, which is the documented exception to Đ-15.
    assert_eq!(f.raw_position_of(&f.owner).interest, 9_999_999_999);
    assert_eq!(f.raw_position_of(&f.owner_b).interest, 10_027_397_260);
    f.check_books();
}

/// Nobody has to touch a position for it to stay correct. The index carries the history, so a
/// position untouched for a year settles in one call whenever it is next used.
#[test]
fn an_untouched_position_settles_correctly_when_it_wakes() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 10_000 * USD);
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 10_000 * USD);

    // Only A is ever accrued, once a month, for a year.
    for m in 1..=12u64 {
        f.warp_fresh(30 * 86_400, P60K);
        f.v().accrue(&f.owner);
        let _ = m;
    }
    // B has not been touched at all, and its view already knows what it owes.
    assert_eq!(f.raw_position_of(&f.owner_b).interest, 0);
    let owed = f.v().get_position(&f.owner_b).interest;
    f.v().accrue(&f.owner_b);
    assert_eq!(f.raw_position_of(&f.owner_b).interest, owed);
    assert_eq!(
        f.raw_position_of(&f.owner).interest,
        f.raw_position_of(&f.owner_b).interest,
        "the same principal over the same span must owe the same interest"
    );
    f.check_books();
}

// ---------------------------------------------------------------------------
// TTL must not grow with the customer base
// ---------------------------------------------------------------------------

/// `bump` touches the shared keys plus the one position the call named — never every position.
///
/// The single-customer version extended every persistent key it knew about. In a market that
/// would cost more with every customer who joined, until nobody could afford to transact. The
/// assertion is that a call by A leaves B's TTL exactly where it was.
///
/// `extend_ttl` only acts when the live TTL has fallen below `TTL_THRESHOLD`, so the ledger is
/// advanced past that point first — otherwise the call would be a no-op for everybody and the
/// test would pass without proving anything.
#[test]
fn bump_is_bounded_and_does_not_touch_other_customers() {
    let f = F::new();
    f.v().fund_reserve(&(200_000 * USD));
    f.arm_for(&f.owner, BTC, 20_000 * USD, 1, 5_000 * USD);
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 5_000 * USD);

    let ttl_of = |who: &soroban_sdk::Address| {
        f.e.as_contract(&f.id, || {
            f.e.storage()
                .persistent()
                .get_ttl(&DataKey::Position(who.clone()))
        })
    };

    // Decay both positions past the threshold. Ledger sequence moves; the clock does not, so the
    // price stays fresh and nothing else about the scenario changes.
    f.e.ledger()
        .with_mut(|l| l.sequence_number += crate::TTL_EXTEND - crate::TTL_THRESHOLD + 1);

    let a_before = ttl_of(&f.owner);
    let b_before = ttl_of(&f.owner_b);
    assert_eq!(a_before, b_before, "the two should have decayed together");

    f.v().deposit(&f.owner, &1);

    assert!(
        ttl_of(&f.owner) > a_before,
        "A transacted and A's own position was not refreshed"
    );
    assert_eq!(
        ttl_of(&f.owner_b),
        b_before,
        "A's call refreshed B's position — bump is not bounded and the cost grows with the \
         customer base"
    );
    f.check_books();
}

// ---------------------------------------------------------------------------
// Property: random traffic across three customers
// ---------------------------------------------------------------------------

/// The invariants under a mixed sequence, with three customers acting against one another's
/// capital. `check_books` runs after **every** step, so `outstanding == Σ principal` is asserted
/// thousands of times over.
#[test]
fn invariants_hold_across_customers_under_random_traffic() {
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }
    let mut rng = Rng(0x4A5E_1234_9876_0001);

    for _ in 0..4 {
        let f = F::new();
        f.v().fund_reserve(&(300_000 * USD));
        for (i, who) in f.owners().iter().enumerate() {
            f.v().deposit(who, &BTC);
            f.grant_for(who, 20_000 * USD, 5_000 * USD, 5000);
            let _ = i;
        }
        let mut draw_id = 0u8;

        for _ in 0..30 {
            let who = f.owners()[rng.below(3) as usize].clone();
            match rng.below(8) {
                0 => {
                    let _ = f.v().try_deposit(&who, &(BTC / 10));
                }
                1 => {
                    let _ = f.v().try_withdraw(&who, &(BTC / 20));
                }
                2 => {
                    draw_id = draw_id.wrapping_add(1);
                    let _ = f
                        .v()
                        .try_draw(&who, &f.operator, &id32(&f.e, draw_id), &(1_000 * USD));
                }
                3 => {
                    let _ =
                        f.v()
                            .try_repay(&who, &f.third, &(500 * USD), &Some(f.operator.clone()));
                }
                4 => {
                    let _ = f.v().try_liquidate(&who, &f.liquidator, &(500 * USD), &0);
                }
                5 => {
                    let _ = f.v().try_lender_liquidate(&who, &(500 * USD));
                }
                6 => {
                    let _ = f.v().try_revoke_authority(&who, &f.operator);
                }
                _ => {
                    f.warp(rng.below(200_000) + 1);
                    f.set_price(
                        [P60K, P16K, P8K, 30_000 * 100_000_000_000_000][rng.below(4) as usize],
                    );
                    let _ = f.v().try_accrue(&who);
                }
            }
            f.check_books();
        }
    }
}
