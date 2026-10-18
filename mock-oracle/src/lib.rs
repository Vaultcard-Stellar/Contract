#![no_std]
//! SEP-40 compatible price source with a settable price. **Testnet only.**
//!
//! It exists for one reason: a real price feed cannot be pushed, so there is no way to prove
//! liquidation or insolvency on chain without it, and the acceptance package needs both.
//!
//! **Never deploy this to a production network.**

use soroban_sdk::{contract, contractimpl, contracttype, symbol_short, Address, Env, Symbol};

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Asset {
    Stellar(Address),
    Other(Symbol),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PriceData {
    pub price: i128,
    pub timestamp: u64,
}

const ADMIN: Symbol = symbol_short!("admin");
const DEC: Symbol = symbol_short!("dec");
const DRIFT: Symbol = symbol_short!("drift");

#[contract]
pub struct MockOracle;

#[contractimpl]
impl MockOracle {
    pub fn __constructor(e: Env, admin: Address, decimals: u32) {
        e.storage().instance().set(&ADMIN, &admin);
        e.storage().instance().set(&DEC, &decimals);
    }

    /// `timestamp = 0` means "use the current ledger time"; pass anything else to simulate a
    /// stale price.
    pub fn set_price(e: Env, asset: Asset, price: i128, timestamp: u64) {
        let admin: Address = e.storage().instance().get(&ADMIN).unwrap();
        admin.require_auth();
        let ts = if timestamp == 0 {
            e.ledger().timestamp()
        } else {
            timestamp
        };
        e.storage().instance().set(
            &asset,
            &PriceData {
                price,
                timestamp: ts,
            },
        );
        e.storage().instance().extend_ttl(100_000, 500_000);
    }

    /// Delete the price so `lastprice` returns `None` — the "feed never published this asset"
    /// case, which is distinct from stale and must not be mistaken for a price of zero.
    pub fn clear_price(e: Env, asset: Asset) {
        let admin: Address = e.storage().instance().get(&ADMIN).unwrap();
        admin.require_auth();
        e.storage().instance().remove(&asset);
    }

    /// Arm the **drift** mode: every `lastprice` call moves the stored price by `delta` *after*
    /// answering, so a second call within the same invocation returns a different number.
    ///
    /// This is the only way to prove the vault's "read the price at most once per invocation"
    /// rule. Without it the rule is untestable: two reads of a constant feed agree, and the bug
    /// — a threshold check passing on one price while the seize settles on another — stays
    /// invisible until a real feed updates mid-transaction.
    ///
    /// `delta = 0` disarms it.
    pub fn set_drift(e: Env, delta: i128) {
        let admin: Address = e.storage().instance().get(&ADMIN).unwrap();
        admin.require_auth();
        e.storage().instance().set(&DRIFT, &delta);
    }

    pub fn lastprice(e: Env, asset: Asset) -> Option<PriceData> {
        let d: Option<PriceData> = e.storage().instance().get(&asset);
        let delta: i128 = e.storage().instance().get(&DRIFT).unwrap_or(0);
        if delta != 0 {
            if let Some(cur) = &d {
                e.storage().instance().set(
                    &asset,
                    &PriceData {
                        price: cur.price + delta,
                        timestamp: cur.timestamp,
                    },
                );
            }
        }
        d
    }

    pub fn decimals(e: Env) -> u32 {
        e.storage().instance().get(&DEC).unwrap_or(14)
    }

    pub fn resolution(_e: Env) -> u32 {
        300
    }
}
