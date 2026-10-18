//! Close-factor, seize, and the insolvency computation.
//!
//! Pure functions over already-loaded structs.

use crate::errors::Error;
use crate::health::{self, Valuation};
use crate::math;
use crate::types::*;
use soroban_sdk::Env;

/// The most of the debt one call may repay, in **USD scale**.
///
/// # The toxic band, and why `full_liq_ltv_bps` exists
///
/// A liquidation removes more collateral than the debt it clears — by exactly the bonus. Below
/// a threshold that still pulls LTV down; above it, LTV goes **up**:
///
/// ```text
/// eligible 10 000, debt 9 500, LTV 95%
/// repay 50% = 4 750, seize 4 750 × 1.08 = 5 130
/// after: eligible 4 870, debt 4 750 → LTV 97.5%   ← worse
/// ```
///
/// The break-even is `LTV = 1/(1 + bonus)` — 92.6% at an 8% bonus. Above it every by-the-book
/// liquidation pushes the position closer to total loss while the liquidator collects another
/// 8% each time. `full_liq_ltv_bps` is set below break-even and the constructor enforces the
/// relationship, so a configuration that spirals cannot be deployed.
///
/// # The dust escape
///
/// Below `dust_threshold` a full close is allowed regardless of LTV. Otherwise a 50% close
/// factor leaves $3 remainders that no liquidator will bother to clear, and positions hang open.
pub fn max_repay(e: &Env, cfg: &Config, v: &Valuation) -> Result<i128, Error> {
    if health::at_or_above_full_liq(e, v, cfg)? || v.debt < cfg.dust_threshold {
        return Ok(v.debt);
    }
    math::mul_div_floor(e, v.debt, cfg.close_factor_bps as i128, BPS)
}

/// USD value of collateral seized for a given repayment, bonus included. Rounds **down**.
pub fn seize_value(e: &Env, cfg: &Config, repay_usd: i128) -> Result<i128, Error> {
    math::mul_div_floor(e, repay_usd, BPS + cfg.liq_bonus_bps as i128, BPS)
}

/// USD value of collateral seized by the lender: **market price, no bonus**.
pub fn seize_value_lender(repay_usd: i128) -> i128 {
    repay_usd
}

/// Is the position beyond saving?
///
/// # Market value, not eligible value
///
/// Haircut is a buffer for **lending** decisions, not for **recovery**. Triggering on
/// `eligible` would declare insolvency while up to `haircut_bps` of collateral remains — enough,
/// possibly, to repay the debt in full — and write off principal that was never lost.
pub fn is_insolvent(v: &Valuation) -> bool {
    v.debt > 0 && v.market < v.debt
}

/// What the insolvency branch resolves to.
pub struct Wipeout {
    /// Everything the position held, in deposit-token units.
    pub seized: i128,
    /// USD recovered, capped at the debt — never credit more than was owed.
    pub recovered: i128,
}

/// Take everything and value it, capped at the debt.
///
/// **This branch must never revert for a reason internal to the contract.** A subtraction below
/// zero, a failed assert, an unchecked panic — any of those leaves the position stuck forever:
/// nobody can close it, the collateral cannot be withdrawn, the debt cannot be cleared. It is
/// the single most serious mistake available in this contract, which is why the clamping here
/// is explicit rather than inherited from the ordinary path.
pub fn wipeout(
    e: &Env,
    cfg: &Config,
    p: &Position,
    v: &Valuation,
    price: i128,
    price_decimals: u32,
) -> Result<Wipeout, Error> {
    let seized = p.deposit;
    let gross = health::value_usd(e, seized, cfg, price, price_decimals, false)?;
    Ok(Wipeout {
        seized,
        recovered: math::min_i128(gross, v.debt),
    })
}

/// Convert a USD figure back to debt-token units, rounding **down**.
///
/// Used to turn a USD repayment cap into the number the caller actually passes.
pub fn usd_to_debt(e: &Env, cfg: &Config, usd: i128) -> Result<i128, Error> {
    math::scale(
        e,
        usd,
        USD_DECIMALS as i32 - cfg.debt_decimals as i32,
        false,
    )
}
