//! SEP-40 price adapter, staleness policy, and the outage guard.
//!
//! **This is the only module allowed to construct an `OracleClient`.** That is not style — it
//! is how "read the price at most once per invocation" is enforced by the module boundary
//! rather than by a comment someone will eventually ignore.
//!
//! Why it matters: `liquidate` needs a price in two places — the threshold check and the seize
//! computation. The oracle is an external contract with no guarantee of determinism inside one
//! transaction, so two independent reads can return two different values. The gate would pass
//! on one price and the seize would settle on another, and nothing would catch it, because each
//! step is individually correct.

use crate::errors::Error;
use crate::storage;
use crate::types::*;
use soroban_sdk::{contractclient, Env};

#[contractclient(name = "OracleClient")]
pub trait OracleInterface {
    /// Returns `None` when the feed has never published this asset — a condition distinct from
    /// "stale", and one that must not be mistaken for a price of zero.
    fn lastprice(env: Env, asset: Asset) -> Option<PriceData>;
    fn decimals(env: Env) -> u32;
    fn resolution(env: Env) -> u32;
}

/// A price, loaded exactly once, carried down to whoever needs it.
#[derive(Clone, Debug)]
pub struct Prices {
    pub price: i128,
    pub decimals: u32,
    pub timestamp: u64,
    /// Set when any freshness check failed. On the strict path the call has already returned an
    /// error by then, so this is only ever `true` on the view path.
    pub stale: bool,
    /// `u64::MAX` when there is no price at all.
    pub age: u64,
}

/// The single freshness predicate.
///
/// **Four checks, and `report_oracle_state` uses the same four.** If the guard used a looser
/// test — say, presence and age only — a feed running ten minutes fast would count as fresh
/// there and be rejected with `PriceInvalid` here. The guard would never record an outage, the
/// grace window would never open, and liquidation would stay blocked with no way to clear it.
fn evaluate(e: &Env, cfg: &Config, d: &PriceData) -> Result<(), Error> {
    if d.price <= 0 {
        return Err(Error::PriceInvalid);
    }
    let now = e.ledger().timestamp();
    if d.timestamp > now.saturating_add(FUTURE_PRICE_SLACK) {
        return Err(Error::PriceInvalid);
    }
    // Boundary is inclusive on the safe side: age == price_max_age is still valid.
    if now.saturating_sub(d.timestamp) > cfg.price_max_age {
        return Err(Error::PriceStale);
    }
    Ok(())
}

/// Load the price for the deposit asset. **Exactly one `lastprice` call.**
///
/// `strict = true` on every write path: a bad price is an error and the call reverts.
/// `strict = false` on the view path: views must render during an outage, so they get a zeroed
/// valuation and a flag rather than a trap.
pub fn load(e: &Env, cfg: &Config, strict: bool) -> Result<Prices, Error> {
    let client = OracleClient::new(e, &cfg.oracle);
    let decimals = client.decimals();
    let now = e.ledger().timestamp();

    match client.lastprice(&cfg.deposit_feed) {
        None => {
            if strict {
                Err(Error::PriceInvalid)
            } else {
                Ok(Prices {
                    price: 0,
                    decimals,
                    timestamp: 0,
                    stale: true,
                    age: u64::MAX,
                })
            }
        }
        Some(d) => match evaluate(e, cfg, &d) {
            Ok(()) => Ok(Prices {
                price: d.price,
                decimals,
                timestamp: d.timestamp,
                stale: false,
                age: now.saturating_sub(d.timestamp),
            }),
            Err(err) => {
                if strict {
                    Err(err)
                } else {
                    Ok(Prices {
                        price: d.price.max(0),
                        decimals,
                        timestamp: d.timestamp,
                        stale: true,
                        age: now.saturating_sub(d.timestamp),
                    })
                }
            }
        },
    }
}

/// Is the feed usable right now? Never panics, never errors.
///
/// Used by `report_oracle_state`, which must be callable when everything else is failing.
pub fn is_fresh(e: &Env, cfg: &Config) -> bool {
    let client = OracleClient::new(e, &cfg.oracle);
    match client.lastprice(&cfg.deposit_feed) {
        None => false,
        Some(d) => evaluate(e, cfg, &d).is_ok(),
    }
}

// `probe` lived here: a liveness check on a *proposed* oracle, run by `accept_oracle` before a
// swap could complete. `Config.oracle` is frozen now, so there is no swap and nothing to probe.
// Deleted rather than kept "in case" — an unreachable function is code an auditor has to read and
// a reviewer has to keep correct, for no behaviour. Coverage is what found it.

/// Outcome of a guard update, so the caller knows which event to emit.
pub enum GuardChange {
    None,
    /// The feed just became unusable.
    OutageStarted(u64),
    /// The feed recovered; liquidation is blocked until the given timestamp.
    Recovered(u64),
}

/// Record an outage or a recovery. **Must never panic.**
///
/// This has to be a separate entry point rather than a side effect of the ordinary price path,
/// because that path *reverts* on a bad price — the state write recording the outage would be
/// rolled back along with the failed call.
///
/// Permissionless because both sides want it called: the owner wants the grace window recorded,
/// the liquidator wants it to expire. Nobody has to be trusted.
pub fn update_guard(e: &Env, cfg: &Config) -> (bool, GuardChange) {
    let fresh = is_fresh(e, cfg);
    let now = e.ledger().timestamp();
    let mut g = storage::oracle_guard(e);

    if !fresh && g.outage_at == 0 {
        g.outage_at = now;
        storage::set_oracle_guard(e, &g);
        return (false, GuardChange::OutageStarted(now));
    }
    if fresh && g.outage_at != 0 {
        let until = now.saturating_add(LIQUIDATION_GRACE);
        g.grace_until = until;
        g.outage_at = 0;
        storage::set_oracle_guard(e, &g);
        return (true, GuardChange::Recovered(until));
    }
    (fresh, GuardChange::None)
}

/// Both liquidation routes call this.
///
/// The window exists because the deck acknowledges that a stale price blocks liquidation but
/// never says what happens when the feed comes back. Without it: the oracle dies for four
/// hours, the market moves 40%, and the instant it recovers every over-threshold position is
/// liquidated at a price it never had a chance to react to.
pub fn require_not_in_grace(e: &Env) -> Result<(), Error> {
    let g = storage::oracle_guard(e);
    if e.ledger().timestamp() < g.grace_until {
        return Err(Error::LiquidationGrace);
    }
    Ok(())
}
