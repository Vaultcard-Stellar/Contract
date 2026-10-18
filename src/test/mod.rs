//! The suite. Organised by the sections of `docs/10-testing.md`.
//!
//! Every module ends its scenarios with [`harness::F::check_books`], which asserts I1–I4 **and**
//! both token balances. The invariant checks inside the contract cannot see the balances; that
//! is where a missing transfer hides, with the accounting staying perfectly self-consistent
//! while the tokens are somewhere else.

#![allow(clippy::bool_assert_comparison)]

#[macro_use]
mod harness;

mod acceptance_tests;
mod config_tests;
mod decimals_tests;
mod deck_conformance;
mod demo_replay;
mod draw_tests;
mod governance_tests;
mod grant_tests;
mod headroom_tests;
mod interest_tests;
mod liquidation_tests;
mod market_tests;
mod math_tests;
mod multi_grant_tests;
mod oracle_tests;
mod position_tests;
mod property_tests;
mod rebuild_test;
mod reserve_tests;
