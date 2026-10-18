//! Construction and configuration validation.
//!
//! The constructor is the **only** opportunity to reject a bad configuration. Nothing here can
//! be changed afterwards — `Config` is frozen, `oracle` included — so a market deployed with a
//! threshold that is too generous carries it for life, and correcting it costs a new market plus
//! a voluntary migration by every customer in the old one. Every row here is a mistake that would
//! otherwise be permanent for everybody at once.
//!
//! These use `#[should_panic]` rather than `assert_err!`: `Env::register` runs the constructor
//! inside the deploy and panics on failure, so there is no `try_` form to unwrap. The expected
//! string still pins the exact code — `Error(Contract, #3)` — which is what the standard asks
//! for.

use super::harness::*;

#[test]
fn valid_config_deploys_and_reads_decimals_from_the_tokens() {
    let f = F::new();
    let c = f.v().get_config();
    // cfg() passes 99 for both. The constructor is specified to ignore the caller and ask the
    // token contracts, because one wrong digit makes every valuation off by a factor of ten and
    // nothing downstream catches it — the numbers stay internally consistent.
    assert_eq!(c.deposit_decimals, DEP_DEC);
    assert_eq!(c.debt_decimals, DEBT_DEC);
    assert_eq!(c.lender, f.lender);
    assert_eq!(c.max_ltv_bps, 5000);
    assert_eq!(f.v().is_paused(), false);

    // The interest clock starts at 1.0, not 0 — a zero index makes every settlement divide by
    // zero.
    assert_eq!(f.raw_index().index, crate::INDEX_SCALE);
    assert_eq!(f.raw_index().last_accrual, START);
    f.check_books();
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn deposit_token_equals_debt_token_rejected() {
    F::with(|mut c| {
        c.debt_token = c.deposit_token.clone();
        c
    });
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn max_ltv_at_or_above_liq_threshold_rejected() {
    F::with(|mut c| {
        c.max_ltv_bps = 7000;
        c
    });
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn liq_threshold_at_or_above_full_liq_rejected() {
    F::with(|mut c| {
        c.liq_threshold_bps = 9000;
        c
    });
}

/// The break-even constraint: `full_liq_ltv × (BPS + bonus) <= BPS²`.
///
/// Above that ratio a partial liquidation *raises* LTV instead of lowering it, and the position
/// spirals — every by-the-book liquidation pushing it closer to total loss while the liquidator
/// collects the bonus each time. Catching it at deploy stops the configuration before anyone
/// loses money, rather than discovering it when they have.
#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn break_even_constraint_rejected() {
    // 9300 × 10800 = 100_440_000 > 100_000_000.
    F::with(|mut c| {
        c.full_liq_ltv_bps = 9300;
        c
    });
}

#[test]
fn break_even_constraint_boundary_accepted() {
    // The largest full_liq_ltv the 8% bonus permits: 9259 × 10800 = 99_997_200 <= 100_000_000.
    let f = F::with(|mut c| {
        c.full_liq_ltv_bps = 9259;
        c
    });
    assert_eq!(f.v().get_config().full_liq_ltv_bps, 9259);
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn zero_max_ltv_rejected() {
    F::with(|mut c| {
        c.max_ltv_bps = 0;
        c
    });
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn price_max_age_below_one_minute_rejected() {
    F::with(|mut c| {
        c.price_max_age = 30;
        c
    });
}

#[test]
#[should_panic(expected = "Error(Contract, #3)")]
fn zero_vault_cap_rejected() {
    F::with(|mut c| {
        c.vault_cap = 0;
        c
    });
}

/// There is no `Config.owner` at all. A market vault serves many customers, each holding a
/// `Position` keyed by their own address, and `lender` is the party that funds the shared pool —
/// not an administrator. Who may pull the brake, and for how long, is `governance_tests`.
#[test]
fn there_is_no_owner_field_and_the_lender_is_not_an_admin() {
    let f = F::new();
    let c = f.v().get_config();
    assert_eq!(c.lender, f.lender);
    // The lender cannot make a position liquidatable, because nothing about a position's terms
    // can be changed by anybody — the entry points that once did it no longer exist.
    assert_eq!(c.haircut_bps, 1200);
    assert_eq!(c.max_ltv_bps, 5000);
    assert_eq!(c.liq_threshold_bps, 7000);
}
