//! The public DNS cutover contract, entered through the crate exports.
//!
//! Type only: every `DnsProvider` carries a required `prepare` callback that
//! takes the complete canonical domain set on `&mut self` and returns a
//! `Send` future. Its zone authority, admission, and wire effects belong to
//! the component, acceptance, and external roots.

use std::future::Future;
use std::sync::Arc;

use camber::RuntimeError;
use camber::dns01::{CloudflareProvider, DnsProvider};

/// Compiles only when `P::prepare` exists with the declared shape.
fn prepare_every_domain<'a, P: DnsProvider>(
    provider: &'a mut P,
    domains: &'a [Arc<str>],
) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'a {
    provider.prepare(domains)
}

#[test]
fn dns_public_cutover_contract_exists() {
    let _witness = prepare_every_domain::<CloudflareProvider>;
}
