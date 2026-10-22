// Tests may panic; the workspace ban on panicking calls covers source only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]

#[path = "focused_bench/support.rs"]
pub mod support;

#[path = "focused_bench/contracts.rs"]
mod contracts;
#[path = "focused_bench/load_contract.rs"]
mod load_contract;
#[path = "focused_bench/loc.rs"]
mod loc;
