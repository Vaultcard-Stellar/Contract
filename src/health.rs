//! Valuation, LTV, health, liquidation price, risk band.
//!
//! Pure functions over already-loaded structs — no storage, no `Env` state beyond the arithmetic
//! host calls. That is what keeps them unit-testable in isolation.
//!
//! # Two scales meet here, and mixing them is the easiest way to be wrong by a power of ten
//!
//! `position.deposit` is in **deposit-token** units and needs a price to become money.
//! `principal` and `interest` are in **debt-token** units and need only a rescale, because the
//! debt token is worth exactly 1 USD. Everything downstream — every gate, `max_repay`,
//! `seize_value`, `liq_price` — works on the **internal USD scale** and nothing else.

use crate::errors::Error;
use crate::math;
use crate::types::*;
use soroban_sdk::Env;

/// A position valued at one price, computed once and passed around.
#[derive(Clone, Debug)]
pub struct Valuation {
    /// USD scale.
    pub market: i128,
    /// USD scale, after the haircut.
    pub eligible: i128,
    /// USD scale, rescaled from debt-token units, rounded **up**.
    pub debt: i128,
}

/// `deposit_decimals + price_decimals − USD_DECIMALS`. **May be negative.**
fn shift(cfg: &Config, price_decimals: u32) -> i32 {
    cfg.deposit_decimals as i32 + price_decimals as i32 - USD_DECIMALS as i32
}

/// Deposit-token amount + price → USD scale.
///
/// One definition. A second one would eventually disagree with this one.
pub fn value_usd(
    e: &Env,
    amount: i128,
    cfg: &Config,
    price: i128,
    price_decimals: u32,
    up: bool,
) -> Result<i128, Error> {
    if amount == 0 || price == 0 {
        return Ok(0);
    }
    // amount × price / 10^shift  ==  amount × price × 10^(−shift) / 1
    math::mul_scaled_div(e, amount, price, -shift(cfg, price_decimals), 1, up)
}

/// USD scale → deposit-token amount. The inverse of [`value_usd`], rounds **down**.
///
/// Rounding down means a liquidator receives one unit less and the vault keeps it — the
/// direction every conversion in this contract leans.
pub fn from_usd(
    e: &Env,
    value: i128,
    cfg: &Config,
    price: i128,
    price_decimals: u32,
) -> Result<i128, Error> {
    if value == 0 {
        return Ok(0);
    }
    if price <= 0 {
        return Err(Error::PriceInvalid);
    }
    // value × 10^shift / price
    math::mul_scaled_div(e, value, 1, shift(cfg, price_decimals), price, false)
}

/// Debt-token units → USD scale, rounded **up**.
///
/// A pure rescale — the debt token is worth exactly 1 USD, so no price is involved. When
/// `debt_decimals == USD_DECIMALS` this is the identity, which is the common case and the
/// cheapest path.
///
/// **Nothing downstream may receive a raw debt figure.** `eligible_value` is on the USD scale;
/// comparing a debt-token figure against it is only correct when the decimals happen to match,
/// which is true of the test fixture and false in general.
pub fn debt_value_usd(e: &Env, raw: i128, cfg: &Config) -> Result<i128, Error> {
    math::scale(e, raw, cfg.debt_decimals as i32 - USD_DECIMALS as i32, true)
}

/// Total debt in debt-token units.
pub fn debt_raw(p: &Position) -> Result<i128, Error> {
    math::checked_add(p.principal, p.interest)
}

pub fn value(
    e: &Env,
    cfg: &Config,
    p: &Position,
    price: i128,
    price_decimals: u32,
) -> Result<Valuation, Error> {
    let market = value_usd(e, p.deposit, cfg, price, price_decimals, false)?;
    let eligible = math::mul_div_floor(e, market, BPS - cfg.haircut_bps as i128, BPS)?;
    let debt = debt_value_usd(e, debt_raw(p)?, cfg)?;
    Ok(Valuation {
        market,
        eligible,
        debt,
    })
}

// ---------------------------------------------------------------------------
// Gates — cross-multiplied, never divided
// ---------------------------------------------------------------------------

/// `debt × BPS <= eligible × ceiling_bps`, exactly.
///
/// Gates never compute an LTV. `ltv_bps` rounds up by as much as a basis point, which on a
/// 5000 bps ceiling against $57 200 of eligible value is roughly $5.72 of borrowing power —
/// the coarsest rounding effect in the contract, and entirely avoidable by not dividing.
pub fn within_ltv(e: &Env, v: &Valuation, ceiling_bps: u32) -> Result<bool, Error> {
    math::cross_le(e, v.debt, BPS, v.eligible, ceiling_bps as i128)
}

/// The one liquidation predicate. `liquidate`, `lender_liquidate`, `quote_liquidation` and the
/// risk band all call this.
///
/// The boundary is **inclusive**: `ltv == liq_threshold` is liquidatable. These three say the
/// same thing — `ltv_bps >= liq_threshold_bps`, `health_bps <= 10_000`, and the comparison
/// below. Two independent formulations would eventually disagree by one unit, and that unit is
/// a dispute about someone's collateral.
pub fn is_liquidatable(e: &Env, v: &Valuation, cfg: &Config) -> Result<bool, Error> {
    if v.debt <= 0 {
        return Ok(false);
    }
    math::cross_ge(e, v.debt, BPS, v.eligible, cfg.liq_threshold_bps as i128)
}

/// At or above `full_liq_ltv_bps` the close factor is dropped.
///
/// Cross-multiplied for the same reason every other gate is: this feeds error 51, so a rounded
/// input would move the close-factor boundary.
pub fn at_or_above_full_liq(e: &Env, v: &Valuation, cfg: &Config) -> Result<bool, Error> {
    if v.debt <= 0 {
        return Ok(false);
    }
    math::cross_ge(e, v.debt, BPS, v.eligible, cfg.full_liq_ltv_bps as i128)
}

// ---------------------------------------------------------------------------
// Display — never used by a gate
// ---------------------------------------------------------------------------

pub fn ltv_bps(e: &Env, v: &Valuation) -> Result<u32, Error> {
    if v.debt <= 0 {
        return Ok(0);
    }
    if v.eligible <= 0 {
        return Ok(u32::MAX);
    }
    let r = math::mul_div_ceil(e, v.debt, BPS, v.eligible)?;
    Ok(if r > u32::MAX as i128 {
        u32::MAX
    } else {
        r as u32
    })
}

/// `eligible × liq_threshold_bps / debt`. **No `BPS` in the denominator.**
///
/// The boundary is `debt × BPS == eligible × liq_thr`; substituting gives
/// `eligible × liq_thr / debt == BPS == 10_000`, which is the definition. Dividing by
/// `debt × BPS` instead yields **1** at the boundary — the wrong form looks symmetrical and
/// reads plausibly, which is why it is worth stating.
pub fn health_bps(e: &Env, v: &Valuation, cfg: &Config) -> Result<u32, Error> {
    if v.debt <= 0 {
        return Ok(u32::MAX);
    }
    let r = math::mul_div_floor(e, v.eligible, cfg.liq_threshold_bps as i128, v.debt)?;
    Ok(if r > u32::MAX as i128 {
        u32::MAX
    } else {
        r as u32
    })
}

/// The oracle price at which the position becomes liquidatable, **in oracle price scale** — so
/// it is directly comparable to what the feed reports.
///
/// Inverting the valuation identity reintroduces the `10^shift` factor that `value_usd` divided
/// out; without it the number is in no unit at all, and the position screen would render
/// something meaningless next to a real price.
///
/// ```text
/// P = debt × BPS² × 10^shift / (deposit × (BPS − haircut) × liq_thr)
/// ```
///
/// Evaluated in three rounded-up steps, so the result can overstate by a couple of units. That
/// is the safe direction — a higher liquidation price warns earlier.
pub fn liq_price(
    e: &Env,
    cfg: &Config,
    p: &Position,
    v: &Valuation,
    price_decimals: u32,
) -> Result<i128, Error> {
    if p.deposit <= 0 || v.debt <= 0 {
        return Ok(0);
    }
    let t1 = math::mul_scaled_div(e, v.debt, BPS, shift(cfg, price_decimals), p.deposit, true)?;
    let t2 = math::mul_div_ceil(e, t1, BPS, BPS - cfg.haircut_bps as i128)?;
    math::mul_div_ceil(e, t2, 1, cfg.liq_threshold_bps as i128)
}

/// Advisory for the UI. Enforcement is the individual gates.
///
/// **The boundary belongs to `Healthy`.** The withdrawal gate permits `ltv_after <= max_ltv`, so
/// a band that said `Warning` at exactly `max_ltv` would tell the interface to block something
/// the contract allows.
pub fn band(e: &Env, v: &Valuation, cfg: &Config) -> Result<Band, Error> {
    if is_liquidatable(e, v, cfg)? {
        return Ok(Band::Liquidatable);
    }
    if v.debt <= 0 {
        return Ok(Band::Healthy);
    }
    // ltv > max_ltv
    if !within_ltv(e, v, cfg.max_ltv_bps)? {
        return Ok(Band::Warning);
    }
    // ltv >= 0.9 × max_ltv  →  debt × BPS × 10 >= eligible × max_ltv × 9
    //
    // The two bare multiplications are constants, not money: `BPS * 10` is 100 000 and
    // `max_ltv_bps` is bounded by BPS, so `× 9` cannot exceed 90 000. The no-bare-arithmetic
    // rule is about values that scale with balances, and neither of these does.
    let watch = math::cross_ge(e, v.debt, BPS * 10, v.eligible, cfg.max_ltv_bps as i128 * 9)?;
    Ok(if watch { Band::Watch } else { Band::Healthy })
}

/// How much more debt the current price supports, in **debt-token units**.
///
/// `eligible × max_ltv / BPS − debt`, converted back out of the USD scale. Rounds down.
pub fn ltv_headroom(e: &Env, cfg: &Config, v: &Valuation, ceiling_bps: u32) -> Result<i128, Error> {
    let allowed = math::mul_div_floor(e, v.eligible, ceiling_bps as i128, BPS)?;
    if allowed <= v.debt {
        return Ok(0);
    }
    let spare_usd = allowed - v.debt;
    // USD → debt-token units: the inverse of debt_value_usd, rounding down.
    math::scale(
        e,
        spare_usd,
        USD_DECIMALS as i32 - cfg.debt_decimals as i32,
        false,
    )
}
