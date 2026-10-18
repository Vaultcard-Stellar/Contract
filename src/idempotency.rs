//! The replay guard.
//!
//! Card systems retry; a blockchain does not deduplicate. Without this, one network timeout
//! creates the debt twice — and the guard has to live where the final state lives, because a
//! `UNIQUE` constraint in the backend's database cannot see a transaction the backend never
//! learned the outcome of.

use crate::storage;
use soroban_sdk::{BytesN, Env};

pub fn seen(e: &Env, id: &BytesN<32>) -> bool {
    storage::draw_seen(e, id)
}

/// Record the id. Written once with `DRAW_SEEN_TTL` and never extended.
///
/// The lifetime is 30 days rather than the deck's 24 hours because the backend's outbox has an
/// `unknown` state whose only exit is a human — and humans have weekends. A guard that expires
/// before the operator gets to the queue is a guard that lets the duplicate through.
pub fn mark(e: &Env, id: &BytesN<32>) {
    storage::mark_draw_seen(e, id);
}
