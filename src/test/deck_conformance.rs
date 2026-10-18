//! One test per case the deck names, addressed by the deck's own vocabulary.
//!
//! `resources/specs/vault-card-technical-deck.html` is the only authoritative specification. The
//! rest of the suite is organised by contract module, which is the right shape for finding a bug
//! and the wrong shape for answering *"does this meet the spec?"*. This file exists to be read
//! side by side with the deck: every test is named `slideNN_…` after the slide that demands it,
//! so a reviewer can walk the deck top to bottom and tick lines off.
//!
//! Where an equivalent assertion already exists elsewhere, the test is still written here under
//! its deck name and its doc comment points at the sibling rather than duplicating the
//! arithmetic. Finding the proof is the job this file does.
//!
//! **Deliberate deviations are tested as deviations**, not skipped — see
//! `slide05_rules_r1_and_r3_are_enforced_at_draw_not_at_grant` and
//! `slide10_seize_above_collateral_does_not_revert`. Their reasons are recorded in
//! `docs/13-deck-conformance.md`.

use super::harness::*;
use crate::{Band, Binding, DataKey, Error};
use soroban_sdk::testutils::{Address as _, Events, Ledger};
use soroban_sdk::{
    contract, contractevent, contractimpl, Address, Env, MuxedAddress, Symbol, TryFromVal,
};

/// $65 000, $0.115 and $1.00 at 14 decimals — the prototype's price table on slide 06.
const P65K: i128 = 65_000 * 100_000_000_000_000;
const P_XLM: i128 = 11_500_000_000_000; // 0.115 × 10^14
const P_USDC: i128 = 100_000_000_000_000; // 1.00 × 10^14

// ===========================================================================
// Slide 02 · System overview
// ===========================================================================

/// > *"Một hàm duy nhất tạo dư nợ."* — exactly one function creates debt.
///
/// Every other entry point is called against a live, funded, granted vault and `principal` must
/// not move. The assertion is on the **negative**: it is not that `draw` works, it is that
/// nothing else does.
#[test]
fn slide02_only_draw_creates_debt() {
    let f = F::new();
    f.armed();
    assert_eq!(f.raw_position().principal, 0);

    let _ = f.v().try_deposit(&f.owner, &BTC);
    let _ = f.v().try_withdraw(&f.owner, &1);
    let _ = f
        .v()
        .try_repay(&f.owner, &f.third, &1, &Some(f.operator.clone()));
    let _ = f.v().try_accrue(&f.owner);
    let _ = f.v().try_fund_reserve(&(1_000 * USD));
    let _ = f.v().try_withdraw_reserve(&(1_000 * USD));
    let _ = f.v().try_freeze_grant(&f.owner, &f.operator, &f.owner);
    let _ = f.v().try_unfreeze_grant(&f.owner, &f.operator);
    let _ = f.v().try_reap_expired(&f.owner, &f.operator);
    let _ = f
        .v()
        .try_liquidate(&f.owner, &f.liquidator, &(1_000 * USD), &0);
    let _ = f.v().try_lender_liquidate(&f.owner, &(1_000 * USD));
    let _ = f.v().try_pause();
    let _ = f.v().try_unpause();
    let _ = f.v().try_report_oracle_state();

    assert_eq!(
        f.raw_position().principal,
        0,
        "something other than draw created debt"
    );
    assert_eq!(f.raw_reserve().outstanding, 0);

    f.draw(1, 1_000 * USD);
    assert_eq!(f.raw_position().principal, 1_000 * USD);
    f.check_books();
}

/// > *"Địa chỉ nhận ghim cứng."* — the recipient address is hard-pinned.
///
/// Slide 13 makes the same point structurally: `draw` takes **no destination parameter** and
/// reads `grant.settlement` from storage, so a compromised operator can only move money to the
/// address the owner signed for. That is a property of the signature, which no runtime test can
/// observe directly — what a test *can* prove is that the funds land at `settlement` and that
/// the operator ends up with nothing.
#[test]
fn slide02_settlement_address_is_pinned() {
    let f = F::new();
    f.armed();
    f.draw(1, 3_000 * USD);

    assert_eq!(f.debt_t().balance(&f.settlement), 3_000 * USD);
    assert_eq!(f.debt_t().balance(&f.operator), 0);
    assert_eq!(f.debt_t().balance(&f.owner), 10_000_000 * USD);

    // Changing where the money goes takes revoke + re-grant. There is no setter.
    f.revoke(&f.owner);
    f.v().grant_authority(
        &f.owner,
        &f.operator,
        &f.third,
        &(5_000 * USD),
        &(5_000 * USD),
        &(f.now() + 86_400),
        &5000,
    );
    // And the operator has to take the new one up. Re-pointing settlement is exactly the change
    // an operator must get a say in: the whole guarantee is that draws land where they can
    // collect, and this call is where they check that.
    f.v().accept_grant(&f.owner, &f.operator);
    f.draw(2, 1_000 * USD);
    assert_eq!(f.debt_t().balance(&f.settlement), 3_000 * USD); // unchanged
    assert_eq!(f.debt_t().balance(&f.third), 10_000_000 * USD + 1_000 * USD);
    f.check_books();
}

/// > *"Bốn ràng buộc chain cưỡng chế."* — four constraints enforced on chain.
///
/// Slide 03 names the codes the swipe path surfaces: cap, per-transaction limit, the grant's LTV
/// ceiling, and expiry. All four are contract gates, not backend policy.
#[test]
fn slide02_four_constraints_enforced_on_chain() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 5_000 * USD, 4000);

    // 1 · per-transaction limit
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(5_000 * USD + 1)),
        Error::PerTxLimitExceeded
    );
    // 2 · the grant's own LTV ceiling — 4000 bps of $52 800 is $21 120, but per-tx caps each
    //     call at $5 000, so fill the line first.
    for i in 1..=4u8 {
        f.draw(i, 5_000 * USD);
    }
    // 3 · cumulative cap
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &1),
        Error::CapExceeded
    );
    // 4 · expiry
    f.warp_fresh(30 * 86_400, P60K);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 6), &1),
        Error::GrantExpired
    );
    f.check_books();
}

// ===========================================================================
// Slide 03 · End-to-end flow
// ===========================================================================

/// The six phases, in one transaction sequence: lock collateral → grant → swipe → monitor →
/// liquidate/repay → withdraw.
#[test]
fn slide03_six_phase_lifecycle() {
    let f = F::new();

    // GĐ 2 · khoá collateral
    f.v().fund_reserve(&(100_000 * USD));
    f.v().deposit(&f.owner, &BTC);
    assert_eq!(f.v().get_position(&f.owner).market_value, 600_000_000_000);

    // GĐ 3 · cấp hạn mức — reserve check first: uncommitted capital ≥ cap
    f.grant(20_000 * USD, 5_000 * USD, 5000);
    assert_eq!(f.v().get_reserve().committed, 20_000 * USD);
    assert_eq!(f.v().get_reserve().free, 80_000 * USD);

    // GĐ 4 · quẹt thẻ
    f.draw(1, 5_000 * USD);
    assert_eq!(f.debt_t().balance(&f.settlement), 5_000 * USD);

    // GĐ 5 · giám sát LTV — the price moves, the band moves with it
    assert_eq!(f.v().get_position(&f.owner).band, Band::Healthy);
    f.set_price(P16K);
    assert_eq!(f.v().get_position(&f.owner).band, Band::Healthy); // $5 000 against $14 080
    f.set_price(6_000 * 100_000_000_000_000);
    assert_eq!(f.v().get_position(&f.owner).band, Band::Liquidatable);

    // GĐ 6 · người dùng khắc phục, rồi rút
    f.set_price(P60K);
    f.v().accrue(&f.owner);
    f.repay_as(&f.owner, &f.owner, &f.v().get_position(&f.owner).debt);
    f.revoke(&f.owner);
    f.v().withdraw(&f.owner, &BTC);

    assert_eq!(f.raw_position().deposit, 0);
    assert_eq!(f.raw_position().principal, 0);
    assert_eq!(f.dep_t().balance(&f.owner), f.minted_dep());
    f.check_books();
}

/// > *"Từ chối kèm mã 33 · 34 · 35 · 12"* — the four codes the swipe path surfaces.
#[test]
fn slide03_decline_codes_at_swipe_are_33_34_35_12() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 5_000 * USD, 1000); // ceiling 1000 bps allows $5 280

    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &0),
        Error::InvalidAmount // 12
    );
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(5_000 * USD + 1)),
        Error::PerTxLimitExceeded // 34
    );
    f.draw(3, 5_000 * USD);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 4), &(300 * USD)),
        Error::LtvCeilingExceeded // 35 — $5 300 exceeds the grant's $5 280
    );
    // Fill the line to reach the cumulative cap.
    f.revoke(&f.owner);
    f.grant(6_000 * USD, 6_000 * USD, 5000);
    f.draw(5, 1_000 * USD); // drawn under this grant: $1 000
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 6), &(5_001 * USD)),
        Error::CapExceeded // 33
    );
    f.check_books();
}

/// > *"Nạp thêm collateral, hoàn trả và thu hồi uỷ quyền không bao giờ bị chặn."*
///
/// The degradation rule, verbatim: deposit, repay and revoke are never blocked. Tested here
/// under the worst combination the deck contemplates — dead oracle **and** paused. The wider
/// cross-product is `property_tests::p3_risk_reducing_actions_survive_every_hostile_condition`.
#[test]
fn slide03_degradation_rule_deposit_repay_revoke_never_blocked() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);

    f.v().pause();
    f.kill_oracle();

    f.v().deposit(&f.owner, &BTC);
    f.repay_as(&f.owner, &f.third, &(1_000 * USD));
    f.revoke(&f.owner);

    assert_eq!(f.raw_position().deposit, 2 * BTC);
    assert_eq!(f.raw_position().principal, 4_000 * USD);
    assert_eq!(f.grant_of(&f.owner), None);
    f.check_books();
}

// ===========================================================================
// Slide 04 · Credit engine — the formulas
// ===========================================================================

/// `market_value = qty × price / 10^dec` and `eligible_value = market × (BPS − haircut) / BPS`.
///
/// One BTC at $60 000 with a 12 % haircut. The full rounding matrix is
/// `math_tests::rounding_directions`.
#[test]
fn slide04_market_and_eligible_value() {
    let f = F::new();
    f.v().deposit(&f.owner, &BTC);
    let p = f.v().get_position(&f.owner);
    assert_eq!(p.market_value, 600_000_000_000); // $60 000.0000000
    assert_eq!(p.eligible_value, 528_000_000_000); // × 0.88
}

/// `ltv_bps = debt_value × BPS / eligible_value`, and `health`.
///
/// The deck writes `health = eligible × liq_thr / (debt × BPS)` and annotates it *"≥ 1.0 là an
/// toàn"* — a ratio around one. As **integer** arithmetic that expression truncates to 0 or 1 and
/// carries no information, so the contract reports the same quantity scaled by `BPS`:
/// `health_bps`, where `10_000` is exactly the liquidation boundary. Same number, usable units.
#[test]
fn slide04_ltv_and_health_are_the_deck_ratios() {
    let f = F::new();
    f.funded();
    f.grant(30_000 * USD, 30_000 * USD, 5000);
    f.draw(1, 26_400 * USD); // exactly 50 % of $52 800

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.ltv_bps, 5_000);
    // eligible × liq_thr / debt = 528e9 × 7000 / 264e9 = 14 000 bps = 1.4 in the deck's units.
    assert_eq!(p.health_bps, 14_000);

    // And 10 000 is the boundary, not 1 — the whole reason for the rescale.
    f.set_price(42_858 * 100_000_000_000_000);
    assert!(f.v().get_position(&f.owner).health_bps >= 10_000);
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, false);
    f.check_books();
}

/// `liq_price = debt × BPS / (qty × liq_thr × (BPS − haircut) / BPS)`, reported in **oracle price
/// scale** so it can be compared with what the feed publishes.
#[test]
fn slide04_liq_price() {
    let f = F::new();
    f.funded();
    f.grant(30_000 * USD, 30_000 * USD, 5000);
    f.draw(1, 26_400 * USD);

    // $42 857.142857… per BTC, rounded up at every step so the warning comes early.
    assert_eq!(
        f.v().get_position(&f.owner).liq_price,
        4_285_714_285_714_285_715
    );

    // At that price the position is at the threshold; the reported number is not decorative.
    f.set_price(4_285_714_285_714_285_715);
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, true);
}

/// > `credit_limit = min(eligible × max_ltv / BPS, reserve.free, program_cap, per_asset_cap)`
///
/// Three tiers, three set-ups, each with a different one binding. Tier 3 is `vault_cap` — with
/// one vault per customer there is no distinction between a per-asset cap and a programme cap at
/// this level, so the deck's two names collapse into one field.
#[test]
fn slide04_credit_limit_is_the_min_of_three_tiers() {
    // Tier 1 — the price binds.
    let f = F::new();
    f.funded(); // $100 000 of reserve, 1 BTC
    f.grant(100_000 * USD, 100_000 * USD, 5000);
    let q = f.v().quote_draw(&f.owner, &f.operator, &0);
    assert_eq!(q.ltv_headroom, 26_400 * USD); // eligible $52 800 × 50 %
    assert_eq!(q.cap_headroom, 100_000 * USD);
    assert_eq!(q.binding, Binding::Ltv);

    // Tier 2 — uncommitted capital binds. R2, error 20.
    let g = F::new();
    g.v().fund_reserve(&(10_000 * USD));
    g.v().deposit(&g.owner, &BTC);
    g.propose(10_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        g.v().try_accept_grant(&g.owner, &g.operator),
        Error::ReserveUnderfunded
    );
    g.v().revoke_authority(&g.owner, &g.operator);
    g.grant(10_000 * USD, 10_000 * USD, 5000);
    let q = g.v().quote_draw(&g.owner, &g.operator, &0);
    assert_eq!(q.cap_headroom, 10_000 * USD);
    assert_eq!(q.binding, Binding::Cap); // $10 000 < the price-driven $26 400

    // Tier 3 — the programme ceiling binds. R5, error 22.
    let h = F::with(|mut c| {
        c.vault_cap = 5_000 * USD;
        c
    });
    h.funded();
    h.propose(5_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        h.v().try_accept_grant(&h.owner, &h.operator),
        Error::ExposureCapExceeded
    );
    h.v().revoke_authority(&h.owner, &h.operator);
    h.grant(5_000 * USD, 5_000 * USD, 5000);
    assert_eq!(
        h.v().quote_draw(&h.owner, &h.operator, &0).cap_headroom,
        5_000 * USD
    );

    f.check_books();
    g.check_books();
    h.check_books();
}

/// The four risk bands. Boundaries are asserted to the unit in `math_tests::band_thresholds`;
/// this is the deck-facing statement of the same table.
#[test]
fn slide04_four_risk_bands() {
    let f = F::new();
    f.funded();
    f.grant(40_000 * USD, 40_000 * USD, 5000);

    assert_eq!(f.v().get_position(&f.owner).band, Band::Healthy);
    f.draw(1, 23_760 * USD); // 45 % of eligible — exactly where Watch starts
    assert_eq!(f.v().get_position(&f.owner).band, Band::Watch);

    // Warning: past max_ltv. Drop the price instead of drawing more — the gate would block it.
    f.set_price(50_000 * 100_000_000_000_000);
    assert_eq!(f.v().get_position(&f.owner).band, Band::Warning);

    f.set_price(35_000 * 100_000_000_000_000);
    assert_eq!(f.v().get_position(&f.owner).band, Band::Liquidatable);
    assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, true);
    f.check_books();
}

// ===========================================================================
// Slide 05 · Risk parameters per asset, and rules R1–R6
// ===========================================================================

/// USDC row: haircut 0 %, max LTV 85 %, liq threshold 92 %, bonus 3 %, close factor 50 %,
/// price max age 600 s. `full_liq_ltv` is ours; 9500 sits between the threshold and the 9708
/// break-even that a 3 % bonus permits.
#[test]
fn slide05_usdc_profile() {
    let f = F::with_tokens(7, ORACLE_DEC, |mut c| {
        c.haircut_bps = 0;
        c.max_ltv_bps = 8500;
        c.liq_threshold_bps = 9200;
        c.full_liq_ltv_bps = 9500;
        c.liq_bonus_bps = 300;
        c.close_factor_bps = 5000;
        c.price_max_age = 600;
        c
    });
    f.set_price(P_USDC);
    f.v().fund_reserve(&(100_000 * USD));
    f.v().deposit(&f.owner, &(10_000 * USD)); // 10 000 USDC, 7 decimals

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.market_value, 100_000_000_000); // $10 000
    assert_eq!(p.eligible_value, 100_000_000_000); // no haircut

    f.grant(20_000 * USD, 20_000 * USD, 8500);
    assert_eq!(
        f.v().quote_draw(&f.owner, &f.operator, &0).ltv_headroom,
        8_500 * USD
    );
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(8_501 * USD)),
        Error::LtvCeilingExceeded
    );
    f.draw(1, 8_500 * USD);

    // A 10 % fall puts LTV at 9445 bps — past the 9200 threshold, short of the 9500 full close.
    f.set_price(90_000_000_000_000); // $0.90
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.liquidatable, true);
    assert_eq!(q.ltv_bps, 9_445);
    assert_eq!(q.max_repay, 4_250 * USD); // 50 % close factor

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &(4_250 * USD), &0);
    assert_eq!(seized, 48_638_888_888); // $4 250 × 1.03 at $0.90
    f.check_books();
}

/// BTC row: haircut 12 %, max LTV 50 %, liq threshold 70 %, bonus 8 %, close factor 50 %,
/// price max age 300 s. This is the default fixture, asserted explicitly.
#[test]
fn slide05_btc_profile() {
    let f = F::new();
    let c = f.v().get_config();
    assert_eq!(c.haircut_bps, 1200);
    assert_eq!(c.max_ltv_bps, 5000);
    assert_eq!(c.liq_threshold_bps, 7000);
    assert_eq!(c.liq_bonus_bps, 800);
    assert_eq!(c.close_factor_bps, 5000);
    assert_eq!(c.price_max_age, 300);
    assert_eq!(c.deposit_decimals, 8);

    f.funded();
    let p = f.v().get_position(&f.owner);
    assert_eq!(p.market_value, 600_000_000_000);
    assert_eq!(p.eligible_value, 528_000_000_000);
}

/// XLM row: haircut 20 %, max LTV 40 %, liq threshold 60 %, bonus 10 %, close factor 50 %,
/// price max age 300 s. Seven decimals, and a price under one dollar — the combination that
/// makes a decimals mistake visible.
#[test]
fn slide05_xlm_profile() {
    let f = F::with_tokens(7, ORACLE_DEC, |mut c| {
        c.haircut_bps = 2000;
        c.max_ltv_bps = 4000;
        c.liq_threshold_bps = 6000;
        c.full_liq_ltv_bps = 8500;
        c.liq_bonus_bps = 1000;
        c.close_factor_bps = 5000;
        c.price_max_age = 300;
        c
    });
    f.set_price(P_XLM);
    f.v().fund_reserve(&(100_000 * USD));
    f.v().deposit(&f.owner, &(120_000 * USD)); // 120 000 XLM, 7 decimals

    let p = f.v().get_position(&f.owner);
    assert_eq!(p.market_value, 138_000_000_000); // $13 800
    assert_eq!(p.eligible_value, 110_400_000_000); // × 0.80

    f.grant(20_000 * USD, 20_000 * USD, 4000);
    assert_eq!(
        f.v().quote_draw(&f.owner, &f.operator, &0).ltv_headroom,
        4_416 * USD
    );
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(4_417 * USD)),
        Error::LtvCeilingExceeded
    );
    f.draw(1, 4_416 * USD);
    f.check_books();
}

/// > *"XLM có 20 điểm, USDC có 7 điểm."*
///
/// The liquidation buffer: the room a customer has between being unable to borrow and being
/// liquidated.
#[test]
fn slide05_liquidation_buffer_widths() {
    let xlm = F::with(|mut c| {
        c.max_ltv_bps = 4000;
        c.liq_threshold_bps = 6000;
        c.full_liq_ltv_bps = 8500;
        c.liq_bonus_bps = 1000;
        c
    });
    let cx = xlm.v().get_config();
    assert_eq!(cx.liq_threshold_bps - cx.max_ltv_bps, 2000); // 20 points

    let usdc = F::with(|mut c| {
        c.haircut_bps = 0;
        c.max_ltv_bps = 8500;
        c.liq_threshold_bps = 9200;
        c.full_liq_ltv_bps = 9500;
        c.liq_bonus_bps = 300;
        c
    });
    let cu = usdc.v().get_config();
    assert_eq!(cu.liq_threshold_bps - cu.max_ltv_bps, 700); // 7 points

    // The default BTC row sits between them: 20 points.
    let btc = F::new().v().get_config();
    assert_eq!(btc.liq_threshold_bps - btc.max_ltv_bps, 2000);
}

/// R2 → `20 ReserveUnderfunded`, R5 → `22 ExposureCapExceeded`, R4 → `46`.
///
/// **R4 is a deliberate deviation.** The deck assigns it `35 LtvCeilingExceeded`, the same code
/// `draw` uses when an operator borrows past the grant's ceiling. Those are opposite comparisons
/// by opposite actors, and slide 14's runbook for 35 — *"release headroom, the operator absorbs
/// the loss, P2 alert"* — is nonsense when the owner has simply proposed terms above what the
/// protocol allows. Code 46 was added so the backend can tell them apart.
#[test]
fn slide05_rules_r2_r4_r5() {
    let f = F::with(|mut c| {
        c.vault_cap = 50_000 * USD;
        c
    });
    f.v().fund_reserve(&(10_000 * USD));
    f.v().deposit(&f.owner, &BTC);

    let propose = |cap: i128, ceiling: u32| {
        f.v().try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &cap,
            &(1_000 * USD),
            &(f.now() + 86_400),
            &ceiling,
        )
    };
    // Propose, then take it up. R2 and R5 are conditions on **committed capital**, and a proposal
    // commits none, so they moved to `accept_grant` when the operator gained a say. R4 stayed at
    // proposal time: it compares two configured ceilings and depends on nothing shared, so a
    // customer asking for impossible terms learns it without a round trip through the operator.
    let take_up = |cap: i128, ceiling: u32| {
        propose(cap, ceiling).unwrap().unwrap();
        let r = f.v().try_accept_grant(&f.owner, &f.operator);
        if r.is_err() {
            // Clear the proposal so the next case starts from nothing — a second proposal to the
            // same operator is refused with 38 regardless of whether the first was ever accepted.
            f.v().revoke_authority(&f.owner, &f.operator);
        }
        r
    };

    // R4 first, and still at proposal time.
    assert_err!(propose(1_000 * USD, 5001), Error::GrantCeilingTooHigh);
    // R2
    assert_err!(take_up(10_001 * USD, 5000), Error::ReserveUnderfunded);
    // R5 — fund past the vault cap so the reserve rule cannot fire first.
    f.v().fund_reserve(&(90_000 * USD));
    assert_err!(take_up(50_001 * USD, 5000), Error::ExposureCapExceeded);
    // ...and the boundary is allowed.
    assert!(take_up(50_000 * USD, 5000).is_ok());
    f.check_books();
}

/// **R1 and R3 are deliberate deviations**, and this test pins the behaviour that replaces them.
///
/// The deck places both at the moment the limit is granted: R1 rejects a limit above
/// `eligible × max_ltv` with code 11, R3 rejects a stale price with code 40. `grant_authority`
/// reads no price and enforces neither.
///
/// The reason is decision Đ-05: the limit is **dynamic**. `cap` is only a static ceiling; the
/// binding constraint is the LTV gate, which runs at `draw` against a fresh price. A cap above
/// `eligible × max_ltv` is therefore harmless — every draw that would breach the ratio is
/// blocked anyway — while enforcing R1 at grant time would make issuing a credit line impossible
/// during an oracle outage and would forbid signing a grant before the collateral is deposited.
///
/// Both halves are proven here: the over-large grant is accepted and the draw is refused; the
/// grant succeeds with a dead feed and the draw is refused.
#[test]
fn slide05_rules_r1_and_r3_are_enforced_at_draw_not_at_grant() {
    // R1 — a cap far above eligible × max_ltv is accepted...
    let f = F::new();
    f.funded(); // eligible $52 800, so the ratio allows $26 400
    f.grant(90_000 * USD, 90_000 * USD, 5000);
    assert_eq!(f.grant_of(&f.owner).unwrap().cap, 90_000 * USD);
    // ...and the draw that would breach the ratio is refused.
    f.draw(1, 26_400 * USD);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &1),
        Error::LtvCeilingExceeded
    );
    assert_eq!(f.raw_position().principal, 26_400 * USD);

    // R3 — granting works with a dead feed...
    let g = F::new();
    g.funded();
    g.kill_oracle();
    g.grant(20_000 * USD, 20_000 * USD, 5000);
    assert_eq!(g.v().get_reserve().committed, 20_000 * USD);
    // ...and drawing does not.
    assert_err!(
        g.v().try_draw(&g.owner, &g.operator, &id32(&g.e, 1), &1),
        Error::PriceInvalid
    );
    // A stale-but-present price is the other half of R3.
    g.set_price_at(P60K, g.now() - 301);
    assert_err!(
        g.v().try_draw(&g.owner, &g.operator, &id32(&g.e, 2), &1),
        Error::PriceStale
    );

    f.check_books();
    g.check_books();
}

// ===========================================================================
// Slide 06 · Prototype reconciliation
// ===========================================================================
//
// > *"Đối chiếu kết quả unit test với công cụ này."*
//
// The prototype prices a basket: XLM 120 000 @ $0.115, BTC 0.85 @ $65 000, USDC 5 000 @ $1.00,
// against $14 000 of debt. Under Đ-01 one vault holds one asset, so each leg is valued by its
// own vault and the basket is summed here — which is exactly the division of labour the decision
// records: the contract values one asset, the backend's `credit-engine` blends them.

fn leg(dep_dec: u32, haircut: u32, liq: u32, price: i128, qty: i128) -> (i128, i128) {
    let f = F::with_tokens(dep_dec, ORACLE_DEC, move |mut c| {
        c.haircut_bps = haircut;
        c.liq_threshold_bps = liq;
        // Only the haircut and the threshold matter to a valuation. The rest are set to keep the
        // constructor's ordering and break-even constraints satisfied for every row, including
        // USDC's 92 % threshold — a 3 % bonus is the most that leaves room above it.
        c.max_ltv_bps = liq - 1000;
        c.full_liq_ltv_bps = liq + 300;
        c.liq_bonus_bps = 300;
        c
    });
    f.set_price(price);
    f.v().deposit(&f.owner, &qty);
    let p = f.v().get_position(&f.owner);
    (p.market_value, p.eligible_value)
}

#[test]
fn slide06_prototype_btc_leg() {
    let (market, eligible) = leg(8, 1200, 7000, P65K, 85_000_000); // 0.85 BTC
    assert_eq!(market, 552_500_000_000); // $55 250
    assert_eq!(eligible, 486_200_000_000); // $48 620
}

#[test]
fn slide06_prototype_xlm_leg() {
    let (market, eligible) = leg(7, 2000, 6000, P_XLM, 120_000 * USD);
    assert_eq!(market, 138_000_000_000); // $13 800
    assert_eq!(eligible, 110_400_000_000); // $11 040
}

#[test]
fn slide06_prototype_usdc_leg() {
    let (market, eligible) = leg(7, 0, 9200, P_USDC, 5_000 * USD);
    assert_eq!(market, 50_000_000_000); // $5 000
    assert_eq!(eligible, 50_000_000_000); // no haircut
}

/// The blended figures the prototype displays, rebuilt from the three legs the contract valued.
#[test]
fn slide06_prototype_basket_reconciles() {
    let (m_btc, e_btc) = leg(8, 1200, 7000, P65K, 85_000_000);
    let (m_xlm, e_xlm) = leg(7, 2000, 6000, P_XLM, 120_000 * USD);
    let (m_usdc, e_usdc) = leg(7, 0, 9200, P_USDC, 5_000 * USD);

    assert_eq!(m_btc + m_xlm + m_usdc, 740_500_000_000); // $74 050
    assert_eq!(e_btc + e_xlm + e_usdc, 646_600_000_000); // $64 660

    // The prototype's health factor: Σ(eligibleᵢ × liq_thrᵢ / BPS) / debt, at $14 000 of debt.
    let weighted = e_btc * 7000 / 10_000 + e_xlm * 6000 / 10_000 + e_usdc * 9200 / 10_000;
    assert_eq!(weighted, 452_580_000_000); // $45 258
    let debt = 14_000 * USD;
    assert_eq!(weighted * 10_000 / debt, 32_327); // 3.2327 in the deck's units
}

// ===========================================================================
// Slide 08 · The four gates, with the prototype's own defaults
// ===========================================================================

/// Prototype defaults: cap 20 000, per-transaction 2 000, grant LTV ceiling 45 %, swipe 850.
///
/// The gate order is the specification, not an implementation detail — the backend branches on
/// which code comes back. Asserted exhaustively in `draw_tests::first_gate_wins_deterministically`;
/// here it is replayed against the deck's numbers.
#[test]
fn slide08_gate_order_matches_the_prototype() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 2_000 * USD, 4500);

    f.draw(1, 850 * USD); // the prototype's swipe
    assert_eq!(f.grant_of(&f.owner).unwrap().drawn, 850 * USD);

    // Gate 3 · per-transaction
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(2_000 * USD + 1)),
        Error::PerTxLimitExceeded
    );
    // Gate 2 · cumulative cap outranks gate 3
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 3), &(99_999 * USD)),
        Error::CapExceeded
    );
    // Gate 1 · grant validity outranks both
    f.freeze(&f.owner, &f.owner);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 4), &(99_999 * USD)),
        Error::GrantFrozen
    );
    f.unfreeze(&f.owner);
    f.warp_fresh(30 * 86_400, P60K);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &(99_999 * USD)),
        Error::GrantExpired
    );
    f.check_books();
}

/// > *"Quẹt nhiều lần liên tiếp mà không có capture ở giữa: hold cộng dồn và headroom giảm dần.
/// > Đây chính là bài test chứng minh cơ chế hold hoạt động."*
///
/// The backend's holds are its own; the contract's half of that guarantee is that each draw
/// reduces the reported headroom immediately, so two swipes are never approved against the same
/// balance.
#[test]
fn slide08_consecutive_draws_shrink_headroom() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 2_000 * USD, 4500);

    let mut expected = 20_000 * USD;
    for i in 1..=3u8 {
        assert_eq!(
            f.v().quote_draw(&f.owner, &f.operator, &0).cap_headroom,
            expected
        );
        f.draw(i, 850 * USD);
        expected -= 850 * USD;
    }
    assert_eq!(
        f.v().quote_draw(&f.owner, &f.operator, &0).cap_headroom,
        17_450 * USD
    );
    assert_eq!(f.raw_position().principal, 2_550 * USD);
    f.check_books();
}

// ===========================================================================
// Slide 09 · The stale-price matrix
// ===========================================================================

/// | `draw` | blocked | `withdraw_collateral` | blocked | `liquidate` | blocked |
/// | `repay` | allowed | `deposit_collateral` | allowed |
///
/// One deviation, in the customer's favour: `withdraw` with **no debt** is allowed under a stale
/// price, because the LTV check is vacuous at any price and a permanently dead feed would
/// otherwise trap the collateral of someone who has already repaid everything.
///
#[test]
fn slide09_stale_price_matrix() {
    let f = F::new();
    f.armed();
    f.draw(1, 5_000 * USD);
    f.warp(1_000); // the price is now 1 000 s old against a 300 s limit

    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &1),
        Error::PriceStale
    );
    assert_err!(f.v().try_withdraw(&f.owner, &1), Error::PriceStale);
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(100 * USD), &0),
        Error::PriceStale
    );

    f.repay_as(&f.owner, &f.third, &(100 * USD));
    f.v().deposit(&f.owner, &BTC);

    // The documented exception.
    f.v().accrue(&f.owner);
    f.repay_as(&f.owner, &f.third, &f.v().get_position(&f.owner).debt);
    f.v().withdraw(&f.owner, &1);
    f.check_books();
}

// ===========================================================================
// Slide 10 · Liquidation
// ===========================================================================

/// `max_repay = debt × close_factor / BPS` and
/// `seize = repay × price_debt × (BPS + bonus) / BPS / price_collateral`.
#[test]
fn slide10_max_repay_and_seize_formulas() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f.set_price(P16K);

    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.max_repay, 5_000 * USD); // $10 000 × 50 %
    assert_eq!(q.seize_for_max_repay, 33_750_000); // $5 000 × 1.08 ÷ $16 000

    let seized = f.v().liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &0);
    assert_eq!(seized, 33_750_000);
    f.check_books();
}

/// `require(ltv >= liq_threshold, 50)`, `require(repay <= max_repay, 51)`,
/// `require(seize >= min_out, 52)` — the three guards, in the deck's own order.
#[test]
fn slide10_requires_50_51_52() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);

    // 50 — healthy
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(100 * USD), &0),
        Error::PositionHealthy
    );

    f.set_price(P16K);
    // 51 — above the close factor
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(5_000 * USD + 1), &0),
        Error::CloseFactorExceeded
    );
    // 52 — slippage
    assert_err!(
        f.v()
            .try_liquidate(&f.owner, &f.liquidator, &(5_000 * USD), &33_750_001),
        Error::SlippageExceeded
    );
    f.check_books();
}

/// Two routes: permissionless third party **with** bonus, lender of last resort **without** close
/// factor and **without** bonus, netting internally so no debt token moves.
#[test]
fn slide10_two_routes() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    // $12 000/BTC puts LTV at 9470 bps — deep enough that the position is still liquidatable
    // after the first call, which is what lets both routes run in one scenario.
    f.set_price(12_000 * 100_000_000_000_000);

    let vault_debt = f.debt_t().balance(&f.id);
    let lender_debt = f.debt_t().balance(&f.lender);

    // Route 2 first — market price, no bonus, no tokens in.
    let by_lender = f.v().lender_liquidate(&f.owner, &(2_000 * USD));
    assert_eq!(by_lender, 16_666_666); // $2 000 ÷ $12 000, no bonus, rounded down
    assert_eq!(f.debt_t().balance(&f.id), vault_debt);
    assert_eq!(f.debt_t().balance(&f.lender), lender_debt);

    // Route 1 — a third party, paying tokens in and collecting the 8 % bonus.
    let by_third = f.v().liquidate(&f.owner, &f.liquidator, &(2_000 * USD), &0);
    assert_eq!(by_third, 18_000_000); // $2 000 × 1.08 ÷ $12 000
    assert_eq!(f.debt_t().balance(&f.id), vault_debt + 2_000 * USD);
    f.check_books();
}

/// > *"Nếu để hàm revert ở đây, vị thế kẹt vĩnh viễn… Đây là lỗi nghiêm trọng nhất có thể mắc
/// > trong toàn bộ hợp đồng."*
#[test]
fn slide10_insolvent_branch_does_not_revert() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);
    f.set_price(P8K); // $8 000 of collateral against $10 000 of debt

    let before = f.raw_reserve().total;
    let seized = f.v().liquidate(&f.owner, &f.liquidator, &USD, &0);

    assert_eq!(seized, BTC); // everything
    let p = f.raw_position();
    assert_eq!(p.deposit, 0);
    assert_eq!(p.principal, 0);
    assert_eq!(p.interest, 0);

    let r = f.raw_reserve();
    assert_eq!(r.bad_debt, 2_000 * USD);
    assert_eq!(r.total, before - 2_000 * USD); // the claim is written down
    assert_eq!(f.grant_of(&f.owner), None); // and the grant revoked
    f.check_books();
}

/// The deck triggers insolvency on `seize > position.collateral`; the contract triggers on
/// `market_value < debt_value`. **This test covers the band between them** — where the collateral
/// is worth more than the debt but less than the debt plus the liquidator's bonus.
///
/// The deck's own formula is wrong here: `bad_debt += debt − cleared` with `cleared > debt`
/// records a **negative** bad debt. The contract takes the ordinary route instead, clamping the
/// seizure to whatever collateral exists. The liquidator is paid less than the full bonus, the
/// debt clears completely, and nothing is written off — which is the correct answer, because
/// nothing was actually lost.
#[test]
fn slide10_seize_above_collateral_does_not_revert() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);

    // 1 BTC at $10 500 against $10 000 of debt: solvent, liquidatable, and past full_liq so the
    // whole debt may be closed in one call. The 8 % bonus would need $10 800 of collateral.
    f.set_price(10_500 * 100_000_000_000_000);
    let q = f.v().quote_liquidation(&f.owner);
    assert_eq!(q.liquidatable, true);
    assert_eq!(q.max_repay, 10_000 * USD);
    assert_eq!(q.seize_for_max_repay, BTC); // already clamped in the quote

    let before = f.raw_reserve().total;
    let seized = f
        .v()
        .liquidate(&f.owner, &f.liquidator, &(10_000 * USD), &0);

    assert_eq!(seized, BTC); // clamped to what exists, not $10 800 worth
    assert_eq!(f.raw_position().deposit, 0);
    assert_eq!(f.raw_position().principal, 0);
    assert_eq!(f.raw_reserve().bad_debt, 0); // nothing was lost
    assert_eq!(f.raw_reserve().total, before); // the claim is intact
    f.check_books();
}

/// > *"vẫn vi phạm → lặp"*
///
/// A position still over the threshold after one call is liquidated again. The contract needs no
/// mechanism: each call is an independent transaction, and the close factor naturally produces
/// several of them.
#[test]
fn slide10_still_violating_loops() {
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    f.draw(1, 10_000 * USD);

    for (price, repay) in [
        (P16K, 5_000 * USD),
        (12_000 * 100_000_000_000_000, 2_500 * USD),
        (P8K, 1_250 * USD),
    ] {
        f.set_price(price);
        assert_eq!(f.v().quote_liquidation(&f.owner).liquidatable, true);
        f.v().liquidate(&f.owner, &f.liquidator, &repay, &0);
        f.check_books();
    }
    assert_eq!(f.raw_position().principal, 1_250 * USD);
    assert_eq!(f.raw_reserve().bad_debt, 0);
}

// ===========================================================================
// Slide 13 · Smart contract architecture
// ===========================================================================

/// The durability class of every storage key, asserted by reading each class directly.
///
/// Getting this wrong is silent and expensive: a Persistent entry that should be Temporary grows
/// without bound and can never be cleaned, while a Temporary entry that should be Persistent
/// disappears for good when its TTL runs out.
#[test]
fn slide13_storage_durability_classes() {
    let f = F::new();
    f.armed();
    f.draw(1, 1_000 * USD);
    let seen = id32(&f.e, 1);

    f.e.as_contract(&f.id, || {
        let s = f.e.storage();
        // Instance. `Paused` is absent until the brake is pulled — absence is the natural
        // "never paused", with no sentinel timestamp to mistake for a real one.
        assert!(s.instance().has(&DataKey::Config));
        assert!(!s.instance().has(&DataKey::Paused));
        assert!(!s.persistent().has(&DataKey::Config));

        // Persistent — two shared, two keyed by customer. `OracleGuard` is written only when
        // `report_oracle_state` first sees trouble, so it is absent in a healthy market and is
        // checked in the oracle tests instead.
        for k in [
            DataKey::BorrowIndex,
            DataKey::Reserve,
            DataKey::Position(f.owner.clone()),
            DataKey::Grant(f.owner.clone(), f.operator.clone()),
        ] {
            assert!(s.persistent().has(&k), "{k:?} should be persistent");
            assert!(!s.instance().has(&k), "{k:?} must not be in instance");
            assert!(!s.temporary().has(&k), "{k:?} must not be temporary");
        }

        // Temporary — the replay guard, and only the replay guard.
        let k = DataKey::DrawSeen(seen.clone());
        assert!(s.temporary().has(&k));
        assert!(!s.persistent().has(&k));

        // And a customer who has never touched the market has no keys at all. Positions are
        // created on first deposit, which is why `storage::position` must seed `last_index`
        // from the live index rather than from `INDEX_SCALE`.
        assert!(!s.persistent().has(&DataKey::Position(f.owner_c.clone())));
        assert!(!s
            .persistent()
            .has(&DataKey::Grant(f.owner_c.clone(), f.operator.clone())));
    });
}

/// Step 8 emits, step 9 transfers. The order is the reentrancy mitigation: a hostile or reentrant
/// token must see a fully consistent, fully committed state.
///
/// Soroban forbids re-entering a contract that is already on the call stack, so a token cannot
/// call back to observe the state directly. What is observable is the **event log**: a token that
/// publishes during `transfer` must appear *after* the vault's own event. If the vault transferred
/// first, the order would be reversed.
#[test]
fn slide13_event_is_emitted_before_the_token_transfer() {
    let e = Env::default();
    e.mock_all_auths();
    e.ledger().with_mut(|l| {
        l.timestamp = START;
        l.sequence_number = 1000;
        l.max_entry_ttl = 3_110_400;
        l.min_persistent_entry_ttl = 100;
        l.min_temp_entry_ttl = 100;
    });

    let owner = Address::generate(&e);
    let lender = Address::generate(&e);
    let oracle_admin = Address::generate(&e);

    let dep = e.register(NoisyToken, (DEP_DEC,));
    let debt = e.register(MockToken, (DEBT_DEC,));
    let oracle = e.register(mock_oracle::MockOracle, (oracle_admin.clone(), ORACLE_DEC));

    let parts = FParts {
        lender: lender.clone(),
        dep: dep.clone(),
        debt: debt.clone(),
        oracle: oracle.clone(),
    };
    let id = e.register(crate::CollateralVault, (cfg(&parts),));

    NoisyTokenClient::new(&e, &dep).mint(&owner, &(10 * BTC));
    crate::CollateralVaultClient::new(&e, &id).deposit(&owner, &BTC);

    let deposited = Symbol::new(&e, "deposited");
    let moved = Symbol::new(&e, "xfer");
    let (mut i, mut j) = (None, None);
    for (n, ev) in e.events().all().events().iter().enumerate() {
        let soroban_sdk::xdr::ContractEventBody::V0(b) = &ev.body;
        let name = Symbol::try_from_val(&e, &b.topics.to_vec()[1]).unwrap();
        if name == deposited && i.is_none() {
            i = Some(n);
        }
        if name == moved && j.is_none() {
            j = Some(n);
        }
    }
    let i = i.expect("the vault emitted no Deposited event");
    let j = j.expect("the token stayed quiet — the probe never fired");
    assert!(
        i < j,
        "the token moved before the vault emitted: step 9 ran ahead of step 8"
    );
}

/// A token that fails takes the whole write with it. Nothing is half-applied, which is what makes
/// "transfer last" safe rather than merely tidy.
#[test]
#[should_panic(expected = "insufficient balance")]
fn slide13_a_failing_transfer_reverts_the_whole_write() {
    let f = F::new();
    f.funded();
    // Drain the vault's debt-token balance behind its back, then try to draw.
    f.grant(20_000 * USD, 20_000 * USD, 5000);
    let away: MuxedAddress = f.third.clone().into();
    f.debt_t().transfer(&f.id, away, &(100_000 * USD));
    f.draw(1, 1_000 * USD);
}

/// Every code in the deck's slide-13 table, triggered exactly.
///
/// Sixteen of the eighteen are exercised here. The other two are unreachable by construction and
/// recorded rather than faked: `1 NotInitialized` cannot occur because `__constructor` runs inside
/// the deploy transaction, and `32 GrantRevoked` is **retired** — revocation deletes the grant
/// entry, so a draw after revocation returns `30 GrantNotFound` and 32 stays permanently vacant.
#[test]
fn slide13_error_table() {
    // 2 · Unauthorized — the manual role check, which now lives only in `freeze_grant`.
    // `pause` lost its `by` parameter when it became lender-only, so a wrong caller there fails
    // on authorisation rather than on a role comparison.
    let f = F::new();
    f.funded();
    f.grant(20_000 * USD, 5_000 * USD, 5000);
    assert_err!(
        f.v().try_freeze_grant(&f.owner, &f.operator, &f.third),
        Error::Unauthorized
    );
    let f = F::new();

    // 12 · InvalidAmount
    assert_err!(f.v().try_deposit(&f.owner, &0), Error::InvalidAmount);

    // 10 · InsufficientCollateral → InsufficientDeposit
    f.v().deposit(&f.owner, &BTC);
    assert_err!(
        f.v().try_withdraw(&f.owner, &(BTC + 1)),
        Error::InsufficientDeposit
    );

    // 20 · ReserveUnderfunded — at acceptance, where the capital is committed.
    f.propose(USD, USD, 5000);
    assert_err!(
        f.v().try_accept_grant(&f.owner, &f.operator),
        Error::ReserveUnderfunded
    );
    f.v().revoke_authority(&f.owner, &f.operator);

    f.v().fund_reserve(&(100_000 * USD));

    // 22 · ExposureCapExceeded
    let h = F::with(|mut c| {
        c.vault_cap = 1_000 * USD;
        c
    });
    h.funded();
    h.propose(1_001 * USD, 1_000 * USD, 5000);
    assert_err!(
        h.v().try_accept_grant(&h.owner, &h.operator),
        Error::ExposureCapExceeded
    );

    // 30 · GrantNotFound
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &1),
        Error::GrantNotFound
    );

    f.grant(20_000 * USD, 5_000 * USD, 5000);

    // 21 · ReserveLocked
    assert_err!(
        f.v().try_withdraw_reserve(&(80_001 * USD)),
        Error::ReserveLocked
    );

    // 34 · PerTxLimitExceeded
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(5_000 * USD + 1)),
        Error::PerTxLimitExceeded
    );
    // 33 · CapExceeded
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 3), &(99_999 * USD)),
        Error::CapExceeded
    );

    f.draw(4, 5_000 * USD);
    // 36 · DuplicateDrawId
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 4), &(5_000 * USD)),
        Error::DuplicateDrawId
    );

    // 13 · Paused
    f.v().pause();
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &1),
        Error::Paused
    );
    f.v().unpause();

    // 11 · HealthTooLow — withdrawing past the protocol ceiling
    assert_err!(f.v().try_withdraw(&f.owner, &BTC), Error::HealthTooLow);

    // 35 · LtvCeilingExceeded
    f.revoke(&f.owner);
    f.grant(20_000 * USD, 20_000 * USD, 1000);
    assert_err!(
        f.v()
            .try_draw(&f.owner, &f.operator, &id32(&f.e, 6), &(5_281 * USD)),
        Error::LtvCeilingExceeded
    );

    // 40 · PriceStale
    f.warp(1_000);
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 7), &1),
        Error::PriceStale
    );
    // 41 · PriceInvalid
    f.kill_oracle();
    assert_err!(
        f.v().try_draw(&f.owner, &f.operator, &id32(&f.e, 8), &1),
        Error::PriceInvalid
    );

    f.check_books();
    h.check_books();
}

/// > *"Bất biến assert ở cuối mọi hàm ghi: `total ≥ committed + outstanding`."*
///
/// Asserted after every scenario in the suite via `check_books`, and after every step of every
/// random sequence in `property_tests::invariants_hold_under_random_traffic`. Here it is stated
/// once against the deck, together with the equality a draw preserves.
#[test]
fn slide13_invariant_total_ge_committed_plus_outstanding() {
    let f = F::new();
    f.armed();

    let before = f.raw_reserve();
    assert!(before.total >= before.committed + before.outstanding);

    f.draw(1, 5_000 * USD);
    let after = f.raw_reserve();
    assert!(after.total >= after.committed + after.outstanding);
    // A draw moves capital between drawers without changing the sum.
    assert_eq!(
        before.committed + before.outstanding,
        after.committed + after.outstanding
    );
    f.check_books();
}

// ===========================================================================
// Slide 18 · Sprint definitions of done · Slide 20 · Milestone M1
// ===========================================================================

/// S2: *"bảng lãi 1/30/365 khớp"*. The figures are asserted in
/// `interest_tests::exact_interest_at_one_thirty_and_three_hundred_sixty_five_days`; this is the
/// deck-facing restatement — 10 %/yr on $10 000 for a year is exactly $1 000.00.
#[test]
fn s2_interest_table_1_30_365() {
    for (days, expected) in [
        (1u64, 27_397_260i128),
        (30, 821_917_808),
        (365, 10_000_000_000),
    ] {
        let f = F::new();
        f.funded();
        f.grant(20_000 * USD, 20_000 * USD, 5000);
        f.draw(1, 10_000 * USD);
        f.warp(days * 86_400);
        f.v().accrue(&f.owner);
        assert_eq!(f.raw_position().interest, expected, "{days} days");
    }
}

/// S4: *"5 kịch bản draw trên testnet, mỗi cái một mã lỗi riêng"* — five draw scenarios, each
/// producing a **distinct** code.
#[test]
fn s4_five_draw_scenarios_five_distinct_codes() {
    let f = F::new();
    f.funded();
    // Ceiling 500 bps allows $2 640, comfortably under the $5 000 per-transaction limit, so the
    // LTV gate can actually be reached rather than being masked by gate 3.
    f.grant(20_000 * USD, 5_000 * USD, 500);

    let codes = [
        (
            f.v()
                .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(5_000 * USD + 1))
                .err()
                .unwrap()
                .unwrap(),
            Error::PerTxLimitExceeded,
        ),
        (
            f.v()
                .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(99_999 * USD))
                .err()
                .unwrap()
                .unwrap(),
            Error::CapExceeded,
        ),
        (
            f.v()
                .try_draw(&f.owner, &f.operator, &id32(&f.e, 3), &(3_000 * USD))
                .err()
                .unwrap()
                .unwrap(),
            Error::LtvCeilingExceeded,
        ),
        (
            f.v()
                .try_draw(&f.owner, &f.third, &id32(&f.e, 4), &(100 * USD))
                .err()
                .unwrap()
                .unwrap(),
            Error::GrantNotFound,
        ),
        (
            f.v()
                .try_draw(&f.owner, &f.operator, &id32(&f.e, 5), &0)
                .err()
                .unwrap()
                .unwrap(),
            Error::InvalidAmount,
        ),
    ];

    for (got, want) in codes.iter() {
        assert_eq!(got, want);
    }
    // Distinct, which is the part the deck actually asks for.
    let mut seen: [u32; 5] = [
        codes[0].0 as u32,
        codes[1].0 as u32,
        codes[2].0 as u32,
        codes[3].0 as u32,
        codes[4].0 as u32,
    ];
    seen.sort_unstable();
    for w in seen.windows(2) {
        assert_ne!(w[0], w[1], "two scenarios returned the same code");
    }
}

/// M1: *"8 mã băm giao dịch testnet gồm 4 lệnh bị chặn với 4 mã lỗi khác nhau"*.
///
/// The four rejections of the acceptance scenario, in the order `docs/10-testing.md` records
/// them: a grant ceiling above the protocol's, a second grant while one is live, a reused
/// `draw_id`, and a draw over the per-transaction limit.
#[test]
fn m1_four_blocked_calls_four_distinct_codes() {
    let f = F::new();
    f.funded();

    let a = f
        .v()
        .try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &(20_000 * USD),
            &(5_000 * USD),
            &(f.now() + 86_400),
            &9999,
        )
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(a, Error::GrantCeilingTooHigh);

    f.grant(20_000 * USD, 5_000 * USD, 5000);
    let b = f
        .v()
        .try_grant_authority(
            &f.owner,
            &f.operator,
            &f.settlement,
            &(1_000 * USD),
            &(1_000 * USD),
            &(f.now() + 86_400),
            &5000,
        )
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(b, Error::GrantExists);

    f.draw(1, 1_000 * USD);
    let c = f
        .v()
        .try_draw(&f.owner, &f.operator, &id32(&f.e, 1), &(1_000 * USD))
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(c, Error::DuplicateDrawId);

    let d = f
        .v()
        .try_draw(&f.owner, &f.operator, &id32(&f.e, 2), &(5_000 * USD + 1))
        .err()
        .unwrap()
        .unwrap();
    assert_eq!(d, Error::PerTxLimitExceeded);

    let mut codes = [a as u32, b as u32, c as u32, d as u32];
    codes.sort_unstable();
    for w in codes.windows(2) {
        assert_ne!(w[0], w[1]);
    }
    f.check_books();
}

// ===========================================================================
// A token that announces itself, for the step 8 / step 9 ordering test
// ===========================================================================

#[contractevent(topics = ["mocktok", "xfer"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenMoved {
    pub amount: i128,
}

/// `MockToken`, plus one event published from inside `transfer`. That event is the probe: where
/// it lands in the log tells you whether the vault emitted before or after it moved the money.
#[contract]
pub struct NoisyToken;

#[contractimpl]
impl NoisyToken {
    pub fn __constructor(e: Env, decimals: u32) {
        e.storage().instance().set(&TKey::Dec, &decimals);
    }
    pub fn decimals(e: Env) -> u32 {
        e.storage().instance().get(&TKey::Dec).unwrap()
    }
    pub fn balance(e: Env, id: Address) -> i128 {
        e.storage().persistent().get(&TKey::Bal(id)).unwrap_or(0)
    }
    pub fn mint(e: Env, to: Address, amount: i128) {
        let b = Self::balance(e.clone(), to.clone());
        e.storage().persistent().set(&TKey::Bal(to), &(b + amount));
    }
    pub fn transfer(e: Env, from: Address, to: MuxedAddress, amount: i128) {
        from.require_auth();
        let to = to.address();
        let bf = Self::balance(e.clone(), from.clone());
        assert!(bf >= amount, "mock token: insufficient balance");
        let bt = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&TKey::Bal(from), &(bf - amount));
        e.storage().persistent().set(&TKey::Bal(to), &(bt + amount));
        TokenMoved { amount }.publish(&e);
    }
}
