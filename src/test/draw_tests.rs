//! The draw gates.
//!
//! Gate **order** is part of the specification, not an implementation detail. The backend
//! branches on which code comes back — 33 means *release headroom and carry on*, 39 means
//! *somebody froze this, escalate* — so a call violating several conditions must return a
//! deterministic code. The ordering test at the bottom is what makes every other row here
//! meaningful; without it the returned code is not a specification.

use super::harness::*;
use crate::Error;

#[test]
fn draw_moves_funds_only_to_the_pinned_settlement_address() {
    let f = F::new();
    f.armed();
    f.draw(1, 1_000 * USD);

    // Property P1, asserted as a NEGATIVE: the operator holds nothing. Asserting only that
    // settlement received something is the weaker test — it passes even if the operator was
    // paid too.
    assert_eq!(f.debt_t().balance(&f.operator), 0);
    assert_eq!(f.debt_t().balance(&f.settlement), 1_000 * USD);

    let g = f.grant_of(&f.owner).unwrap();
    assert_eq!(g.drawn, 1_000 * USD);
    assert_eq!(f.raw_position().principal, 1_000 * USD);
    f.check_books();
}

/// A draw preserves I1 as an **equality**: capital changes drawer without leaving the ledger.
#[test]
fn committed_falls_by_exactly_what_outstanding_rises_by() {
    let f = F::new();
    f.armed();
    let before = f.raw_reserve();
    f.draw(1, 3_000 * USD);
    let after = f.raw_reserve();

    assert_eq!(before.committed - after.committed, 3_000 * USD);
    assert_eq!(after.outstanding - before.outstanding, 3_000 * USD);
    assert_eq!(after.total, before.total);
    assert_eq!(
        before.committed + before.outstanding,
        after.committed + after.outstanding
    );
    f.check_books();
}

#[test]
fn draw_beyond_cap() {
    let f = F::new();
    f.armed(); // cap $20 000, per-tx $5 000
    for i in 1..=4u8 {
        f.draw(i, 5_000 * USD);
    }
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &1),
        Error::CapExceeded
    );
    assert_eq!(f.raw_position().principal, 20_000 * USD);
    f.check_books();
}

#[test]
fn draw_beyond_per_tx_limit() {
    let f = F::new();
    f.armed();
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(5_000 * USD + 1)),
        Error::PerTxLimitExceeded
    );
}

#[test]
fn draw_after_expiry() {
    let f = F::new();
    f.armed();
    f.warp_fresh(30 * 86_400, P60K);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::GrantExpired
    );
}

#[test]
fn draw_with_no_grant() {
    let f = F::new();
    f.funded();
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::GrantNotFound
    );
}

/// A wrong operator is `GrantNotFound`, not `Unauthorized`: from the caller's point of view
/// there is no grant addressed to them, and saying so leaks nothing about who else holds one.
#[test]
fn draw_by_the_wrong_operator() {
    let f = F::new();
    f.armed();
    assert_err!(
        f.v().try_draw(&f.owner, &f.third, &id32(&f.e, 1), &1),
        Error::GrantNotFound
    );
}

#[test]
fn draw_with_a_frozen_grant() {
    let f = F::new();
    f.armed();
    f.freeze(&f.owner, &f.lender);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::GrantFrozen
    );
    f.unfreeze(&f.owner);
    f.draw(1, 1_000 * USD);
    f.check_books();
}

#[test]
fn draw_while_paused() {
    let f = F::new();
    f.armed();
    f.v().pause();
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::Paused
    );
}

#[test]
fn draw_rejects_non_positive() {
    let f = F::new();
    f.armed();
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &0),
        Error::InvalidAmount
    );
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &-1),
        Error::InvalidAmount
    );
}

/// One network timeout must not create the debt twice. The guard lives here rather than in the
/// backend's database because a `UNIQUE` constraint cannot see a transaction whose outcome the
/// backend never learned.
#[test]
fn duplicate_draw_id_changes_nothing() {
    let f = F::new();
    f.armed();
    f.draw(1, 1_000 * USD);

    let pos = f.raw_position();
    let res = f.raw_reserve();
    let bal = f.debt_t().balance(&f.settlement);

    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(1_000 * USD)),
        Error::DuplicateDrawId
    );
    // ...even for a different amount: the id is the key, not the payload.
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(2_000 * USD)),
        Error::DuplicateDrawId
    );

    assert_eq!(f.raw_position(), pos);
    assert_eq!(f.raw_reserve(), res);
    assert_eq!(f.debt_t().balance(&f.settlement), bal);

    // A distinct id applies normally.
    f.draw(2, 1_000 * USD);
    assert_eq!(f.raw_position().principal, 2_000 * USD);
    f.check_books();
}

/// The grant's own ceiling, tighter than the protocol's.
#[test]
fn draw_beyond_the_grants_ltv_ceiling() {
    let f = F::new();
    f.funded();
    // eligible = $52 800; a 1000 bps grant ceiling allows $5 280.
    f.grant(20_000 * USD, 20_000 * USD, 1000);

    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(5_281 * USD)),
        Error::LtvCeilingExceeded
    );
    f.draw(1, 5_280 * USD);
    f.check_books();
}

/// The protocol ceiling is a **defensive backstop**, not an ordinary outcome.
///
/// `grant_authority` rejects a grant ceiling above the protocol's with code 46, so
/// `grant.max_ltv <= cfg.max_ltv` always holds — and a draw breaching the protocol ceiling has
/// necessarily breached the grant's first. Code 35 therefore always wins in `draw`; 11 there
/// would mean an accounting bug. `withdraw` is where 11 is reachable, and it is tested there.
#[test]
fn protocol_ceiling_breach_reports_the_grants_code() {
    let f = F::new();
    f.funded();
    f.grant(100_000 * USD, 100_000 * USD, 5000); // equal to the protocol ceiling

    // $26 400 is the protocol maximum against $52 800 of eligible value.
    f.draw(1, 26_400 * USD);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &1),
        Error::LtvCeilingExceeded
    );
    f.check_books();
}

/// The row that makes the rest of this file a specification rather than a set of observations.
///
/// The grant is frozen **and** expired **and** over cap **and** over per-tx, all at once. The
/// contract must return the first gate in the fixed order every time, not whichever check
/// happens to run first after a refactor.
#[test]
fn first_gate_wins_deterministically() {
    let f = F::new();
    f.armed();
    f.freeze(&f.owner, &f.owner);
    f.warp_fresh(30 * 86_400, P60K); // past expiry
    let huge = 999_999 * USD; // over cap AND over per-tx

    // 1 · operator mismatch outranks everything.
    assert_err!(
        f.v().try_draw(&f.owner, &f.third, &id32(&f.e, 1), &huge),
        Error::GrantNotFound
    );
    // 2 · frozen outranks expired, cap and per-tx.
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &huge),
        Error::GrantFrozen
    );
    // 3 · with the freeze lifted, expired outranks cap and per-tx.
    f.unfreeze(&f.owner);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &huge),
        Error::GrantExpired
    );
    // 4 · within the term, cap outranks per-tx.
    let g = F::new();
    g.armed();
    assert_err!(
        g.v().try_draw(&g.owner, &g.operator, &id32(&g.e, 1), &huge),
        Error::CapExceeded
    );
    // 5 · under cap, per-tx fires.
    assert_err!(
        g.v()
            .try_draw(&g.owner, &g.operator, &id32(&g.e, 1), &(6_000 * USD)),
        Error::PerTxLimitExceeded
    );
    // 6 · pause outranks every grant gate.
    g.v().pause();
    assert_err!(
        g.v().try_draw(&g.owner, &g.third, &id32(&g.e, 1), &huge),
        Error::Paused
    );
    // 7 · and a non-positive amount outranks even the pause.
    assert_err!(
        g.v().try_draw(&g.owner, &g.third, &id32(&g.e, 1), &0),
        Error::InvalidAmount
    );
}

/// The replay guard is checked **before** the price, because a Temporary lookup is far cheaper
/// than a cross-contract call and a duplicate is not worth paying oracle gas to discover.
#[test]
fn duplicate_id_is_reported_even_with_a_dead_oracle() {
    let f = F::new();
    f.armed();
    f.draw(1, 1_000 * USD);
    f.kill_oracle();
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(1_000 * USD)),
        Error::DuplicateDrawId
    );
}

// ---------------------------------------------------------------------------
// quote_draw
// ---------------------------------------------------------------------------

/// A caller must be able to learn *why* a draw would fail without paying for a failing
/// transaction, and must be told **which ceiling** is biting — the remedies are opposite,
/// repay versus deposit more.
#[test]
fn quote_draw_reports_the_binding_ceiling_and_the_code() {
    let f = F::new();
    f.armed(); // cap $20 000; LTV allows $26 400

    let q = f.v().quote_draw(&f.owner, &f.operator, &(1_000 * USD));
    assert_eq!(q.allowed, true);
    assert_eq!(q.blocked_reason, None);
    assert_eq!(q.cap_headroom, 20_000 * USD);
    assert_eq!(q.ltv_headroom, 26_400 * USD);
    assert_eq!(q.binding, crate::Binding::Cap); // the static ceiling is the tighter one
    assert_eq!(q.price_age, 0);

    // Over cap: the quote names the exact code a real call would hit.
    let q = f.v().quote_draw(&f.owner, &f.operator, &(21_000 * USD));
    assert_eq!(q.allowed, false);
    assert_eq!(q.blocked_reason, Some(Error::CapExceeded as u32));

    // A price crash flips which ceiling binds.
    f.set_price(P16K); // eligible $14 080 → LTV allows $7 040
    let q = f.v().quote_draw(&f.owner, &f.operator, &(1_000 * USD));
    assert_eq!(q.ltv_headroom, 7_040 * USD);
    assert_eq!(q.binding, crate::Binding::Ltv);
    assert_eq!(q.allowed, true);
}

#[test]
fn quote_draw_with_a_stale_price_reports_it_rather_than_trapping() {
    let f = F::new();
    f.armed();
    f.warp(301); // price_max_age is 300
    let q = f.v().quote_draw(&f.owner, &f.operator, &(1_000 * USD));
    assert_eq!(q.allowed, false);
    assert_eq!(q.blocked_reason, Some(Error::PriceStale as u32));
    assert_eq!(q.ltv_headroom, 0);
    assert_eq!(q.price_age, 301);
}
