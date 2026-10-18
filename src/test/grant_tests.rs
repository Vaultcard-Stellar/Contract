//! Grant lifecycle: issue, freeze, revoke, reap.
//!
//! Revocation is the fire alarm. It reads no price, is not blocked by pause, and has no failure
//! condition beyond "no grant" and "not the owner" — a fire alarm a dead oracle can disable is
//! not a fire alarm. Several tests here exist only to prove that.

use super::harness::*;
use crate::Error;

#[test]
fn grant_commits_capital_and_records_the_terms() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 5_000 * USD, 4000);

    let g = f.grant_of(&f.owner).unwrap();
    assert_eq!(g.operator, f.operator);
    assert_eq!(g.settlement, f.settlement);
    assert_eq!(g.cap, 20_000 * USD);
    assert_eq!(g.drawn, 0);
    assert_eq!(g.per_tx_limit, 5_000 * USD);
    assert_eq!(g.max_ltv_bps, 4000);
    assert_eq!(g.frozen, false);

    let r = f.v().get_reserve();
    assert_eq!(r.committed, 20_000 * USD);
    assert_eq!(r.free, 80_000 * USD);
    f.check_books();
}

/// `Grant.drawn` lives inside the struct, so overwriting resets it to zero:
///
/// ```text
/// cap 50 000, drawn 45 000  →  grant again with the same terms  →  drawn 0
/// total principal creatable: 95 000 against a 50 000 cap
/// ```
///
/// Changing terms therefore takes two transactions. Slower, but there is no path by which
/// `drawn` disappears.
#[test]
fn grant_while_one_is_live() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);

    let r = f.v().try_grant_authority(
        &f.owner,
        &f.operator,
        &f.settlement,
        &(20_000 * USD),
        &(5_000 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    assert_err!(r, Error::GrantExists);
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 5_000 * USD);
    f.check_books();
}

#[test]
fn settlement_equal_to_operator_is_rejected() {
    let f = F::new();
    f.funded();
    // Otherwise a compromised operator simply pays itself, and the entire pinned-settlement
    // guarantee is void.
    let r = f.v().try_grant_authority(
        &f.owner,
        &f.operator,
        &f.operator,
        &(1_000 * USD),
        &(1_000 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    assert_err!(r, Error::InvalidGrantParams);
}

#[test]
fn expiry_bounds() {
    let f = F::new();
    f.funded();
    let mk = |expiry: u64| {
        f.v().try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &(1_000 * USD),
            &(1_000 * USD),
            &expiry,
            &5000,
        )
    };
    // In the past.
    assert_err!(mk(f.now() - 1), Error::InvalidGrantParams);
    assert_err!(mk(f.now()), Error::InvalidGrantParams);
    // Beyond MAX_GRANT_SECONDS: a grant that outlives the TTL of its own storage entry could be
    // archived while live, leaving `committed` locked forever.
    assert_err!(
        mk(f.now() + crate::MAX_GRANT_SECONDS + 1),
        Error::InvalidGrantParams
    );
    // Exactly at the bound is fine.
    assert!(mk(f.now() + crate::MAX_GRANT_SECONDS).is_ok());
}

#[test]
fn per_tx_limit_above_cap_is_rejected() {
    let f = F::new();
    f.funded();
    let r = f.v().try_grant_authority(
        &f.owner,
        &f.operator,
        &f.settlement,
        &(1_000 * USD),
        &(1_001 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    assert_err!(r, Error::InvalidGrantParams);
}

/// Rule R2: capital is reserved at grant time, which is what makes the promise *"never declined
/// because someone else borrowed first"* true. A grant beyond `reserve.free` would be a promise
/// with nothing behind it.
#[test]
fn grant_beyond_free_reserve() {
    let f = F::new();
    f.funded(); // $100 000
                // The **proposal** is accepted, because it reserves nothing. R2 is a condition on committing
                // capital, and a proposal commits none — refusing here would turn a thin reserve at this
                // instant into a permanent no for a line the operator might take up tomorrow.
    f.propose(100_001 * USD, 1_000 * USD, 5000);
    // Acceptance is where it bites, because that is where the capital moves.
    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.operator),
        Error::ReserveUnderfunded
    );
    f.v().revoke_authority(&f.owner, &f.operator);

    // Exactly the free balance is fine.
    f.grant(100_000 * USD, 1_000 * USD, 5000);
    assert_eq!(f.v().get_reserve().free, 0);
    f.check_books();
}

/// Distinct from 35: that is a *draw* exceeding terms the owner set; this is the owner proposing
/// terms above what the protocol allows. Opposite comparison, different actor — and the
/// backend's documented response to 35 is nonsense for this case.
#[test]
fn grant_ceiling_above_the_protocol_ceiling() {
    let f = F::new();
    f.funded();
    let r = f.v().try_grant_authority(
        &f.owner,
        &f.operator,
        &f.settlement,
        &(1_000 * USD),
        &(1_000 * USD),
        &(f.now() + 86_400),
        &5001,
    );
    assert_err!(r, Error::GrantCeilingTooHigh);
}

#[test]
fn grant_beyond_the_vault_cap() {
    let f = F::with(|mut c| {
        c.vault_cap = 5_000 * USD;
        c
    });
    f.funded();
    // Same as R2: the exposure ceiling is a condition on committed capital, so it is checked
    // when the operator takes the line up rather than when the customer offers it.
    f.propose(5_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.operator),
        Error::ExposureCapExceeded
    );
}

// ---------------------------------------------------------------------------
// Revoke and reap
// ---------------------------------------------------------------------------

#[test]
fn revoke_releases_the_undrawn_commitment_exactly_once() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);
    assert_eq!(f.v().get_reserve().committed, 15_000 * USD);

    f.revoke(&f.owner);
    assert_eq!(f.grant_of(&f.owner), None);
    let r = f.v().get_reserve();
    assert_eq!(r.committed, 0);
    assert_eq!(r.outstanding, 5_000 * USD);
    assert_eq!(r.free, 95_000 * USD);

    // Property P6. The second call finds nothing and releases nothing — which is why the entry
    // is deleted rather than flagged.
    assert_err!(
        f.v().try_revoke_authority(&f.owner, &f.operator),
        Error::GrantNotFound
    );
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

#[test]
fn revoke_works_with_a_dead_oracle_and_while_paused() {
    let f = F::new();
    f.armed();
    f.kill_oracle();
    f.v().pause();
    f.revoke(&f.owner);
    assert_eq!(f.grant_of(&f.owner), None);
    assert_eq!(f.v().get_reserve().committed, 0);
}

#[test]
fn draw_after_revoke_is_grant_not_found() {
    let f = F::new();
    f.armed();
    f.revoke(&f.owner);
    // Code 32 was retired for exactly this reason: revocation deletes the entry, so there is no
    // "revoked" state to report. Never reuse 32.
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::GrantNotFound
    );
}

/// Nothing runs when time passes, and only the owner may revoke. If the owner disappears, an
/// expired grant would lock the lender's capital forever — so reaping is permissionless. Safe,
/// because the precondition is objective: anyone checking gets the same answer.
#[test]
fn reap_expired() {
    let f = F::new();
    f.armed();
    assert_err!(
        f.v().try_reap_expired(&f.owner, &f.operator),
        Error::GrantNotExpired
    );

    f.warp_fresh(30 * 86_400, P60K);
    f.reap(&f.owner);
    assert_eq!(f.grant_of(&f.owner), None);
    assert_eq!(f.v().get_reserve().committed, 0);
    assert_eq!(f.v().get_reserve().free, 100_000 * USD);

    assert_err!(
        f.v().try_reap_expired(&f.owner, &f.operator),
        Error::GrantNotFound
    );
    f.check_books();
}

#[test]
fn revoke_then_reap_releases_exactly_once() {
    let f = F::new();
    f.armed();
    f.revoke(&f.owner);
    assert_eq!(f.v().get_reserve().committed, 0);
    f.warp_fresh(30 * 86_400, P60K);
    assert_err!(
        f.v().try_reap_expired(&f.owner, &f.operator),
        Error::GrantNotFound
    );
    assert_eq!(f.v().get_reserve().committed, 0);
    f.check_books();
}

// ---------------------------------------------------------------------------
// Freeze
// ---------------------------------------------------------------------------

/// The lender may freeze as well as the owner: they bear the risk too and may spot an anomaly
/// first. Revoke is the blunt alternative — it destroys `cap`, `drawn` and `settlement` and
/// forces the owner to re-sign from scratch, and a user made to do that over false alarms stops
/// responding to alarms.
#[test]
fn freeze_by_either_party_unfreeze_by_the_owner() {
    let f = F::new();
    f.armed();
    f.freeze(&f.owner, &f.lender);
    assert_eq!(f.grant_of(&f.owner).unwrap().frozen, true);
    f.unfreeze(&f.owner);
    assert_eq!(f.grant_of(&f.owner).unwrap().frozen, false);
    f.freeze(&f.owner, &f.owner);
    assert_eq!(f.grant_of(&f.owner).unwrap().frozen, true);
}

#[test]
fn freeze_by_a_third_party() {
    let f = F::new();
    f.armed();
    assert_err!(
        f.v().try_freeze_grant(&f.owner, &f.operator, &f.third),
        Error::Unauthorized
    );
    assert_eq!(f.grant_of(&f.owner).unwrap().frozen, false);
}

#[test]
fn freeze_with_no_grant() {
    let f = F::new();
    f.funded();
    assert_err!(
        f.v().try_freeze_grant(&f.owner, &f.operator, &f.owner),
        Error::GrantNotFound
    );
    assert_err!(
        f.v().try_unfreeze_grant(&f.owner, &f.operator),
        Error::GrantNotFound
    );
}

/// Freezing is risk-reducing, so a dead oracle cannot block it. Unfreezing reopens the ability
/// to borrow, so it is blocked by pause — but it still reads no price, because the next `draw`
/// checks LTV against a fresh one anyway.
#[test]
fn freeze_ignores_the_oracle_unfreeze_respects_the_pause() {
    let f = F::new();
    f.armed();
    f.kill_oracle();
    f.freeze(&f.owner, &f.owner);

    f.v().pause();
    assert_err!(
        f.v().try_unfreeze_grant(&f.owner, &f.operator),
        Error::Paused
    );

    f.v().unpause();
    f.unfreeze(&f.owner); // still no price available, and still fine
    assert_eq!(f.grant_of(&f.owner).unwrap().frozen, false);
}
