//! Deposit and withdraw.
//!
//! The asymmetry is the whole design: adding collateral can only improve a position, so it is
//! never blocked and never reads a price. Removing it can only worsen one, so it post-checks
//! LTV — except when there is no debt, where the check is vacuous.

use super::harness::*;
use crate::Error;

/// $10 000 of debt against 1 BTC at $60 000.
fn with_debt() -> F {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f
}

#[test]
fn deposit_accumulates_and_needs_no_price() {
    let f = F::new();
    f.kill_oracle(); // risk-reducing actions are never blocked by a dead feed
    f.v().deposit(&f.owner, &BTC);
    f.v().deposit(&f.owner, &(BTC / 2));
    assert_eq!(f.raw_position().deposit, BTC + BTC / 2);
    assert_eq!(f.dep_t().balance(&f.id), BTC + BTC / 2);
    assert_eq!(f.dep_t().balance(&f.owner), f.minted_dep() - BTC - BTC / 2);
    f.check_books();
}

#[test]
fn deposit_works_while_paused() {
    let f = F::new();
    f.v().pause();
    f.v().deposit(&f.owner, &BTC);
    assert_eq!(f.raw_position().deposit, BTC);
}

#[test]
fn deposit_rejects_non_positive() {
    let f = F::new();
    assert_err!(f.v().try_deposit(&f.owner, &0), Error::InvalidAmount);
    assert_err!(f.v().try_deposit(&f.owner, &-1), Error::InvalidAmount);
}

#[test]
fn withdraw_more_than_held() {
    let f = F::new();
    f.v().deposit(&f.owner, &BTC);
    assert_err!(
        f.v().try_withdraw(&f.owner, &(BTC + 1)),
        Error::InsufficientDeposit
    );
    assert_eq!(f.raw_position().deposit, BTC);
}

#[test]
fn withdraw_rejects_non_positive() {
    let f = F::new();
    f.v().deposit(&f.owner, &BTC);
    assert_err!(f.v().try_withdraw(&f.owner, &0), Error::InvalidAmount);
    assert_err!(f.v().try_withdraw(&f.owner, &-5), Error::InvalidAmount);
}

#[test]
fn withdraw_while_paused() {
    let f = F::new();
    f.v().deposit(&f.owner, &BTC);
    f.v().pause();
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::Paused);
}

/// The boundary, to the unit.
///
/// `eligible = deposit × 5280` exactly at $60 000 with a 12% haircut, and the gate is
/// `debt × BPS <= eligible × max_ltv`, so $10 000 of debt needs `eligible >= 200_000_000_000`,
/// i.e. `deposit >= 37_878_788` — the ceiling of `2e11 / 5280`.
#[test]
fn withdraw_to_the_max_ltv_boundary_exactly() {
    let f = with_debt();
    let keep = 37_878_788i128;

    // One unit past the boundary fails...
    assert_err!(
        f.v().try_withdraw(&f.owner, &(BTC - keep + 1)),
        Error::HealthTooLow
    );
    assert_eq!(f.raw_position().deposit, BTC);

    // ...and the boundary itself succeeds.
    f.v().withdraw(&f.owner, &(BTC - keep));
    assert_eq!(f.raw_position().deposit, keep);
    assert_eq!(f.dep_t().balance(&f.id), keep);
    // ltv_after is reported as exactly 5000 — the display figure rounds up, and the gate that
    // let this through did not use it.
    assert_eq!(f.v().get_position(&f.owner).ltv_bps, 5000);
    f.check_books();
}

/// A position with no debt has an LTV of zero at any price, so the check is vacuous — and
/// skipping it means a permanently dead oracle cannot trap the collateral of a customer who has
/// already repaid everything.
#[test]
fn withdraw_with_no_debt_needs_no_price() {
    let f = F::new();
    f.v().deposit(&f.owner, &BTC);
    f.kill_oracle();
    f.v().withdraw(&f.owner, &BTC);
    assert_eq!(f.raw_position().deposit, 0);
    assert_eq!(f.dep_t().balance(&f.owner), f.minted_dep());
    f.check_books();
}

#[test]
fn withdraw_with_debt_and_a_dead_oracle_is_blocked() {
    let f = with_debt();
    f.kill_oracle();
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceInvalid);
}

/// The reported quantity and the real balance must agree after every operation. This is the
/// collateral counterpart of I2, and the only assertion that catches a transfer that never
/// happened.
#[test]
fn reported_quantity_tracks_the_real_balance() {
    let f = F::new();
    for amount in [BTC, BTC / 3, 7i128, 1i128] {
        f.v().deposit(&f.owner, &amount);
        f.check_books();
    }
    f.v().withdraw(&f.owner, &(BTC / 2));
    f.check_books();
    f.v().withdraw(&f.owner, &1);
    f.check_books();
}
