//! Arithmetic, valuation, and the display formulas.
//!
//! Unit tests against the pure modules. Reference vectors from `docs/03-credit-math.md`.
//!
//! Two scales meet in `health`, and mixing them is the easiest way to be wrong by a power of
//! ten: `deposit` is in deposit-token units and needs a price to become money; `principal` and
//! `interest` are in debt-token units and need only a rescale. Half of this file exists to pin
//! that boundary.

#![allow(dead_code)]

use super::harness::*;
use crate::{health, math, Config, Error, Position};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env};

fn conf(e: &Env, deposit_decimals: u32, debt_decimals: u32) -> Config {
    let a = Address::generate(e);
    let b = Address::generate(e);
    Config {
        lender: b.clone(),
        deposit_token: a.clone(),
        deposit_decimals,
        deposit_feed: feed(),
        debt_token: b.clone(),
        debt_decimals,
        oracle: a,
        haircut_bps: 1200,
        max_ltv_bps: 5000,
        liq_threshold_bps: 7000,
        full_liq_ltv_bps: 9000,
        liq_bonus_bps: 800,
        close_factor_bps: 5000,
        price_max_age: 300,
        rate_bps_year: 1000,
        vault_cap: 1_000_000 * USD,
        dust_threshold: 10 * USD,
    }
}

fn pos(deposit: i128, principal: i128, interest: i128) -> Position {
    Position {
        deposit,
        principal,
        interest,
        last_index: crate::INDEX_SCALE,
        accrual_rem: 0,
    }
}

// ---------------------------------------------------------------------------
// Valuation
// ---------------------------------------------------------------------------

/// The fixture's shift is `8 + 14 − 7 = +15`, so the conversion **divides**.
#[test]
fn valuation_positive_shift() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let v = health::value(&e, &c, &pos(BTC, 0, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(v.market, 600_000_000_000); // $60 000.0000000
    assert_eq!(v.eligible, 528_000_000_000); // × 0.88
    assert_eq!(v.debt, 0);
}

/// `deposit_decimals + price_decimals < USD_DECIMALS` makes the shift **negative**, so the
/// conversion multiplies instead.
///
/// This is the classic place to implement one direction correctly and silently round the other
/// one down, which is why it gets its own row in the matrix rather than being assumed covered.
#[test]
fn valuation_negative_shift() {
    let e = Env::default();
    let c = conf(&e, 2, DEBT_DEC); // shift = 2 + 2 − 7 = −3
                                   // price 3 at 2 decimals = $0.03; 500 units at 2 decimals = 5.00 tokens → $0.15
    let v = health::value(&e, &c, &pos(500, 0, 0), 3, 2).unwrap();
    assert_eq!(v.market, 1_500_000); // 500 × 3 × 10^3 = 0.15 at 7 dp
    assert_eq!(v.eligible, 1_320_000);
}

/// With `debt_decimals != 7`, a raw debt figure is **not** comparable to `eligible_value`.
/// Every gate must see the rescaled USD number.
#[test]
fn debt_value_rescales_when_decimals_differ() {
    let e = Env::default();

    // 18-decimal debt token: 1.5 tokens = 1.5e18 raw → $1.5000000 = 15_000_000 at 7 dp.
    let c18 = conf(&e, DEP_DEC, 18);
    assert_eq!(
        health::debt_value_usd(&e, 1_500_000_000_000_000_000, &c18).unwrap(),
        15_000_000
    );
    // Rounds UP: the direction that favours the lender.
    assert_eq!(health::debt_value_usd(&e, 1, &c18).unwrap(), 1);

    // 6-decimal debt token: 1.5 tokens = 1_500_000 raw → 15_000_000 at 7 dp.
    let c6 = conf(&e, DEP_DEC, 6);
    assert_eq!(
        health::debt_value_usd(&e, 1_500_000, &c6).unwrap(),
        15_000_000
    );

    // 7 decimals is the identity — the cheapest path, and the common case.
    let c7 = conf(&e, DEP_DEC, 7);
    assert_eq!(
        health::debt_value_usd(&e, 1_234_567, &c7).unwrap(),
        1_234_567
    );
}

/// A gate fed raw debt-token units instead of the USD figure passes when it should fail.
#[test]
fn gate_uses_the_usd_figure_not_raw_units() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, 6); // debt token has 6 decimals

    // $30 000 of debt as 6-decimal raw units is 30_000_000_000 — which, read as a 7-decimal
    // USD figure, looks like $3 000. Against $52 800 of eligible value the first is over the
    // 50% ceiling and the second is comfortably under it.
    let v = health::value(&e, &c, &pos(BTC, 30_000_000_000, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(v.eligible, 528_000_000_000);
    assert_eq!(v.debt, 300_000_000_000); // rescaled, not raw
    assert_eq!(health::within_ltv(&e, &v, 5000).unwrap(), false);
}

// ---------------------------------------------------------------------------
// The boundary
// ---------------------------------------------------------------------------

/// `health_bps == 10_000` at exactly the liquidation boundary.
///
/// This is the test that proves `BPS` is not in the denominator. The wrong form —
/// `eligible × liq_thr / (debt × BPS)` — reads plausibly and yields **1** here.
#[test]
fn health_is_exactly_10000_at_the_boundary() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    // eligible 528e9 × 0.70 = 369.6e9 of debt is exactly the threshold.
    let v = health::value(&e, &c, &pos(BTC, 369_600_000_000, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(v.debt, 369_600_000_000);
    assert_eq!(health::health_bps(&e, &v, &c).unwrap(), 10_000);
    assert_eq!(health::ltv_bps(&e, &v).unwrap(), 7_000);
    // The boundary is INCLUSIVE — ltv == threshold is liquidatable.
    assert_eq!(health::is_liquidatable(&e, &v, &c).unwrap(), true);
}

/// The predicate flips exactly at the boundary, not one unit either side of it.
#[test]
fn liquidation_predicate_flips_exactly_at_the_threshold() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);

    let under = health::value(&e, &c, &pos(BTC, 369_599_999_999, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::is_liquidatable(&e, &under, &c).unwrap(), false);

    let over = health::value(&e, &c, &pos(BTC, 369_600_000_001, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::is_liquidatable(&e, &over, &c).unwrap(), true);
}

/// `within_ltv` is cross-multiplied, so the ceiling is exact rather than rounded.
///
/// `ltv_bps` rounds up by as much as a basis point — on a 5000 bps ceiling against $52 800 of
/// eligible value that is roughly $5.28 of borrowing power, the coarsest rounding effect
/// anywhere in the contract, and entirely avoidable by not dividing.
#[test]
fn ltv_gate_boundary_is_exact() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);

    let at = health::value(&e, &c, &pos(BTC, 264_000_000_000, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::within_ltv(&e, &at, 5000).unwrap(), true);

    let over = health::value(&e, &c, &pos(BTC, 264_000_000_001, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::within_ltv(&e, &over, 5000).unwrap(), false);
    // ...while the DISPLAY figure still reads 5000, because it rounds. A gate built on this
    // number would let the extra unit through.
    assert_eq!(health::ltv_bps(&e, &at).unwrap(), 5000);
}

// ---------------------------------------------------------------------------
// Degenerate inputs
// ---------------------------------------------------------------------------

#[test]
fn zero_eligible_value_with_debt_does_not_divide_by_zero() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let v = health::value(&e, &c, &pos(0, 100 * USD, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(v.eligible, 0);
    assert_eq!(health::ltv_bps(&e, &v).unwrap(), u32::MAX);
    assert_eq!(health::health_bps(&e, &v, &c).unwrap(), 0);
    assert_eq!(health::is_liquidatable(&e, &v, &c).unwrap(), true);
}

#[test]
fn zero_debt_is_infinitely_healthy() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let v = health::value(&e, &c, &pos(BTC, 0, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::ltv_bps(&e, &v).unwrap(), 0);
    assert_eq!(health::health_bps(&e, &v, &c).unwrap(), u32::MAX);
    assert_eq!(health::is_liquidatable(&e, &v, &c).unwrap(), false);
    assert_eq!(
        health::liq_price(&e, &c, &pos(BTC, 0, 0), &v, ORACLE_DEC).unwrap(),
        0
    );
}

#[test]
fn zero_deposit_and_zero_debt_is_healthy() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let v = health::value(&e, &c, &pos(0, 0, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::band(&e, &v, &c).unwrap(), crate::Band::Healthy);
}

// ---------------------------------------------------------------------------
// 256-bit intermediates
// ---------------------------------------------------------------------------

/// `qty × price` overflows `i128` well inside plausible balances once token and price decimals
/// are both large. The 256-bit intermediate is what makes it work; without it this panics.
#[test]
fn product_exceeding_i128_does_not_overflow() {
    let e = Env::default();
    let a = 1_000_000_000_000_000_000_000i128; // 1e21
    let b = 1_000_000_000_000_000_000i128; // 1e18   → 1e39 > i128::MAX (~1.7e38)
    let d = 1_000_000_000_000_000i128; // 1e15
    assert_eq!(
        math::mul_div_floor(&e, a, b, d).unwrap(),
        1_000_000_000_000_000_000_000_000i128 // 1e24
    );
}

/// A *result* that does not fit is a named error, not a bare host trap. The runbook can act on
/// code 4; it cannot act on an opaque panic.
#[test]
fn result_exceeding_i128_is_math_overflow() {
    let e = Env::default();
    assert_eq!(
        math::mul_div_floor(&e, i128::MAX, 10, 1).unwrap_err(),
        Error::MathOverflow
    );
}

#[test]
fn negative_operands_and_zero_divisor_are_math_overflow() {
    let e = Env::default();
    assert_eq!(
        math::mul_div_floor(&e, -1, 2, 3).unwrap_err(),
        Error::MathOverflow
    );
    assert_eq!(
        math::mul_div_floor(&e, 1, -2, 3).unwrap_err(),
        Error::MathOverflow
    );
    assert_eq!(
        math::mul_div_floor(&e, 1, 2, 0).unwrap_err(),
        Error::MathOverflow
    );
    assert_eq!(
        math::mul_div_floor(&e, 1, 2, -3).unwrap_err(),
        Error::MathOverflow
    );
    assert_eq!(
        math::scale(&e, -1, 2, false).unwrap_err(),
        Error::MathOverflow
    );
}

// ---------------------------------------------------------------------------
// Rounding directions
// ---------------------------------------------------------------------------

/// Every fixture here is deliberately indivisible. A test with a clean quotient cannot tell
/// `floor` from `ceil`, which is how a backwards rounding direction survives a full suite.
#[test]
fn rounding_directions() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);

    // 1 · mul_div_floor / mul_div_ceil, 7/3
    assert_eq!(math::mul_div_floor(&e, 7, 1, 3).unwrap(), 2);
    assert_eq!(math::mul_div_ceil(&e, 7, 1, 3).unwrap(), 3);
    // ...and an exact quotient must NOT be bumped by the ceil path.
    assert_eq!(math::mul_div_ceil(&e, 6, 1, 3).unwrap(), 2);

    // 2 · scale down rounds as asked, in both directions
    assert_eq!(math::scale(&e, 1_234_567, 3, false).unwrap(), 1_234);
    assert_eq!(math::scale(&e, 1_234_567, 3, true).unwrap(), 1_235);
    // 3 · scale up is exact — multiplication cannot need rounding
    assert_eq!(math::scale(&e, 1_234, -3, false).unwrap(), 1_234_000);
    assert_eq!(math::scale(&e, 1_234, -3, true).unwrap(), 1_234_000);

    // 4 · collateral value rounds DOWN — the vault never overstates what backs a loan
    let v = health::value(&e, &c, &pos(3, 0, 0), 1, 0).unwrap();
    // 3 × 1 / 10^1 = 0.3 → 0
    assert_eq!(v.market, 0);

    // 5 · debt value rounds UP — the vault never understates what is owed
    let c18 = conf(&e, DEP_DEC, 18);
    assert_eq!(
        health::debt_value_usd(&e, 100_000_000_001, &c18).unwrap(),
        2 // 1.00000000001 units → 2, not 1
    );

    // 6 · eligible value rounds DOWN (the haircut favours the lender)
    let v = health::value(&e, &c, &pos(BTC, 0, 0), P60K + 1, ORACLE_DEC).unwrap();
    assert_eq!(v.market, 600_000_000_000);
    assert_eq!(v.eligible, 528_000_000_000);

    // 7 · from_usd rounds DOWN — a liquidator receives one unit less, the vault keeps it
    assert_eq!(health::from_usd(&e, 1, &c, P60K, ORACLE_DEC).unwrap(), 0);

    // 8 · seize value rounds DOWN
    assert_eq!(crate::liquidation::seize_value(&e, &c, 7).unwrap(), 7); // 7 × 1.08 = 7.56

    // 9 · max_repay rounds DOWN
    let v = health::value(&e, &c, &pos(BTC, 369_600_000_001, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(
        crate::liquidation::max_repay(&e, &c, &v).unwrap(),
        184_800_000_000
    );

    // 10 · ltv_bps rounds UP (a displayed LTV never flatters the borrower)
    let v = health::value(&e, &c, &pos(BTC, 264_000_000_001, 0), P60K, ORACLE_DEC).unwrap();
    assert_eq!(health::ltv_bps(&e, &v).unwrap(), 5001);

    // 11 · health_bps rounds DOWN (a displayed health never flatters the borrower either).
    // Exactly 14_000 at debt 264e9; one unit more must not round back up to it.
    assert_eq!(health::health_bps(&e, &v, &c).unwrap(), 13_999);
}

/// `liq_price` is in **oracle price scale**, so it can be compared directly to what the feed
/// reports. Inverting the valuation identity reintroduces the `10^shift` factor that `value_usd`
/// divided out; without it the number is in no unit at all.
#[test]
fn liq_price_is_in_oracle_scale() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let p = pos(BTC, 264_000_000_000, 0); // $26 400 against 1 BTC

    let v = health::value(&e, &c, &p, P60K, ORACLE_DEC).unwrap();
    let lp = health::liq_price(&e, &c, &p, &v, ORACLE_DEC).unwrap();

    // debt × BPS² × 10^15 / (deposit × 8800 × 7000)
    //   = 264e9 × 1e8 × 1e15 / (1e8 × 8800 × 7000) = $42 857.142857… → rounds up
    let expected = 42_857 * 100_000_000_000_000 + 14_285_714_285_715;
    assert_eq!(lp, expected);

    // Sanity: at that price the position is at (or just past) the threshold, and one price unit
    // higher it is not.
    let at = health::value(&e, &c, &p, lp, ORACLE_DEC).unwrap();
    assert_eq!(health::is_liquidatable(&e, &at, &c).unwrap(), true);
}

/// `pow10` refuses an exponent that cannot correspond to a real decimals value.
#[test]
fn pow10_is_bounded() {
    assert_eq!(math::pow10(38).unwrap(), 10i128.pow(38));
    assert_eq!(math::pow10(39).unwrap_err(), Error::MathOverflow);
}

#[test]
fn band_thresholds() {
    let e = Env::default();
    let c = conf(&e, DEP_DEC, DEBT_DEC);
    let band = |debt: i128| {
        let v = health::value(&e, &c, &pos(BTC, debt, 0), P60K, ORACLE_DEC).unwrap();
        health::band(&e, &v, &c).unwrap()
    };
    // eligible 528e9. Watch starts at 0.9 × max_ltv = 45% → 237.6e9.
    assert_eq!(band(0), crate::Band::Healthy);
    assert_eq!(band(237_599_999_999), crate::Band::Healthy);
    assert_eq!(band(237_600_000_000), crate::Band::Watch);
    // The boundary belongs to Healthy/Watch, not Warning: the withdrawal gate permits
    // ltv_after == max_ltv, so a band that said Warning there would tell the interface to block
    // something the contract allows.
    assert_eq!(band(264_000_000_000), crate::Band::Watch);
    assert_eq!(band(264_000_000_001), crate::Band::Warning);
    assert_eq!(band(369_599_999_999), crate::Band::Warning);
    assert_eq!(band(369_600_000_000), crate::Band::Liquidatable);
}
