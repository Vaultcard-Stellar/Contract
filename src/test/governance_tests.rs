//! The brake, and the absence of everything else.
//!
//! There is no governance in a market vault. `Config` is frozen at construction — every field,
//! `oracle` included — so `propose_config`, `accept_config`, `propose_oracle` and `accept_oracle`
//! do not exist, and codes 43, 44 and 45 are permanently vacant.
//!
//! That is not a simplification for its own sake. A mutable parameter needs somebody to sign for
//! the positions it affects, and in a market there are thousands of them; the only alternative is
//! a privileged address that can move a threshold under other people's collateral. Freezing
//! everything removes the role rather than trying to constrain it.
//!
//! What is left is the emergency brake, which the lender alone can pull and which **expires by
//! itself**. Half this file exists to pin that bound, because an unbounded lender-only pause
//! would be the very privilege the rest of the design refuses to grant.

use super::harness::*;
use crate::Error;
use soroban_sdk::IntoVal;

fn borrowed() -> F {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f
}

// ---------------------------------------------------------------------------
// What pause blocks
// ---------------------------------------------------------------------------

#[test]
fn pause_blocks_only_risk_increasing_actions() {
    let f = borrowed();
    f.v().pause();

    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::Paused);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 9), &1),
        Error::Paused
    );
    assert_err!(
        f.v().try_unfreeze_grant(&f.owner, &f.operator),
        Error::Paused
    );
    let r = f.v().try_grant_authority(
        &f.owner_b,
        &f.operator,
        &f.settlement_b,
        &USD,
        &USD,
        &(f.now() + 86_400),
        &5000,
    );
    assert_err!(r, Error::Paused);

    // ...and nothing else.
    f.v().deposit(&f.owner, &BTC);
    f.repay_as(&f.owner, &f.owner, &(1_000 * USD));
    f.v().accrue(&f.owner);
    f.freeze(&f.owner, &f.owner);
    f.v().withdraw_reserve(&(1_000 * USD));
    f.revoke(&f.owner);
    f.check_books();
}

/// A pause is market-wide. It has to be — the incident it exists for is a bug in shared code, not
/// in one customer's arithmetic.
#[test]
fn pause_applies_to_every_customer() {
    let f = F::new();
    f.funded();
    f.arm_for(&f.owner_b, BTC, 20_000 * USD, 50, 1_000 * USD);

    f.v().pause();
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::Paused);
    assert_err!(f.v().try_withdraw(&f.owner_b, &1), Error::Paused);
    assert_err!(
        f.v().try_draw(&f.owner_b, &f.operator, &id32(&f.e, 51), &1),
        Error::Paused
    );
    f.check_books();
}

/// The brake has to work when everything else is failing, so it reads no price, asserts no
/// invariants, and makes no external call.
#[test]
fn pause_works_with_a_dead_oracle() {
    let f = borrowed();
    f.kill_oracle();
    f.v().pause();
    assert_eq!(f.v().is_paused(), true);
}

// ---------------------------------------------------------------------------
// Who may pull it, and for how long
// ---------------------------------------------------------------------------

/// Lender only. In a single-customer vault the brake was safe because it was asymmetric — either
/// party could pull it, both had to agree to release it. A market vault has no such pair, so the
/// customer side of that asymmetry is gone and the bound below replaces it.
#[test]
fn only_the_lender_can_pause() {
    use soroban_sdk::testutils::{MockAuth, MockAuthInvoke};

    let f = F::new();
    f.e.mock_auths(&[MockAuth {
        address: &f.owner,
        invoke: &MockAuthInvoke {
            contract: &f.id,
            fn_name: "pause",
            args: ().into_val(&f.e),
            sub_invokes: &[],
        },
    }]);
    // A customer's signature is present, and it is not the one required.
    assert!(f.v().try_pause().is_err());
    assert_eq!(f.v().is_paused(), false);
}

/// A pause nobody renews ends by itself.
///
/// This bounds **inattention, not the lender** — `pausing_again_restarts_the_clock` below is the
/// other half of the truth, and the two together are what the risk register records. What this
/// one guarantees is that no transaction and no counterparty is needed for a forgotten pause to
/// stop applying.
#[test]
fn a_pause_expires_by_itself_after_seven_days() {
    let f = borrowed();
    f.v().pause();
    assert_eq!(f.v().is_paused(), true);
    assert_eq!(f.v().paused_at(), Some(f.now()));

    // One second before the bound: still in force.
    f.warp_fresh(crate::MAX_PAUSE_SECONDS - 1, P60K);
    assert_eq!(f.v().is_paused(), true);
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::Paused);

    // And then it lapses, with nobody calling anything.
    f.warp_fresh(1, P60K);
    assert_eq!(f.v().is_paused(), false);
    f.v().withdraw(&f.owner, &1);
    f.check_books();
}

/// Pausing again during a live pause restarts the clock. Deliberate: an incident that is still
/// live deserves an explicit renewal, on the record, rather than silent continuation.
#[test]
fn pausing_again_restarts_the_clock() {
    let f = borrowed();
    f.v().pause();
    f.warp_fresh(crate::MAX_PAUSE_SECONDS - 100, P60K);
    assert_eq!(f.v().is_paused(), true);

    f.v().pause();
    f.warp_fresh(200, P60K);
    // Past the original bound, and still paused because it was renewed.
    assert_eq!(f.v().is_paused(), true);

    f.warp_fresh(crate::MAX_PAUSE_SECONDS, P60K);
    assert_eq!(f.v().is_paused(), false);
}

/// The lender can always shorten a pause. They can never extend one past its bound.
#[test]
fn unpause_lifts_it_early() {
    let f = borrowed();
    f.v().pause();
    f.warp_fresh(3_600, P60K);
    f.v().unpause();
    assert_eq!(f.v().is_paused(), false);
    assert_eq!(f.v().paused_at(), None);
    f.v().withdraw(&f.owner, &1);
    f.check_books();
}
