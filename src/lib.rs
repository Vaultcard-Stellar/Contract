#![no_std]
//! # Collateral Vault — a market vault
//!
//! **One contract per (deposit token, debt token) **pair**, shared by every customer who deposits it.** Each customer
//! holds a [`Position`] keyed by their own address, draws on a revolving credit line through an
//! authorised operator, and is liquidated permissionlessly if the price falls far enough. The
//! debt token is fixed for the life of the market.
//!
//! This shape is what makes the rest of the system possible. An indexer can follow one contract;
//! it cannot follow a million. And liquidation is only meaningfully permissionless when a third
//! party has **one address to watch** rather than a registry they must trust us to publish.
//!
//! # There is no administrator, and no owner field
//!
//! [`Config`] is frozen at construction — every field, `oracle` included. No address can change a
//! threshold, swap the price feed, or upgrade the code. The `lender` funds the shared [`Reserve`]
//! and can pull an emergency brake that expires by itself after seven days; that is the entire
//! list of privileges anyone holds.
//!
//! The price of that is real and stated rather than hidden: if the feed dies permanently, `draw`
//! and `liquidate` stop working here forever. `repay` reads no price and `withdraw` reads none
//! once a debt is clear, so the market can always wind itself down — every customer can get their
//! collateral out. The lender carries the loss on anyone who declines to repay.
//!
//! # The ten-step write discipline
//!
//! Every write function follows the same order, and three of the steps are load-bearing:
//!
//! ```text
//!  1 require_auth on the correct role
//!  2 validate parameters
//!  3 accrue()                      must not read the price
//!  4 load state                    NAMING whose position
//!  5 guards, cheap → expensive     storage reads before cross-contract calls
//!  6 apply changes
//!  7 assert invariants
//!  8 emit event                    naming whose position
//!  9 token transfer                ALWAYS SECOND-TO-LAST
//! 10 bump TTL                      ALWAYS LAST, and BOUNDED
//! ```
//!
//! **Step 9 second-to-last** is the reentrancy mitigation: a hostile or reentrant token sees a
//! fully consistent, fully committed state. **Step 10** exists because Persistent entries
//! archive, and an archived position means that customer cannot withdraw — a failure that needs
//! no bug and no attacker, just eight quiet months. In a market vault it must also be *bounded*:
//! it touches the shared keys and the one position this call named, never every position.
//!
//! # Why every entry point returns `Result`
//!
//! `try_*` on the generated client yields `Err(Ok(Error::X))` only for functions declared to
//! return `Result`. With `panic_with_error!` a test can assert nothing more precise than "it
//! failed", and the specification requires exact-code assertions — the backend branches on the
//! number, so a test that cannot tell 33 from 35 is not testing the thing that matters.

pub mod errors;
pub mod events;
pub mod grant;
pub mod health;
pub mod idempotency;
pub mod interest;
pub mod invariant;
pub mod liquidation;
pub mod math;
pub mod oracle;
pub mod storage;
pub mod types;

#[cfg(test)]
mod test;

pub use errors::Error;
pub use types::*;

use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env};

#[contract]
pub struct CollateralVault;

// ---------------------------------------------------------------------------
// internal helpers — no arithmetic lives in the entry points themselves
// ---------------------------------------------------------------------------

/// One customer's slice of the market, loaded together with the market's shared state.
///
/// `pos0` and `res0` are the values **as loaded**. They exist for invariant I3′, which asserts
/// that this call moved `reserve.outstanding` by exactly what it moved `position.principal` — the
/// differential form of the equality a single-customer vault could check absolutely. See
/// `invariant.rs`.
struct State {
    cfg: Config,
    owner: Address,
    idx: IndexState,
    pos: Position,
    res: Reserve,
    pos0: Position,
    res0: Reserve,
}

fn load(e: &Env, owner: &Address) -> Result<State, Error> {
    let pos = storage::position(e, owner);
    let res = storage::reserve(e);
    Ok(State {
        cfg: storage::config(e)?,
        owner: owner.clone(),
        idx: storage::index_state(e),
        pos0: pos.clone(),
        res0: res.clone(),
        pos,
        res,
    })
}

/// One grant, named by the operator it authorises.
///
/// Loaded here rather than in [`load`] because most calls need no grant at all — a deposit, a
/// withdrawal, an accrual — and a customer may hold several. Loading all of them on every call
/// would make the cost of a deposit grow with the number of providers the customer uses.
fn load_grant(e: &Env, owner: &Address, operator: &Address) -> Result<Grant, Error> {
    storage::grant(e, owner, operator).ok_or(Error::GrantNotFound)
}

fn save(e: &Env, s: &State) -> Result<(), Error> {
    invariant::check(&s.res0, &s.res, &s.pos0, &s.pos, &s.idx)?;
    storage::set_index_state(e, &s.idx);
    storage::set_position(e, &s.owner, &s.pos);
    storage::set_reserve(e, &s.res);
    Ok(())
}

/// Write a grant back, or delete it and drop the operator from the customer's index.
///
/// The index and the entry move together. A grant whose entry is gone but whose operator is still
/// listed makes the insolvency sweep read a `None` it has to tolerate; one listed nowhere but still
/// stored is capital nobody will ever release.
fn put_grant(e: &Env, owner: &Address, operator: &Address, g: Option<&Grant>) {
    match g {
        Some(g) => storage::set_grant(e, owner, operator, g),
        None => {
            storage::remove_grant(e, owner, operator);
            let ops = storage::grant_operators(e, owner);
            let mut kept = soroban_sdk::Vec::new(e);
            for candidate in ops.iter() {
                if candidate != *operator {
                    kept.push_back(candidate);
                }
            }
            storage::set_grant_operators(e, owner, &kept);
        }
    }
}

/// Advance the market clock and settle this customer against it.
///
/// The index is shared; the settlement is not. A position untouched for a year settles correctly
/// on its next call, because the index carries the whole history and the position remembers where
/// it left off. This is what a borrow index is actually for, and what a single-customer vault
/// never needed it for.
fn accrue_into(e: &Env, s: &mut State) -> Result<i128, Error> {
    // Emitted when **anything the accrual clock owns actually changed** — the market index, or
    // this position's `last_index` / `accrual_rem` / `interest`.
    //
    // Two narrower conditions were tried and both left state that could not be rebuilt from the
    // stream. `inc > 0` misses a position with no principal, whose clock still advances.
    // `moved || inc > 0` additionally misses the settlement that **clears `accrual_rem`** when a
    // debt is repaid in full inside the same ledger as the previous accrual: the index has not
    // moved, no interest is added, and yet the position changed. The rebuild test compares
    // `accrual_rem` field by field, which is exactly how that one surfaced.
    //
    // The condition still skips the case worth skipping: a repeated call inside one ledger, on a
    // position already settled, moves nothing and emits nothing.
    let moved = interest::advance(e, &s.cfg, &mut s.idx)?;
    let before = s.pos.clone();
    let inc = interest::settle(e, &mut s.pos, s.idx.index)?;
    let settled = s.pos.last_index != before.last_index || s.pos.accrual_rem != before.accrual_rem;
    if moved || inc > 0 || settled {
        events::Accrued {
            owner: s.owner.clone(),
            amount: inc,
            interest_after: s.pos.interest,
            index: s.idx.index,
            last_accrual: s.idx.last_accrual,
            accrual_rem: s.pos.accrual_rem,
        }
        .publish(e);
    }
    Ok(inc)
}

fn require_unpaused(e: &Env) -> Result<(), Error> {
    if storage::paused(e) {
        return Err(Error::Paused);
    }
    Ok(())
}

fn deposit_client<'a>(e: &'a Env, cfg: &Config) -> token::TokenClient<'a> {
    token::TokenClient::new(e, &cfg.deposit_token)
}

fn debt_client<'a>(e: &'a Env, cfg: &Config) -> token::TokenClient<'a> {
    token::TokenClient::new(e, &cfg.debt_token)
}

/// The full validation set, run once, by the constructor.
///
/// It used to be shared with `propose_config` so a rule added to one could not be forgotten in the
/// other. There is no `propose_config` any more, and this is the only gate a market's parameters
/// will ever pass through — which makes it the last chance to catch a configuration that would
/// hurt every customer in it.
fn validate(cfg: &Config) -> Result<(), Error> {
    if cfg.deposit_token == cfg.debt_token {
        return Err(Error::InvalidConfig);
    }
    if cfg.haircut_bps >= BPS_U32
        || cfg.max_ltv_bps == 0
        || cfg.max_ltv_bps >= cfg.liq_threshold_bps
        || cfg.liq_threshold_bps >= cfg.full_liq_ltv_bps
        || cfg.full_liq_ltv_bps >= BPS_U32
        || cfg.liq_bonus_bps >= BPS_U32
        || cfg.close_factor_bps == 0
        || cfg.close_factor_bps > BPS_U32
        || cfg.rate_bps_year > BPS_U32
        || cfg.price_max_age < 60
        || cfg.vault_cap <= 0
        || cfg.dust_threshold < 0
    {
        return Err(Error::InvalidConfig);
    }
    // The break-even constraint: full_liq_ltv <= 1/(1 + bonus), in integer form.
    //
    // Above that ratio a partial liquidation raises LTV instead of lowering it and the position
    // spirals — every by-the-book liquidation pushing it closer to total loss while the
    // liquidator collects the bonus each time. Checking it at deploy stops the configuration
    // before anyone loses money, rather than discovering it when they have.
    //
    // Bare arithmetic on bounded parameters, not on balances: both operands are basis-point
    // figures already checked against BPS above, so the product cannot exceed ~2 × 10^8.
    let lhs = cfg.full_liq_ltv_bps as i128 * (BPS + cfg.liq_bonus_bps as i128);
    if lhs > BPS * BPS {
        return Err(Error::InvalidConfig);
    }
    Ok(())
}

#[contractimpl]
impl CollateralVault {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Runs **inside the deploy transaction**, so there is no window in which someone else can
    /// initialise the market.
    ///
    /// This is the **only** opportunity to get the configuration right. Nothing here can be
    /// changed afterwards, so a market deployed with a threshold that is too generous carries it
    /// for life, and correcting it costs a new market plus a voluntary migration by every
    /// customer. That is the deliberate trade for having no privileged address at all.
    ///
    /// **Decimals are read from the token contracts**, overwriting whatever the caller passed.
    /// Trusting a caller-supplied value means one wrong digit makes every valuation off by ten,
    /// and nothing downstream catches it because everything stays internally consistent.
    pub fn __constructor(e: Env, config: Config) -> Result<(), Error> {
        validate(&config)?;

        let mut cfg = config;
        cfg.deposit_decimals = token::TokenClient::new(&e, &cfg.deposit_token).decimals();
        cfg.debt_decimals = token::TokenClient::new(&e, &cfg.debt_token).decimals();

        storage::set_config(&e, &cfg);
        storage::set_index_state(
            &e,
            &IndexState {
                index: INDEX_SCALE,
                last_accrual: e.ledger().timestamp(),
            },
        );
        storage::set_reserve(
            &e,
            &Reserve {
                total: 0,
                committed: 0,
                outstanding: 0,
                bad_debt: 0,
            },
        );
        // No position is written — one comes into existence the first time an address deposits,
        // and `storage::position()` supplies a default whose `last_index` is live.

        events::Initialized { config: cfg }.publish(&e);
        storage::bump_shared(&e);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Collateral
    // -----------------------------------------------------------------------

    /// Risk-reducing, so never blocked — not by pause, not by a dead oracle. No price is read and
    /// no LTV is checked, because adding collateral can only improve a position.
    pub fn deposit(e: Env, owner: Address, amount: i128) -> Result<(), Error> {
        owner.require_auth();
        let mut s = load(&e, &owner)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        s.pos.deposit = math::checked_add(s.pos.deposit, amount)?;
        save(&e, &s)?;

        events::Deposited {
            owner: owner.clone(),
            amount,
            deposit_after: s.pos.deposit,
        }
        .publish(&e);

        deposit_client(&e, &s.cfg).transfer(&owner, e.current_contract_address(), &amount);
        storage::bump(&e, &owner);
        Ok(())
    }

    /// Risk-increasing: post-checks LTV against this customer's own position.
    ///
    /// # The debt-free shortcut
    ///
    /// A position with no debt has an LTV of zero at any price, so the check is vacuous. Skipping
    /// it is what lets a market whose oracle has died permanently still wind itself down: every
    /// customer can repay and then withdraw. Without it one dead feed would trap every customer's
    /// collateral at once — and with `oracle` frozen there would be no remedy.
    pub fn withdraw(e: Env, owner: Address, amount: i128) -> Result<(), Error> {
        owner.require_auth();
        let mut s = load(&e, &owner)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        accrue_into(&e, &mut s)?;
        require_unpaused(&e)?;
        if amount > s.pos.deposit {
            return Err(Error::InsufficientDeposit);
        }

        let debt_free = s.pos.principal == 0 && s.pos.interest == 0;
        let mut after = s.pos.clone();
        after.deposit = math::checked_sub(after.deposit, amount)?;

        let ltv_after_bps = if debt_free {
            0
        } else {
            let px = oracle::load(&e, &s.cfg, true)?;
            let v = health::value(&e, &s.cfg, &after, px.price, px.decimals)?;
            if !health::within_ltv(&e, &v, s.cfg.max_ltv_bps)? {
                return Err(Error::HealthTooLow);
            }
            health::ltv_bps(&e, &v)?
        };

        s.pos = after;
        save(&e, &s)?;

        events::Withdrawn {
            owner: owner.clone(),
            to: owner.clone(),
            amount,
            deposit_after: s.pos.deposit,
            ltv_after_bps,
        }
        .publish(&e);

        deposit_client(&e, &s.cfg).transfer(&e.current_contract_address(), &owner, &amount);
        storage::bump(&e, &owner);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Authority
    // -----------------------------------------------------------------------

    /// Issue this customer's spending grant.
    ///
    /// No price is read: neither the capital rule (`free >= cap`) nor the ceiling rule needs one.
    /// Whether a cap is sensible against current collateral is the customer's and the backend's
    /// judgement, and the LTV gate at `draw` enforces the real limit against a fresh price anyway.
    /// The consequence is that a line can be granted while the feed is down, and that a customer
    /// can authorise spending before depositing.
    ///
    /// # Why an existing grant is rejected rather than overwritten
    ///
    /// `Grant.drawn` lives inside the struct. Overwriting resets it to zero:
    ///
    /// ```text
    /// cap 50_000, drawn 45_000, headroom 5_000
    /// grant_authority(same terms) → drawn 0, headroom 50_000
    /// total principal creatable: 95_000 against a 50_000 cap
    /// ```
    ///
    /// Changing terms therefore takes two transactions — revoke, then grant. Slower, but there is
    /// no path by which `drawn` disappears.
    #[allow(clippy::too_many_arguments)]
    pub fn grant_authority(
        e: Env,
        owner: Address,
        operator: Address,
        settlement: Address,
        cap: i128,
        per_tx_limit: i128,
        expiry: u64,
        max_ltv_bps: u32,
    ) -> Result<(), Error> {
        owner.require_auth();
        let mut s = load(&e, &owner)?;

        let now = e.ledger().timestamp();
        if cap <= 0 || per_tx_limit <= 0 || per_tx_limit > cap || expiry <= now {
            return Err(Error::InvalidGrantParams);
        }
        // Bounded so a grant can never outlive the TTL of its own storage entry. If it did, the
        // entry could archive while the grant is live, `revoke_authority` could not read it, and
        // `committed` would be locked forever — capital the lender could never recover, for a
        // customer who may be long gone. Expressed in SECONDS: `TTL_EXTEND` is a ledger count,
        // and the two only look compatible because of the ~5s/ledger factor.
        if expiry - now > MAX_GRANT_SECONDS {
            return Err(Error::InvalidGrantParams);
        }
        // Otherwise a compromised operator simply pays itself, and the entire pinned-settlement
        // guarantee is void.
        if settlement == operator {
            return Err(Error::InvalidGrantParams);
        }

        accrue_into(&e, &mut s)?;
        require_unpaused(&e)?;

        // One grant **per operator**, not per customer. A customer may authorise an electricity
        // supplier, a card issuer and a public-services portal at once; what is still refused is a
        // second grant to the *same* operator, because overwriting one resets `drawn` to zero and
        // a cap of 50,000 with 45,000 drawn becomes a fresh 50,000 of headroom.
        if storage::grant(&e, &owner, &operator).is_some() {
            return Err(Error::GrantExists);
        }

        let mut operators = storage::grant_operators(&e, &owner);
        // Bounded, because insolvency releases every grant's capital in one call and an unbounded
        // list makes that call's cost something the customer chooses.
        if operators.len() >= MAX_GRANTS_PER_OWNER {
            return Err(Error::TooManyGrants);
        }
        if max_ltv_bps > s.cfg.max_ltv_bps {
            return Err(Error::GrantCeilingTooHigh);
        }
        // R2 and R5 are NOT checked here, and that is the substantive half of this change.
        //
        // Both read shared state — `reserve.free` and the market's total exposure — and both are
        // conditions on *committing capital*. A proposal commits none. Checking them here would
        // refuse a proposal because the reserve happens to be thin at this instant, for a line
        // the operator might not take up for another day; and passing them here would prove
        // nothing, because the reserve can drain between the proposal and the acceptance.
        //
        // They are checked in `accept_grant`, at the moment capital actually moves. R4 stays
        // here: `max_ltv_bps <= config.max_ltv_bps` depends on nothing shared, so a customer
        // proposing impossible terms learns it immediately rather than after a round trip
        // through the operator.
        save(&e, &s)?;

        put_grant(
            &e,
            &owner,
            &operator,
            Some(&Grant {
                operator: operator.clone(),
                settlement: settlement.clone(),
                cap,
                drawn: 0,
                per_tx_limit,
                expiry,
                max_ltv_bps,
                frozen: false,
                // Pending. Nothing is committed, nothing can be drawn, and the operator has not
                // agreed to anything yet.
                accepted: false,
            }),
        );
        operators.push_back(operator.clone());
        storage::set_grant_operators(&e, &owner, &operators);

        events::GrantProposed {
            owner: owner.clone(),
            operator,
            settlement,
            cap,
            per_tx_limit,
            expiry,
            max_ltv_bps,
        }
        .publish(&e);

        storage::bump(&e, &owner);
        Ok(())
    }

    /// The operator takes the authority up. **This is where capital is committed.**
    ///
    /// Signed by the operator, because the two things that make a grant unacceptable are things
    /// only they can judge:
    ///
    /// - **`settlement`.** The owner chose it and the contract could only check that it is not
    ///   the operator themselves. Whether it is an address the operator controls is unknowable
    ///   here — a registry on chain would need governance, and this market has none. An operator
    ///   that draws against a grant settling somewhere they do not control has delivered goods
    ///   and been paid to a stranger.
    /// - **`cap`.** The sum of caps granted to an operator is what moves if that operator's key
    ///   is stolen, and it may exceed a per-customer exposure limit they are obliged to keep.
    ///   A customer must not be able to enlarge either unilaterally.
    ///
    /// Neither is checkable on chain, and that is exactly why the decision belongs to the party
    /// that can check them off chain and then sign.
    ///
    /// Not blocked by pause: accepting commits capital but creates no debt, and the pause exists
    /// to stop debt growing.
    pub fn accept_grant(e: Env, owner: Address, operator: Address) -> Result<(), Error> {
        operator.require_auth();
        let mut s = load(&e, &owner)?;

        let mut g = load_grant(&e, &owner, &operator)?;
        if g.accepted {
            return Err(Error::GrantAlreadyAccepted);
        }
        // An expired proposal is not acceptable. Without this an operator could sit on one until
        // it lapsed and take it up afterwards, which is a line the customer believes is gone.
        if e.ledger().timestamp() >= g.expiry {
            return Err(Error::GrantExpired);
        }

        accrue_into(&e, &mut s)?;

        // Rule R2, and the reason the deck can promise that a card transaction is never declined
        // because somebody else borrowed first: capital is reserved for this line, out of the
        // shared pool, at the moment the line becomes usable.
        if s.res.free() < g.cap {
            return Err(Error::ReserveUnderfunded);
        }
        // Rule R5, market-wide.
        //
        // The exposure is rescaled to the USD scale before the comparison. `committed`,
        // `outstanding` and `cap` are debt-token units; `vault_cap` — like `dust_threshold` — is
        // a USD figure. The two coincide only when `debt_decimals == 7`, and comparing them raw
        // silently moved the market's ceiling by a power of ten for any other token.
        //
        // `debt_value_usd` rounds up, which is the conservative direction for a ceiling.
        let exposure = math::checked_add(
            math::checked_add(s.res.committed, s.res.outstanding)?,
            g.cap,
        )?;
        if health::debt_value_usd(&e, exposure, &s.cfg)? > s.cfg.vault_cap {
            return Err(Error::ExposureCapExceeded);
        }

        s.res.committed = math::checked_add(s.res.committed, g.cap)?;
        g.accepted = true;
        save(&e, &s)?;
        put_grant(&e, &owner, &operator, Some(&g));

        events::GrantAccepted {
            owner: owner.clone(),
            operator,
            settlement: g.settlement,
            cap: g.cap,
            per_tx_limit: g.per_tx_limit,
            expiry: g.expiry,
            max_ltv_bps: g.max_ltv_bps,
            committed_after: s.res.committed,
        }
        .publish(&e);

        storage::bump(&e, &owner);
        Ok(())
    }

    /// The operator declines. Deletes the proposal; commits and releases nothing.
    ///
    /// The customer could revoke it themselves and expiry would reap it eventually, so this
    /// exists for one reason: it tells them **why** nothing happened. "Declined" and "still
    /// waiting" look identical from the outside, and a customer who cannot tell them apart waits
    /// for something that is never coming.
    ///
    /// Only a pending grant. Once accepted, an operator who wants out has no unilateral exit —
    /// capital is committed against the line and the customer may have drawn on it. That is the
    /// customer's to end, with `revoke_authority`.
    pub fn reject_grant(e: Env, owner: Address, operator: Address) -> Result<(), Error> {
        operator.require_auth();
        let s = load(&e, &owner)?;

        let g = load_grant(&e, &owner, &operator)?;
        if g.accepted {
            return Err(Error::GrantAlreadyAccepted);
        }
        put_grant(&e, &owner, &operator, None);

        events::GrantRejected {
            owner: owner.clone(),
            operator,
            cap: g.cap,
            // Unchanged, and carried so a reducer never has to infer that a rejection moved no
            // capital. It could derive it; being told is cheaper than being right by reasoning.
            committed_after: s.res.committed,
        }
        .publish(&e);

        storage::bump(&e, &owner);
        Ok(())
    }

    /// The fire alarm.
    ///
    /// Reads no price, is not blocked by pause, and has no failure condition beyond "no grant" and
    /// "not this customer". A fire alarm a dead oracle can disable is not a fire alarm.
    ///
    /// Calling twice is safe: the second call finds nothing and releases no capital a second time
    /// — which is why the entry is deleted rather than flagged.
    pub fn revoke_authority(e: Env, owner: Address, operator: Address) -> Result<(), Error> {
        owner.require_auth();
        let mut s = load(&e, &owner)?;

        // Named, because a customer may hold several. Revoking the electricity supplier must not
        // touch the card.
        let g = load_grant(&e, &owner, &operator)?;
        let released = grant::release(&g, &mut s.res)?;
        save(&e, &s)?;
        put_grant(&e, &owner, &operator, None);

        events::GrantRevoked {
            owner: owner.clone(),
            operator: g.operator,
            cap: g.cap,
            drawn: g.drawn,
            released,
            committed_after: s.res.committed,
        }
        .publish(&e);

        storage::bump(&e, &owner);
        Ok(())
    }

    /// Stop one operator drawing, without destroying the grant.
    ///
    /// The lender may call it as well as the customer — they bear the risk too and may spot an
    /// anomaly first. One customer has no say over another's grant. Revoke is the blunt
    /// alternative: it destroys `cap`, `drawn` and `settlement` and forces the customer to
    /// re-sign from scratch, and a user made to do that over false alarms stops responding to
    /// alarms.
    pub fn freeze_grant(
        e: Env,
        owner: Address,
        operator: Address,
        by: Address,
    ) -> Result<(), Error> {
        let cfg = storage::config(&e)?;
        storage::require_owner_or_lender(&cfg, &owner, &by)?;

        let mut g = load_grant(&e, &owner, &operator)?;
        // Nothing to stop. A pending grant cannot draw, so freezing one would record a state that
        // changes nothing and reads as though a live line had been halted.
        if !g.accepted {
            return Err(Error::GrantNotAccepted);
        }
        g.frozen = true;
        put_grant(&e, &owner, &operator, Some(&g));

        events::GrantFrozen {
            owner: owner.clone(),
            operator,
            by,
        }
        .publish(&e);
        storage::bump(&e, &owner);
        Ok(())
    }

    /// Customer-only and blocked by pause, because reopening the ability to borrow is
    /// risk-increasing.
    ///
    /// Reads no price: unfreezing only restores the operator's ability to *attempt* a draw, and
    /// the draw itself still passes every LTV gate with a fresh price. Requiring one here would
    /// let a dead oracle block a purely administrative action that cannot increase debt.
    pub fn unfreeze_grant(e: Env, owner: Address, operator: Address) -> Result<(), Error> {
        owner.require_auth();
        require_unpaused(&e)?;

        let mut g = load_grant(&e, &owner, &operator)?;
        // Symmetric with freeze: a pending grant was never stopped, so there is nothing to
        // restart. The operator's move is `accept_grant`.
        if !g.accepted {
            return Err(Error::GrantNotAccepted);
        }
        g.frozen = false;
        put_grant(&e, &owner, &operator, Some(&g));

        events::GrantUnfrozen {
            owner: owner.clone(),
            operator,
        }
        .publish(&e);
        storage::bump(&e, &owner);
        Ok(())
    }

    /// Release the commitment behind an expired grant. Permissionless.
    ///
    /// Without it, an expired grant still locks `committed` — nothing runs when time passes — and
    /// only that customer can revoke. If they disappear, the lender's capital is locked forever,
    /// and in a market vault it is capital that would otherwise back somebody else's line.
    /// Permissionless is safe because the precondition is objective: anyone checking gets the same
    /// answer, and the lender has the incentive to call it.
    pub fn reap_expired(e: Env, owner: Address, operator: Address) -> Result<(), Error> {
        let mut s = load(&e, &owner)?;
        let g = load_grant(&e, &owner, &operator)?;
        if e.ledger().timestamp() < g.expiry {
            return Err(Error::GrantNotExpired);
        }

        let released = grant::release(&g, &mut s.res)?;
        save(&e, &s)?;
        put_grant(&e, &owner, &operator, None);

        events::GrantReaped {
            owner: owner.clone(),
            operator: g.operator,
            cap: g.cap,
            drawn: g.drawn,
            released,
            committed_after: s.res.committed,
        }
        .publish(&e);

        storage::bump(&e, &owner);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Capital — shared by the whole market
    // -----------------------------------------------------------------------

    pub fn fund_reserve(e: Env, amount: i128) -> Result<(), Error> {
        let cfg = storage::config(&e)?;
        cfg.lender.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let before = storage::reserve(&e);
        let mut res = before.clone();
        res.total = math::checked_add(res.total, amount)?;
        invariant::check_shared(&before, &res, &storage::index_state(&e))?;
        storage::set_reserve(&e, &res);

        events::ReserveFunded {
            amount,
            total_after: res.total,
        }
        .publish(&e);

        debt_client(&e, &cfg).transfer(&cfg.lender, e.current_contract_address(), &amount);
        storage::bump_shared(&e);
        Ok(())
    }

    /// Not blocked by pause: invariant I1 already prevents withdrawing committed capital, and
    /// blocking further would only lock the lender's legitimate funds without adding safety.
    ///
    /// **This is the one function that asserts I2 on chain** — that the contract's debt-token
    /// balance actually backs the undisbursed capital of the whole market. It costs a
    /// cross-contract call, and this is the single place where a discrepancy turns directly into a
    /// loss.
    pub fn withdraw_reserve(e: Env, amount: i128) -> Result<(), Error> {
        let cfg = storage::config(&e)?;
        cfg.lender.require_auth();
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let before = storage::reserve(&e);
        // `checked_add`, not `+`: these are balances, and the no-bare-arithmetic rule exists
        // precisely so a reviewer grepping this file finds nothing to check by hand.
        let locked = math::checked_add(before.committed, before.outstanding)?;
        if math::checked_sub(before.total, amount)? < locked {
            return Err(Error::ReserveLocked);
        }

        let mut res = before.clone();
        res.total = math::checked_sub(res.total, amount)?;
        invariant::check_shared(&before, &res, &storage::index_state(&e))?;
        storage::set_reserve(&e, &res);

        // I2: balance >= total − outstanding.
        let bal = debt_client(&e, &cfg).balance(&e.current_contract_address());
        if bal < math::checked_sub(res.total, res.outstanding)? {
            return Err(Error::InvariantViolated);
        }

        events::ReserveWithdrawn {
            amount,
            total_after: res.total,
            free_after: res.free(),
        }
        .publish(&e);

        debt_client(&e, &cfg).transfer(&e.current_contract_address(), &cfg.lender, &amount);
        storage::bump_shared(&e);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Debt
    // -----------------------------------------------------------------------

    /// The only function that creates debt.
    ///
    /// # The signature has no destination
    ///
    /// It reads `grant.settlement` from that customer's own grant instead. That turns a business
    /// rule into a property of the type: there is no way to write the call such that funds reach
    /// anywhere else, so a compromised operator can still only move money to the addresses each
    /// customer pinned, and only up to each customer's own cap.
    ///
    /// # On the step ordering
    ///
    /// `accrue` runs *after* the cheap gates rather than at step 3, deliberately: every gate above
    /// it is a storage read, so a rejected draw never pays for accrual or for a cross-contract
    /// oracle call. The consequence is that the position loaded at step 4 is pre-accrual, so the
    /// LTV check below uses the **post-accrual** copy — reading the stale one would compute LTV
    /// against yesterday's interest.
    ///
    /// A draw preserves invariant I1 as an *equality*: `committed` falls by exactly what
    /// `outstanding` rises by. Capital changes drawer without leaving the market.
    pub fn draw(
        e: Env,
        owner: Address,
        operator: Address,
        draw_id: BytesN<32>,
        amount: i128,
    ) -> Result<(), Error> {
        operator.require_auth();
        let mut s = load(&e, &owner)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        require_unpaused(&e)?;

        // Keyed by this operator, so a customer's other grants are neither read nor touched. A
        // draw against the electricity supplier's authority cannot spend the card's cap.
        let g = load_grant(&e, &owner, &operator)?;
        grant::cheap_gates(&g, &operator, amount, e.ledger().timestamp())?;
        // Checked before the price read: a Temporary lookup is far cheaper than a cross-contract
        // call, and a duplicate is not worth paying oracle gas to discover.
        if idempotency::seen(&e, &draw_id) {
            return Err(Error::DuplicateDrawId);
        }

        accrue_into(&e, &mut s)?;

        let px = oracle::load(&e, &s.cfg, true)?;
        let mut after = s.pos.clone();
        after.principal = math::checked_add(after.principal, amount)?;
        let v = health::value(&e, &s.cfg, &after, px.price, px.decimals)?;

        // The grant's own ceiling first, then the market's: the backend reacts differently to the
        // two, so the more specific code must win. Since `grant.max_ltv <= cfg.max_ltv` always
        // holds — code 46 enforces it at grant time — the second check is a defensive backstop
        // that should be unreachable.
        if !health::within_ltv(&e, &v, g.max_ltv_bps)? {
            return Err(Error::LtvCeilingExceeded);
        }
        if !health::within_ltv(&e, &v, s.cfg.max_ltv_bps)? {
            return Err(Error::HealthTooLow);
        }
        // Should be unreachable: `committed` was credited exactly `cap` at grant time and the cap
        // gate above already bounds `drawn + amount`. If it ever fires there is an accounting
        // bug — keep it as a defensive assert.
        if s.res.committed < amount {
            return Err(Error::ReserveUnderfunded);
        }

        idempotency::mark(&e, &draw_id);
        let mut ng = g.clone();
        ng.drawn = math::checked_add(ng.drawn, amount)?;
        s.pos = after;
        // `committed` moves incrementally, exactly as it did with one grant — no sum over the
        // customer's grants, so the cost of a draw does not grow with how many providers they use.
        s.res.committed = math::checked_sub(s.res.committed, amount)?;
        s.res.outstanding = math::checked_add(s.res.outstanding, amount)?;
        save(&e, &s)?;
        put_grant(&e, &owner, &operator, Some(&ng));

        events::Drawn {
            owner: owner.clone(),
            draw_id,
            operator,
            settlement: g.settlement.clone(),
            amount,
            drawn_after: ng.drawn,
            principal_after: s.pos.principal,
            committed_after: s.res.committed,
            outstanding_after: s.res.outstanding,
        }
        .publish(&e);

        debt_client(&e, &s.cfg).transfer(&e.current_contract_address(), &g.settlement, &amount);
        storage::bump(&e, &owner);
        Ok(())
    }

    /// Open to anyone — refunds and chargebacks arrive from the card programme's treasury, not the
    /// customer's wallet, and a third party paying down somebody's debt can only help them.
    ///
    /// Interest is paid first, then principal. Only the **principal** portion reopens the grant's
    /// headroom; interest does not, because `cap` is a ceiling on principal and a line that shrank
    /// as interest accrued would be unpredictable for the customer.
    pub fn repay(
        e: Env,
        owner: Address,
        from: Address,
        amount: i128,
        // Which authority's headroom reopens. `None` repays the debt and reopens nothing — right
        // for a third party who has no view of the customer's arrangements, and for principal
        // whose grant has already been revoked.
        //
        // Naming it is how several grants become possible without a policy: the alternative is
        // deciding, inside the contract, whether a repayment reopens the oldest line or the
        // largest or the one drawn most recently, and every answer needs a loop on the money path.
        operator: Option<Address>,
    ) -> Result<(), Error> {
        from.require_auth();
        let mut s = load(&e, &owner)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        accrue_into(&e, &mut s)?;

        if health::debt_raw(&s.pos)? <= 0 {
            return Err(Error::NoDebt);
        }
        let (interest_paid, principal_paid) = interest::split(amount, &s.pos);
        if principal_paid > s.pos.principal {
            return Err(Error::InvalidAmount);
        }

        s.pos.interest = math::checked_sub(s.pos.interest, interest_paid)?;
        s.pos.principal = math::checked_sub(s.pos.principal, principal_paid)?;
        // Interest belongs to the lender, and enters the books only when actually paid —
        // accrued-but-unpaid interest is unrealised revenue, not capital, so it needs no backing.
        s.res.total = math::checked_add(s.res.total, interest_paid)?;
        s.res.outstanding = math::checked_sub(s.res.outstanding, principal_paid)?;

        let mut restored = 0;
        let mut reopened: Option<(Address, Grant)> = None;
        if let Some(op) = operator.clone() {
            // A named grant that no longer exists reopens nothing, and the repayment still lands.
            //
            // The tempting alternative is `GrantNotFound`, so a payer expecting a line back learns
            // it did not come back. It is the wrong trade. Repayment is the risk-reducing action,
            // and the contract guarantees it is never blocked — the same rule that keeps `repay`
            // off the oracle. Reverting a real repayment because an *authority* vanished leaves a
            // customer owing money they tried to pay, for a reason that has nothing to do with the
            // debt; a standing instruction repaying nightly would simply start failing the day the
            // customer revoked, and the position would drift toward liquidation.
            //
            // The payer is still told: `headroom_restored` is 0 in the `Repaid` event.
            if let Some(mut g) = storage::grant(&e, &owner, &op) {
                restored = grant::restore_headroom(&mut g, &mut s.res, principal_paid)?;
                reopened = Some((op, g));
            }
        }
        save(&e, &s)?;
        if let Some((op, g)) = &reopened {
            put_grant(&e, &owner, op, Some(g));
        }

        events::Repaid {
            owner: owner.clone(),
            from: from.clone(),
            // What the payer named, not what was found: a name that matched no grant restored
            // nothing, and the pair (named, 0) is the honest record of that.
            operator: operator.clone(),
            amount,
            interest_paid,
            principal_paid,
            headroom_restored: restored,
            principal_after: s.pos.principal,
            interest_after: s.pos.interest,
        }
        .publish(&e);

        debt_client(&e, &s.cfg).transfer(&from, e.current_contract_address(), &amount);
        storage::bump(&e, &owner);
        Ok(())
    }

    /// Advance the market clock, settle one customer, and refresh their storage. Permissionless.
    ///
    /// The permissionless part matters more here than it did in a single-customer vault: this is
    /// the cheap way to keep somebody else's position from archiving, and `bump` only touches the
    /// position it was given.
    ///
    /// **Never reads the price.** `repay` runs this at step 3; if it touched the oracle, a dead
    /// oracle would block `repay`, and risk-reducing actions are never blocked. Interest does not
    /// depend on price, so the rule costs nothing.
    pub fn accrue(e: Env, owner: Address) -> Result<(), Error> {
        let mut s = load(&e, &owner)?;
        accrue_into(&e, &mut s)?;
        save(&e, &s)?;
        storage::bump(&e, &owner);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Liquidation
    // -----------------------------------------------------------------------

    /// Permissionless. Returns the deposit-token amount seized.
    ///
    /// Not blocked by pause: a pause is normally triggered by an incident at the card or backend
    /// layer, and letting it block liquidation would convert an operational incident into capital
    /// risk for the lender — now across every customer at once.
    pub fn liquidate(
        e: Env,
        owner: Address,
        liquidator: Address,
        repay_amount: i128,
        min_deposit_out: i128,
    ) -> Result<i128, Error> {
        liquidator.require_auth();
        let mut s = load(&e, &owner)?;
        if repay_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        accrue_into(&e, &mut s)?;
        if health::debt_raw(&s.pos)? <= 0 {
            return Err(Error::NoDebt);
        }

        let px = oracle::load(&e, &s.cfg, true)?;
        oracle::require_not_in_grace(&e)?;
        let v = health::value(&e, &s.cfg, &s.pos, px.price, px.decimals)?;
        if !health::is_liquidatable(&e, &v, &s.cfg)? {
            return Err(Error::PositionHealthy);
        }
        let ltv_before_bps = health::ltv_bps(&e, &v)?;

        if liquidation::is_insolvent(&v) {
            return Self::wipe_out(e, s, v, px, liquidator, min_deposit_out, ltv_before_bps);
        }

        let repay_usd = health::debt_value_usd(&e, repay_amount, &s.cfg)?;
        if repay_usd > liquidation::max_repay(&e, &s.cfg, &v)? {
            return Err(Error::CloseFactorExceeded);
        }
        let seize_usd = liquidation::seize_value(&e, &s.cfg, repay_usd)?;
        let seize = math::min_i128(
            health::from_usd(&e, seize_usd, &s.cfg, px.price, px.decimals)?,
            s.pos.deposit,
        );
        if seize < min_deposit_out {
            return Err(Error::SlippageExceeded);
        }

        let (interest_paid, principal_paid) = interest::split(repay_amount, &s.pos);
        if principal_paid > s.pos.principal {
            return Err(Error::InvalidAmount);
        }

        s.pos.interest = math::checked_sub(s.pos.interest, interest_paid)?;
        s.pos.principal = math::checked_sub(s.pos.principal, principal_paid)?;
        s.pos.deposit = math::checked_sub(s.pos.deposit, seize)?;
        // `total` is NOT reduced here: the liquidator's tokens arrived, so the lender's claim is
        // intact. That single line is the whole difference from `lender_liquidate`.
        s.res.total = math::checked_add(s.res.total, interest_paid)?;
        s.res.outstanding = math::checked_sub(s.res.outstanding, principal_paid)?;
        // **No headroom is restored.** A liquidator is a stranger to the customer's arrangements
        // and cannot say which of several authorities should reopen — and a forced repayment
        // reopening somebody's spending power is not obviously right even if it could. Both
        // `committed` and `outstanding` stay put, so invariant I1 is unaffected.
        save(&e, &s)?;

        let after = health::value(&e, &s.cfg, &s.pos, px.price, px.decimals)?;
        events::Liquidated {
            owner: owner.clone(),
            liquidator: liquidator.clone(),
            by_lender: false,
            repay_amount,
            interest_paid,
            principal_paid,
            seized: seize,
            ltv_before_bps,
            ltv_after_bps: health::ltv_bps(&e, &after)?,
            principal_after: s.pos.principal,
            deposit_after: s.pos.deposit,
        }
        .publish(&e);

        debt_client(&e, &s.cfg).transfer(&liquidator, e.current_contract_address(), &repay_amount);
        deposit_client(&e, &s.cfg).transfer(&e.current_contract_address(), &liquidator, &seize);
        storage::bump(&e, &owner);
        Ok(seize)
    }

    /// The lender of last resort: **no close factor, no bonus, and no debt tokens move in.**
    ///
    /// They net internally — receiving collateral and writing down their own claim by the same
    /// amount. If `total` did not fall, the lender would take the collateral *and* keep the full
    /// claim, collecting twice; invariant I2 would catch it on the next assertion, but only after
    /// the fact.
    ///
    /// # Two bounds the specification originally lacked
    ///
    /// Dropping the close factor removes the only thing that limited `repay_amount` on this route,
    /// and `total -= principal_part` would then drive the claim negative — surfacing as
    /// `InvariantViolated`, a code that is supposed to mean *there is a bug*. So the repayment is
    /// clamped to the debt.
    ///
    /// The insolvency guard is the second. This route has no wipe-out branch: it nets, so there
    /// are no incoming tokens to measure a recovery against and the write-down would never be
    /// booked. Rejecting an insolvent position forces the lender through ordinary `liquidate`,
    /// where `bad_debt` is recorded properly.
    pub fn lender_liquidate(e: Env, owner: Address, repay_amount: i128) -> Result<i128, Error> {
        let mut s = load(&e, &owner)?;
        s.cfg.lender.require_auth();
        if repay_amount <= 0 {
            return Err(Error::InvalidAmount);
        }
        accrue_into(&e, &mut s)?;
        if health::debt_raw(&s.pos)? <= 0 {
            return Err(Error::NoDebt);
        }

        let px = oracle::load(&e, &s.cfg, true)?;
        oracle::require_not_in_grace(&e)?;
        let v = health::value(&e, &s.cfg, &s.pos, px.price, px.decimals)?;
        if !health::is_liquidatable(&e, &v, &s.cfg)? {
            return Err(Error::PositionHealthy);
        }
        if liquidation::is_insolvent(&v) {
            return Err(Error::PositionHealthy);
        }
        if repay_amount > health::debt_raw(&s.pos)? {
            return Err(Error::InvalidAmount);
        }
        let ltv_before_bps = health::ltv_bps(&e, &v)?;

        let repay_usd = health::debt_value_usd(&e, repay_amount, &s.cfg)?;
        let seize_usd = liquidation::seize_value_lender(repay_usd);
        let seize = math::min_i128(
            health::from_usd(&e, seize_usd, &s.cfg, px.price, px.decimals)?,
            s.pos.deposit,
        );

        let (interest_paid, principal_paid) = interest::split(repay_amount, &s.pos);
        s.pos.interest = math::checked_sub(s.pos.interest, interest_paid)?;
        s.pos.principal = math::checked_sub(s.pos.principal, principal_paid)?;
        s.pos.deposit = math::checked_sub(s.pos.deposit, seize)?;
        // `total` falls by the principal written off, and **is not credited the interest**.
        //
        // Every other route books `total += interest_paid` because the payer's debt tokens
        // arrived and the pool really did grow. Here nothing arrives: the lender collected the
        // whole `repay_amount`, interest included, as **collateral**. Crediting the interest to
        // the pool as well would claim backing that does not exist, and invariant I2 —
        // `balance >= total − outstanding` — would fail by exactly `interest_paid` on the next
        // assertion.
        s.res.total = math::checked_sub(s.res.total, principal_paid)?;
        s.res.outstanding = math::checked_sub(s.res.outstanding, principal_paid)?;
        // Nothing restored, for the same reason as `liquidate`. This route used to net `cap` and
        // `drawn` down instead, because restoring headroom here would credit `committed` while
        // `total` fell and break I1. With nothing restored, neither figure moves and I1 holds by
        // construction — so the netting variant is gone rather than generalised to several grants.
        save(&e, &s)?;

        let after = health::value(&e, &s.cfg, &s.pos, px.price, px.decimals)?;
        events::Liquidated {
            owner: owner.clone(),
            liquidator: s.cfg.lender.clone(),
            by_lender: true,
            repay_amount,
            interest_paid,
            principal_paid,
            seized: seize,
            ltv_before_bps,
            ltv_after_bps: health::ltv_bps(&e, &after)?,
            principal_after: s.pos.principal,
            deposit_after: s.pos.deposit,
        }
        .publish(&e);

        // The ONLY transfer. No debt token moves.
        deposit_client(&e, &s.cfg).transfer(&e.current_contract_address(), &s.cfg.lender, &seize);
        storage::bump(&e, &owner);
        Ok(seize)
    }

    // -----------------------------------------------------------------------
    // The brake
    // -----------------------------------------------------------------------

    /// Lender only, and it **lapses by itself** after `MAX_PAUSE_SECONDS`.
    ///
    /// In a single-customer vault the brake was safe because it was asymmetric: either party could
    /// pull it, both had to agree to release it, so neither could hold the other's collateral
    /// hostage. A market vault has no such pair — thousands of customers cannot co-sign — so only
    /// the lender is left, and an unbounded lender-only pause would let one address freeze every
    /// customer's withdrawal indefinitely.
    ///
    /// Seven days makes it an incident window — but it bounds **inattention, not the lender**.
    /// Calling again during a pause restarts the clock, so a lender who means to keep the market
    /// shut can renew weekly. That is deliberate for the case it was designed for — an incident
    /// still live deserves an explicit renewal, on the record, rather than silent continuation —
    /// and it does mean the expiry is not a ceiling on the lender's power. Accepted and recorded
    /// in `docs/13-deck-conformance.md`; a cooldown between pauses is the fix if it is ever
    /// wanted.
    ///
    /// # A deliberate exception to the write discipline
    ///
    /// This reads no price, **asserts no invariants**, makes no external call, and does not bump.
    /// With upgrades disabled it is the only escape from a discovered bug, so it has to work when
    /// the oracle is dead **and when an invariant is already violated** — asserting here would
    /// make the brake fail exactly when it is needed.
    pub fn pause(e: Env) -> Result<(), Error> {
        let cfg = storage::config(&e)?;
        cfg.lender.require_auth();
        let at = e.ledger().timestamp();
        storage::set_paused_at(&e, at);
        events::Paused {
            by: cfg.lender,
            at,
            until: at.saturating_add(MAX_PAUSE_SECONDS),
        }
        .publish(&e);
        Ok(())
    }

    /// Lift the brake early. The lender can only ever shorten a pause, never extend one past its
    /// bound.
    pub fn unpause(e: Env) -> Result<(), Error> {
        let cfg = storage::config(&e)?;
        cfg.lender.require_auth();
        storage::clear_pause(&e);
        events::Unpaused {
            at: e.ledger().timestamp(),
        }
        .publish(&e);
        storage::bump_shared(&e);
        Ok(())
    }

    /// Record an outage or a recovery. **Never panics, never errors on a bad price.**
    ///
    /// This must be a separate entry point rather than a side effect of the ordinary price path,
    /// because that path *reverts* on a bad price — the state write recording the outage would be
    /// rolled back along with the failed call.
    ///
    /// The grace window it opens exists because a stale price blocks liquidation, but nothing said
    /// what happens when the feed returns: without it, the oracle dies for four hours, the market
    /// moves 40%, and the instant it recovers every over-threshold position in the market is
    /// liquidated at a price nobody had a chance to react to.
    ///
    /// Permissionless because both sides want it called — customers want the window recorded,
    /// liquidators want it to expire. Nobody has to be trusted.
    ///
    /// Like [`CollateralVault::pause`], it skips the invariant assert and the bump: giving it a
    /// way to fail would mean the outage goes unrecorded precisely during an incident.
    pub fn report_oracle_state(e: Env) -> Result<bool, Error> {
        let cfg = storage::config(&e)?;
        let (fresh, change) = oracle::update_guard(&e, &cfg);
        match change {
            oracle::GuardChange::OutageStarted(at) => events::OracleOutage { at }.publish(&e),
            oracle::GuardChange::Recovered(grace_until) => {
                events::OracleRecovered { grace_until }.publish(&e)
            }
            oracle::GuardChange::None => {}
        }
        Ok(fresh)
    }

    // -----------------------------------------------------------------------
    // Views — never panic on a stale price
    // -----------------------------------------------------------------------

    pub fn get_config(e: Env) -> Result<Config, Error> {
        storage::config(&e)
    }

    /// True while a pause is in force. A pause that has run past `MAX_PAUSE_SECONDS` reports
    /// `false` here without anybody having called `unpause`.
    pub fn is_paused(e: Env) -> bool {
        storage::paused(&e)
    }

    /// When the current pause was pulled, or `None`. Add `MAX_PAUSE_SECONDS` for when it lapses;
    /// the `Paused` event carries that figure directly.
    pub fn paused_at(e: Env) -> Option<u64> {
        storage::paused_at(&e)
    }

    /// One grant, named by its operator.
    ///
    /// A customer may hold several — one per provider — so the operator is part of the question.
    pub fn get_grant(e: Env, owner: Address, operator: Address) -> Option<Grant> {
        storage::grant(&e, &owner, &operator)
    }

    /// Which operators this customer has live grants with.
    ///
    /// Bounded by `MAX_GRANTS_PER_OWNER`, so reading it is a constant. It exists for the
    /// insolvency sweep; a backend learns the same thing from the event stream, which is why
    /// nothing on the money path consults it.
    pub fn get_grant_operators(e: Env, owner: Address) -> soroban_sdk::Vec<Address> {
        storage::grant_operators(&e, &owner)
    }

    /// The market's capital, not any one customer's.
    pub fn get_reserve(e: Env) -> Result<ReserveView, Error> {
        let r = storage::reserve(&e);
        Ok(ReserveView {
            total: r.total,
            committed: r.committed,
            outstanding: r.outstanding,
            free: r.free(),
            bad_debt: r.bad_debt,
        })
    }

    /// Everything one customer's position screen needs, in one call.
    ///
    /// Takes the **non-strict** oracle path: a screen has to render during an outage, so a missing
    /// or stale price yields a zeroed valuation and a flag rather than a trap.
    pub fn get_position(e: Env, owner: Address) -> Result<PositionView, Error> {
        let s = load(&e, &owner)?;
        let mut pos = s.pos.clone();
        let mut idx = s.idx.clone();
        interest::advance(&e, &s.cfg, &mut idx)?;
        interest::settle(&e, &mut pos, idx.index)?;

        let px = oracle::load(&e, &s.cfg, false)?;
        let v = health::value(&e, &s.cfg, &pos, px.price, px.decimals)?;
        Ok(PositionView {
            deposit: pos.deposit,
            principal: pos.principal,
            interest: pos.interest,
            debt: health::debt_raw(&pos)?,
            market_value: v.market,
            eligible_value: v.eligible,
            debt_value: v.debt,
            ltv_bps: health::ltv_bps(&e, &v)?,
            health_bps: health::health_bps(&e, &v, &s.cfg)?,
            liq_price: health::liq_price(&e, &s.cfg, &pos, &v, px.decimals)?,
            band: health::band(&e, &v, &s.cfg)?,
            price_age: px.age,
            price_stale: px.stale,
        })
    }

    /// Would this draw pass, and if not — **which code**.
    ///
    /// Without it, callers discover the reason by submitting failing transactions, paying fees to
    /// learn something the contract already knows.
    ///
    /// Returns both ceilings rather than one number. They move independently: `cap_headroom` only
    /// changes when this customer draws or repays, `ltv_headroom` changes with every price tick.
    /// The interface must say which is biting, because the remedies are opposite — repay versus
    /// deposit more.
    pub fn quote_draw(
        e: Env,
        owner: Address,
        operator: Address,
        amount: i128,
    ) -> Result<DrawQuote, Error> {
        let s = load(&e, &owner)?;
        let mut pos = s.pos.clone();
        let mut idx = s.idx.clone();
        interest::advance(&e, &s.cfg, &mut idx)?;
        interest::settle(&e, &mut pos, idx.index)?;

        let px = oracle::load(&e, &s.cfg, false)?;
        // This operator's grant. A quote is always about one authority: the cap that binds is the
        // one belonging to whoever is asking whether they may draw.
        let quoted = storage::grant(&e, &owner, &operator);
        let cap_headroom = quoted.as_ref().map(grant::cap_headroom).unwrap_or(0);

        let mut after = pos.clone();
        after.principal = math::checked_add(after.principal, amount)?;
        let v = health::value(&e, &s.cfg, &after, px.price, px.decimals)?;
        let v_now = health::value(&e, &s.cfg, &pos, px.price, px.decimals)?;

        let ceiling = match &quoted {
            Some(g) => math::min_i128(g.max_ltv_bps as i128, s.cfg.max_ltv_bps as i128) as u32,
            None => s.cfg.max_ltv_bps,
        };
        let ltv_headroom = if px.stale {
            0
        } else {
            health::ltv_headroom(&e, &s.cfg, &v_now, ceiling)?
        };

        let blocked = Self::draw_block_reason(&e, &s, &v, &px, operator, amount)?;
        let binding = if blocked == Some(Error::CapExceeded as u32) {
            Binding::Cap
        } else if blocked == Some(Error::LtvCeilingExceeded as u32)
            || blocked == Some(Error::HealthTooLow as u32)
        {
            Binding::Ltv
        } else if cap_headroom < ltv_headroom {
            Binding::Cap
        } else if ltv_headroom < cap_headroom {
            Binding::Ltv
        } else {
            Binding::None
        };

        Ok(DrawQuote {
            allowed: blocked.is_none(),
            blocked_reason: blocked,
            cap_headroom,
            ltv_headroom,
            binding,
            ltv_after_bps: health::ltv_bps(&e, &v)?,
            price_age: px.age,
        })
    }

    /// Liquidatable, how much, for how much collateral — and **which code** blocks it.
    ///
    /// Liquidation is permissionless, so a third party has to be able to evaluate a position
    /// without our infrastructure. A liquidator who discovers the blocking reason only by burning a
    /// transaction is a liquidator who stops trying.
    pub fn quote_liquidation(e: Env, owner: Address) -> Result<LiquidationQuote, Error> {
        let s = load(&e, &owner)?;
        let mut pos = s.pos.clone();
        let mut idx = s.idx.clone();
        interest::advance(&e, &s.cfg, &mut idx)?;
        interest::settle(&e, &mut pos, idx.index)?;

        let px = oracle::load(&e, &s.cfg, false)?;
        let v = health::value(&e, &s.cfg, &pos, px.price, px.decimals)?;
        let liquidatable = !px.stale && health::is_liquidatable(&e, &v, &s.cfg)?;

        let max_repay_usd = if v.debt > 0 {
            liquidation::max_repay(&e, &s.cfg, &v)?
        } else {
            0
        };
        let max_repay = liquidation::usd_to_debt(&e, &s.cfg, max_repay_usd)?;
        let seize_for_max_repay = if px.stale || max_repay_usd == 0 {
            0
        } else {
            let sv = liquidation::seize_value(&e, &s.cfg, max_repay_usd)?;
            math::min_i128(
                health::from_usd(&e, sv, &s.cfg, px.price, px.decimals)?,
                pos.deposit,
            )
        };

        let guard = storage::oracle_guard(&e);
        let blocked = if health::debt_raw(&pos)? <= 0 {
            Some(Error::NoDebt as u32)
        } else if px.stale {
            Some(Error::PriceStale as u32)
        } else if e.ledger().timestamp() < guard.grace_until {
            Some(Error::LiquidationGrace as u32)
        } else if !liquidatable {
            Some(Error::PositionHealthy as u32)
        } else {
            None
        };

        Ok(LiquidationQuote {
            liquidatable,
            ltv_bps: health::ltv_bps(&e, &v)?,
            max_repay,
            seize_for_max_repay,
            blocked_reason: blocked,
        })
    }
}

// ---------------------------------------------------------------------------
// helpers that need the contract type
// ---------------------------------------------------------------------------

impl CollateralVault {
    /// The insolvency branch. **Must never revert for a reason internal to the contract.**
    ///
    /// A subtraction below zero, a failed assert, an unchecked panic — any of those leaves the
    /// position stuck forever: nobody can close it, the collateral cannot be withdrawn, the debt
    /// cannot be cleared. That is the single most serious mistake available here, which is why
    /// every clamp below is explicit rather than inherited.
    ///
    /// Three things the deck's sketch omits, all of them load-bearing:
    ///
    /// 1. **`total -= lost`.** Without it the books still record a lender claim on money that is
    ///    genuinely gone. Invariant I1 still passes — `outstanding` fell too — so only I2 sees the
    ///    error. That is precisely why I2 exists.
    /// 2. **Every grant is released, and none has its headroom restored first.** Restoring would
    ///    be the same arithmetic by a longer route — `drawn` to zero, `committed` up by that
    ///    amount, `released` then the whole `cap` — so `grant::release` is called directly and
    ///    `restore_headroom` keeps its single call site in `repay`.
    /// 3. **The trigger is market value, not eligible value** — see `liquidation::is_insolvent`.
    #[allow(clippy::too_many_arguments)]
    fn wipe_out(
        e: Env,
        mut s: State,
        v: health::Valuation,
        px: oracle::Prices,
        liquidator: Address,
        min_deposit_out: i128,
        ltv_before_bps: u32,
    ) -> Result<i128, Error> {
        let owner = s.owner.clone();
        let w = liquidation::wipeout(&e, &s.cfg, &s.pos, &v, px.price, px.decimals)?;
        if w.seized < min_deposit_out {
            // The liquidator receives everything that is left; asking for more than that is their
            // own error, and reverting here is safe — anyone else can call with zero.
            return Err(Error::SlippageExceeded);
        }

        let recovered_raw = liquidation::usd_to_debt(&e, &s.cfg, w.recovered)?;
        let (interest_paid, principal_paid) = interest::split(recovered_raw, &s.pos);
        let principal_paid = math::min_i128(principal_paid, s.pos.principal);
        let lost = math::checked_sub(s.pos.principal, principal_paid)?;

        s.pos.deposit = 0;
        s.pos.principal = 0;
        s.pos.interest = 0;

        s.res.total = math::checked_add(s.res.total, interest_paid)?;
        s.res.outstanding = math::checked_sub(s.res.outstanding, principal_paid)?;
        s.res.total = math::checked_sub(s.res.total, lost)?;
        s.res.outstanding = math::checked_sub(s.res.outstanding, lost)?;
        s.res.bad_debt = math::checked_add(s.res.bad_debt, lost)?;

        // Every grant this customer holds, released in one call.
        //
        // This is the reason `MAX_GRANTS_PER_OWNER` exists. The walk is bounded, so the cost of
        // liquidating a position cannot be inflated by its owner — the gas bomb Đ-02 refused when
        // it refused the deck's unbounded `GrantIndex`, refused here by a constant instead of by
        // allowing only one grant.
        //
        // Leaving any of them would strand that grant's `committed`: capital promised to a line
        // whose position no longer exists, which nobody could ever release.
        let operators = storage::grant_operators(&e, &owner);
        let mut released_events = soroban_sdk::Vec::new(&e);
        for op in operators.iter() {
            if let Some(g) = storage::grant(&e, &owner, &op) {
                let released = grant::release(&g, &mut s.res)?;
                released_events.push_back((op.clone(), g.cap, g.drawn, released));
            }
        }

        save(&e, &s)?;

        for op in operators.iter() {
            storage::remove_grant(&e, &owner, &op);
        }
        storage::set_grant_operators(&e, &owner, &soroban_sdk::Vec::new(&e));

        for (operator, cap, drawn, released) in released_events.iter() {
            events::GrantRevoked {
                owner: owner.clone(),
                operator,
                cap,
                drawn,
                released,
                committed_after: s.res.committed,
            }
            .publish(&e);
        }

        events::Liquidated {
            owner: owner.clone(),
            liquidator: liquidator.clone(),
            by_lender: false,
            repay_amount: recovered_raw,
            interest_paid,
            principal_paid,
            seized: w.seized,
            ltv_before_bps,
            ltv_after_bps: 0,
            principal_after: 0,
            deposit_after: 0,
        }
        .publish(&e);

        events::BadDebtRealised {
            owner: owner.clone(),
            lost_principal: lost,
            recovered: w.recovered,
            bad_debt_total: s.res.bad_debt,
            total_after: s.res.total,
        }
        .publish(&e);

        // The liquidator pays what the collateral is worth and takes all of it. `recovered` was
        // booked above as principal and interest returned, so the tokens have to actually arrive —
        // otherwise `total − outstanding` claims backing that does not exist and I2 breaks. There
        // is no bonus on this branch: the debt exceeds the collateral, so there is nothing to pay
        // one out of.
        if recovered_raw > 0 {
            debt_client(&e, &s.cfg).transfer(
                &liquidator,
                e.current_contract_address(),
                &recovered_raw,
            );
        }
        deposit_client(&e, &s.cfg).transfer(&e.current_contract_address(), &liquidator, &w.seized);
        storage::bump(&e, &owner);
        Ok(w.seized)
    }

    /// The first gate a real `draw` would fail, in the same order the real one checks them.
    ///
    /// Shares nothing with `draw` by construction — it must not mutate — but the order is the
    /// specification, so any divergence here is a bug in the quote, not in the gate.
    fn draw_block_reason(
        e: &Env,
        s: &State,
        v_after: &health::Valuation,
        px: &oracle::Prices,
        operator: Address,
        amount: i128,
    ) -> Result<Option<u32>, Error> {
        if amount <= 0 {
            return Ok(Some(Error::InvalidAmount as u32));
        }
        if storage::paused(e) {
            return Ok(Some(Error::Paused as u32));
        }
        // This operator's grant, not "the" grant: the question is whether *they* may draw.
        let g = match storage::grant(e, &s.owner, &operator) {
            None => return Ok(Some(Error::GrantNotFound as u32)),
            Some(g) => g,
        };
        let g = &g;
        if let Err(err) = grant::cheap_gates(g, &operator, amount, e.ledger().timestamp()) {
            return Ok(Some(err as u32));
        }
        if px.stale {
            return Ok(Some(Error::PriceStale as u32));
        }
        if !health::within_ltv(e, v_after, g.max_ltv_bps)? {
            return Ok(Some(Error::LtvCeilingExceeded as u32));
        }
        if !health::within_ltv(e, v_after, s.cfg.max_ltv_bps)? {
            return Ok(Some(Error::HealthTooLow as u32));
        }
        if s.res.committed < amount {
            return Ok(Some(Error::ReserveUnderfunded as u32));
        }
        Ok(None)
    }
}
