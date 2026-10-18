#![no_std]
//! A SEP-41 token with settable decimals. **Testnet only.**
//!
//! # Why not the Stellar Asset Contract
//!
//! The SAC is the right answer for a real deployment and the wrong one for this demo, for one
//! reason: **it is fixed at 7 decimals.** A market whose collateral token also has 7 exercises
//! none of the rescaling — `shift` is `7 + price_decimals − 7`, the negative branch never runs,
//! and every conversion happens to be the identity on the axis most likely to be wrong.
//!
//! The acceptance scenario is supposed to demonstrate a market that works, not a market whose
//! decimals happen to cancel. This deploys as an 8-decimal collateral token against a 7-decimal
//! debt token and a 14-decimal feed, which is the arrangement the contract's own fixture uses
//! and the one where a scale error has somewhere to hide.
//!
//! # What it deliberately does not have
//!
//! No allowances, no authorisation on `mint`, no admin, no burn. Anyone can mint. That is
//! catastrophic for a real token and exactly right for a demo one: the script needs to fund six
//! accounts without a key-management ceremony, and a token nobody could mint from would need one.
//!
//! **Nothing in this crate may ever reach mainnet.** It is not a member of the contract's
//! dependency graph — the vault takes its tokens by address — so the only thing standing between
//! this and a real deployment is not deploying it.

use soroban_sdk::{contract, contractimpl, contracttype, Address, Env, MuxedAddress};

#[contracttype]
enum Key {
    Decimals,
    Balance(Address),
}

#[contract]
pub struct MockToken;

#[contractimpl]
impl MockToken {
    pub fn __constructor(e: Env, decimals: u32) {
        e.storage().instance().set(&Key::Decimals, &decimals);
    }

    /// Read by the vault's constructor, which never trusts the decimals its caller passes.
    pub fn decimals(e: Env) -> u32 {
        e.storage().instance().get(&Key::Decimals).unwrap_or(7)
    }

    pub fn balance(e: Env, id: Address) -> i128 {
        e.storage().persistent().get(&Key::Balance(id)).unwrap_or(0)
    }

    /// Unauthenticated on purpose. See the module note.
    pub fn mint(e: Env, to: Address, amount: i128) {
        let b = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&Key::Balance(to), &(b + amount));
    }

    /// `MuxedAddress` because that is what SEP-41 declares in SDK 27, and the vault calls through
    /// the generated SEP-41 client — a signature that differs by that much does not link.
    pub fn transfer(e: Env, from: Address, to: MuxedAddress, amount: i128) {
        from.require_auth();
        let to = to.address();
        let fb = Self::balance(e.clone(), from.clone());
        // Panics on an overdraft rather than going negative. A demo token that lets a balance go
        // below zero would let the acceptance scenario "succeed" through a transfer that could
        // not happen against a real token.
        let tb = Self::balance(e.clone(), to.clone());
        e.storage()
            .persistent()
            .set(&Key::Balance(from), &(fb - amount));
        e.storage()
            .persistent()
            .set(&Key::Balance(to), &(tb + amount));
    }
}
