//! Price freshness, the outage guard, and the oracle swap.
//!
//! The four freshness checks are one predicate used in two places. If `report_oracle_state` used
//! a looser test — presence and age only — a feed running ten minutes fast would count as fresh
//! there and be rejected with `PriceInvalid` on the strict path. The guard would never record an
//! outage, the grace window would never open, and liquidation would stay blocked with no way to
//! clear it.

use super::harness::*;
use crate::Error;

/// $10 000 of debt, so the strict price path is actually exercised.
fn borrowed() -> F {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f
}

// ---------------------------------------------------------------------------
// The four checks
// ---------------------------------------------------------------------------

#[test]
fn age_exactly_at_the_limit_is_valid_one_second_past_is_not() {
    let f = borrowed();

    f.set_price_at(P60K, f.now() - 300); // price_max_age == 300, inclusive
    assert_eq!(f.v().get_position(&f.owner).price_stale, false);
    f.v().withdraw(&f.owner, &1); // strict path accepts it

    f.set_price_at(P60K, f.now() - 301);
    assert_eq!(f.v().get_position(&f.owner).price_stale, true);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceStale);
}

#[test]
fn no_price_at_all_is_price_invalid() {
    let f = borrowed();
    f.kill_oracle();
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);
    // Distinct from a price of zero, and the view says so without trapping.
    let p = f.v().get_position(&f.owner);
    assert_eq!(p.price_stale, true);
    assert_eq!(p.price_age, u64::MAX);
    assert_eq!(p.market_value, 0);
}

#[test]
fn non_positive_price_is_price_invalid() {
    let f = borrowed();
    f.set_price(0);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);
    f.set_price(-1);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);
}

/// Clock skew between the feed and the ledger is tolerated up to `FUTURE_PRICE_SLACK`; beyond
/// that the feed is not merely early, it is wrong.
#[test]
fn future_timestamps_are_bounded() {
    let f = borrowed();

    f.set_price_at(P60K, f.now() + 30);
    f.v().withdraw(&f.owner, &1); // within slack

    f.set_price_at(P60K, f.now() + 61);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);
}

// ---------------------------------------------------------------------------
// What a stale price does and does not block
// ---------------------------------------------------------------------------

/// Risk-reducing actions are never blocked. This is property **P3**, in its simplest form.
#[test]
fn stale_price_does_not_block_risk_reducing_actions() {
    let f = borrowed();
    f.warp(1_000); // price now 1000 s old

    f.v().deposit(&f.owner, &BTC);
    f.repay_as(&f.owner, &f.owner, &(1_000 * USD));
    f.v().accrue(&f.owner);
    f.freeze(&f.owner, &f.owner);
    f.revoke(&f.owner);
    f.check_books();
}

#[test]
fn stale_price_blocks_withdrawal_only_when_there_is_debt() {
    let f = borrowed();
    f.warp(1_000);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceStale);

    f.v().accrue(&f.owner);
    f.repay_as(&f.owner, &f.owner, &f.v().get_position(&f.owner).debt);
    assert_eq!(f.v().get_position(&f.owner).debt, 0);
    // Debt-free: the LTV check is vacuous at any price, so a permanently dead oracle cannot
    // trap the collateral of a customer who has already repaid everything.
    f.v().withdraw(&f.owner, &BTC);
    assert_eq!(f.raw_position().deposit, 0);
    f.check_books();
}

/// Views must render during an outage. A screen that traps is a screen the customer cannot use
/// at the exact moment they most need to see their position.
#[test]
fn views_never_panic_on_a_bad_price() {
    let f = borrowed();
    f.warp(1_000);

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.price_stale, true);
    assert_eq!(p.price_age, 1_000);
    assert_eq!(p.principal, 10_000 * USD);

    let q = f.v().quote_draw(&f.owner, &f.operator, &(1_000 * USD));
    assert_eq!(q.blocked_reason, Some(Error::PriceStale as u32));

    let l = f.v().quote_liquidation(&f.owner);
    assert_eq!(l.liquidatable, false);
    assert_eq!(l.blocked_reason, Some(Error::PriceStale as u32));

    f.kill_oracle();
    let _ = f.v().get_position(&f.owner);
    let _ = f.v().quote_draw(&f.owner, &f.operator, &1);
    let _ = f.v().quote_liquidation(&f.owner);
    let _ = f.v().get_reserve();
}

// ---------------------------------------------------------------------------
// The outage guard
// ---------------------------------------------------------------------------

/// The guard has to be a separate entry point rather than a side effect of the ordinary price
/// path, because that path *reverts* on a bad price — the state write recording the outage would
/// be rolled back along with the failed call.
#[test]
fn report_oracle_state_records_an_outage_without_panicking() {
    let f = borrowed();
    f.kill_oracle();

    assert_eq!(f.v().report_oracle_state(), false);
    assert_eq!(f.raw_guard().outage_at, f.now());

    // Called again during the same outage: the start time does not move, or the grace window
    // would keep resetting and never expire.
    f.warp(60);
    assert_eq!(f.v().report_oracle_state(), false);
    assert_eq!(f.raw_guard().outage_at, START);
}

/// The same four checks as the strict path. A feed 61 s in the future is an outage here too.
#[test]
fn report_oracle_state_uses_the_strict_predicate() {
    let f = borrowed();
    f.set_price_at(P60K, f.now() + 61);
    assert_eq!(f.v().report_oracle_state(), false);
    assert_eq!(f.raw_guard().outage_at, f.now());
}

/// Without the grace window: the oracle dies for four hours, the market moves 40 %, and the
/// instant it recovers every over-threshold position is liquidated at a price the owner never
/// had a chance to react to.
#[test]
fn recovery_opens_a_grace_window_that_blocks_liquidation() {
    let f = borrowed();
    f.kill_oracle();
    f.v().report_oracle_state();

    f.warp(4 * 3600);
    f.set_price(P16K); // the price gap
    assert_eq!(f.v().report_oracle_state(), true);
    let until = f.now() + crate::LIQUIDATION_GRACE;
    assert_eq!(f.raw_guard().grace_until, until);
    assert_eq!(f.raw_guard().outage_at, 0);

    // The position IS liquidatable, and liquidation is still refused.
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, true);
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0),
        Error::LiquidationGrace
    );
    assert_err!(
        f.v().try_lender_liquidate(&f.owner, &(1_000 * USD)),
        Error::LiquidationGrace
    );

    // One second before the window closes, still refused.
    f.warp(crate::LIQUIDATION_GRACE - 1);
    f.set_price(P16K);
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0),
        Error::LiquidationGrace
    );

    // And then it opens.
    f.warp(1);
    f.set_price(P16K);
    f.v().liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0);
    f.check_books();
}

/// The vault reads the price **at most once per invocation**.
///
/// `liquidate` needs a price in two places — the threshold check and the seize computation. The
/// oracle is an external contract with no guarantee of determinism inside one transaction, so
/// two reads can return two different values: the gate would pass on one price and the seize
/// would settle on another, and nothing would catch it, because each step is individually
/// correct.
///
/// The mock's drift mode makes that visible. Every `lastprice` call moves the stored price by
/// the delta *after* answering, so a second read inside one invocation returns $32 000 instead
/// of $16 000 — which would halve the collateral seized.
#[test]
fn the_price_is_read_at_most_once_per_invocation() {
    let f = borrowed();
    f.set_price(P16K);
    f.orc().set_drift(&P16K);

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &0);

    // $5 000 × 1.08 = $5 400 of collateral at $16 000/BTC = 0.3375 BTC.
    assert_eq!(seized, 33_750_000);
    // At $32 000 it would have been 16 875 000 — half. The number IS the assertion.
    assert_eq!(f.dep_t().balance(&f.liquidator), 33_750_000);
    f.check_books();
}

// ---------------------------------------------------------------------------
// The oracle cannot be swapped
// ---------------------------------------------------------------------------

/// `Config.oracle` is frozen at construction like every other field, so `propose_oracle` and
/// `accept_oracle` do not exist and code 43 is permanently vacant.
///
/// The cost is real: a feed that dies permanently takes `draw` and `liquidate` with it, for every
/// customer, forever. What makes that survivable rather than catastrophic is asserted here — the
/// market can always **wind itself down**. `repay` reads no price, and `withdraw` reads none once
/// the debt is clear, so every customer can still get their collateral out.
///
/// The alternative was a lender-signed swap. It would have handed one address the ability to
/// point the market at a feed of its choosing and liquidate everyone in it after a timelock, and
/// no liveness probe can distinguish a feed that is alive from a feed that is honest.
#[test]
fn a_permanently_dead_feed_still_lets_the_market_wind_down() {
    let f = borrowed();
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 60, 5_000 * USD);

    f.kill_oracle();

    // Nothing risk-increasing works, for anybody.
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &1),
        Error::PriceInvalid
    );
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(100 * USD), &0),
        Error::PriceInvalid
    );
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);

    // And every customer can still leave.
    for who in [f.owner.clone(), f.owner_b.clone()] {
        f.v().accrue(&who);
        let owed = f.v().get_position(&who).debt;
        f.repay_as(&who, &f.third, &owed);
        let held = f.raw_position_of(&who).deposit;
        f.v().withdraw(&who, &held);
        assert_eq!(f.raw_position_of(&who).deposit, 0);
    }
    // ...and the lender can recover the capital that came back.
    f.revoke(&f.owner);
    f.revoke(&f.owner_b);
    let free = f.v().get_reserve().free;
    f.v().withdraw_reserve(&free);
    f.check_books();
}
