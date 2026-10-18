//! The revolving credit line.
//!
//! Nothing in the deck specifies this behaviour; it is the resolution recorded in
//! `docs/02-contract-interface.md`. Two independent ceilings govern borrowing — the static `cap − drawn` and
//! the dynamic LTV gate — and only **principal** repayment reopens the static one.

use super::harness::*;
use crate::Error;

fn drawn() -> F {
    let f = F::new();
    f.armed(); // cap $20 000, per-tx $5 000
    f.draw(1, 5_000 * USD);
    f
}

/// The line revolves: `drawn` falls by the principal repaid and `committed` rises by the same,
/// so the capital is available to lend again without the lender doing anything.
#[test]
fn repaying_principal_reopens_the_line() {
    let f = drawn();
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 5_000 * USD);
    assert_eq!(f.v().get_reserve().committed, 15_000 * USD);

    f.repay_as(&f.owner, &f.owner, &(2_000 * USD));

    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 3_000 * USD);
    assert_eq!(f.v().get_reserve().committed, 17_000 * USD);
    assert_eq!(f.v().get_reserve().outstanding, 3_000 * USD);

    // And the reopened headroom is genuinely usable.
    f.draw(2, 5_000 * USD);
    assert_eq!(f.raw_position().principal, 8_000 * USD);
    f.check_books();
}

/// Revocation already released that grant's `committed`. Crediting it again on repayment would
/// double-count capital and inflate `committed` out of nothing.
#[test]
fn repaying_after_revoke_does_not_release_capital_twice() {
    let f = drawn();
    f.revoke(&f.owner);
    let before = f.v().get_reserve();
    assert_eq!(before.committed, 0);

    // Naming the revoked grant is allowed and reopens nothing. It must not fail: repayment is the
    // one action the contract never blocks, and the grant's absence says nothing about the debt.
    f.repay_as(&f.owner, &f.owner, &(2_000 * USD));

    let after = f.v().get_reserve();
    assert_eq!(after.committed, 0);
    assert_eq!(after.outstanding, 3_000 * USD);
    assert_eq!(after.free, before.free + 2_000 * USD);
    f.check_books();
}

/// `position.principal` can exceed the *current* grant's `drawn`: grant A draws and is revoked,
/// grant B draws less, and a repayment covering both would drive `drawn` negative without the
/// clamp.
#[test]
fn headroom_restoration_is_clamped_to_the_current_grants_drawn() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);
    f.revoke(&f.owner);

    f.grant(4_000 * USD, 4_000 * USD, 5000);
    f.draw(2, 4_000 * USD);
    assert_eq!(f.raw_position().principal, 9_000 * USD);
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 4_000 * USD);

    // Repay more than grant B ever drew.
    f.repay_as(&f.owner, &f.owner, &(9_000 * USD));

    let g = f.grant_of(&f.owner).unwrap();
    assert_eq!(g.drawn, 0); // floors, never negative
    assert_eq!(f.v().get_reserve().committed, 4_000 * USD);
    assert_eq!(f.v().get_reserve().outstanding, 0);
    f.check_books();
}

#[test]
fn repaying_interest_does_not_move_the_line() {
    let f = drawn();
    f.warp_fresh(30 * 86_400, P60K);
    f.v().accrue(&f.owner);
    let interest = f.raw_position().interest;
    assert!(interest > 0);

    let g_before = f.grant_of(&f.owner).unwrap();
    let r_before = f.v().get_reserve();

    f.repay_as(&f.owner, &f.owner, &interest);

    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, g_before.drawn);
    assert_eq!(f.v().get_reserve().committed, r_before.committed);
    assert_eq!(f.raw_position().interest, 0);
    f.check_books();
}

/// The static ceiling and the dynamic one move independently, and the interface must say which
/// is biting because the remedies are opposite — repay versus deposit more.
#[test]
fn the_two_ceilings_are_independent() {
    let f = F::new();
    f.funded();
    f.grant(100_000 * USD, 100_000 * USD, 5000);

    // Price-driven ceiling: $26 400 against 1 BTC at $60 000.
    let q = f.v().quote_draw(&f.owner, &f.operator, &0);
    assert_eq!(q.cap_headroom, 100_000 * USD);
    assert_eq!(q.ltv_headroom, 26_400 * USD);
    assert_eq!(q.binding, crate::Binding::Ltv);

    f.draw(1, 20_000 * USD);

    // Depositing more collateral raises the dynamic ceiling without touching the static one.
    f.v().deposit(&f.owner, &BTC);
    let q = f.v().quote_draw(&f.owner, &f.operator, &0);
    assert_eq!(q.cap_headroom, 80_000 * USD);
    assert_eq!(q.ltv_headroom, 52_800 * USD - 20_000 * USD);

    // A price crash lowers the dynamic ceiling to zero while the static one is untouched.
    f.set_price(P16K); // eligible 2 BTC × $16 000 × 0.88 = $28 160 → allows $14 080
    let q = f.v().quote_draw(&f.owner, &f.operator, &0);
    assert_eq!(q.cap_headroom, 80_000 * USD);
    assert_eq!(q.ltv_headroom, 0);
    assert_eq!(q.binding, crate::Binding::Ltv);
    f.check_books();
}

/// A grant with nothing drawn against it can be revoked and the capital withdrawn — the line
/// does not trap the lender's money once the customer stops using it.
#[test]
fn the_full_cycle_returns_every_unit() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);
    f.repay_as(&f.owner, &f.owner, &(5_000 * USD));
    f.revoke(&f.owner);

    let r = f.v().get_reserve();
    assert_eq!(r.total, 100_000 * USD);
    assert_eq!(r.committed, 0);
    assert_eq!(r.outstanding, 0);
    assert_eq!(r.free, 100_000 * USD);

    f.v().withdraw_reserve(&(100_000 * USD));
    assert_eq!(f.debt_t().balance(&f.id), 0);

    f.v().withdraw(&f.owner, &BTC);
    assert_eq!(f.dep_t().balance(&f.owner), f.minted_dep());
    f.check_books();
}

#[test]
fn repay_rejects_non_positive() {
    let f = drawn();
    assert_err!(
        f.v()
            .try_repay(&f.owner, &f.owner, &0, &Some(f.operator.clone())),
        Error::InvalidAmount
    );
    assert_err!(
        f.v()
            .try_repay(&f.owner, &f.owner, &-1, &Some(f.operator.clone())),
        Error::InvalidAmount
    );
}
