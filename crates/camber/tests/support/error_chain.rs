//! What one error shows an operator, read through every way it can be printed.
//!
//! A redacted error is redacted in its `Display`, in both `Debug` forms, and in
//! every link `source` reaches. A check that read one rendering would pass an
//! error that kept its secret in another, so each claim here reads them all.
//!
//! Nothing in this module names a Camber type, so a root mounts it without
//! mounting anything else.

use std::error::Error;

/// The longest chain a closed error may have before the walk reports a cycle.
const CHAIN_BOUND: usize = 16;

/// Every link `error` reaches through `source`, itself first.
pub fn links<'a>(error: &'a (dyn Error + 'static), label: &str) -> Vec<&'a (dyn Error + 'static)> {
    let mut chain = Vec::new();
    let mut next = Some(error);
    while let Some(link) = next {
        assert!(
            chain.len() < CHAIN_BOUND,
            "{label}: the source chain did not end within {CHAIN_BOUND} links"
        );
        chain.push(link);
        next = link.source();
    }
    chain
}

/// Every rendering of every link: `Display`, `Debug`, and alternate `Debug`.
pub fn renderings(error: &(dyn Error + 'static), label: &str) -> Vec<String> {
    links(error, label)
        .into_iter()
        .flat_map(|link| [format!("{link}"), format!("{link:?}"), format!("{link:#?}")])
        .collect()
}

/// Require that no rendering of `error` or of its sources repeats a secret.
///
/// An empty list is refused first. This is a leak assertion, and one handed
/// nothing to look for passes over every error there is.
pub fn assert_secret_free(error: &(dyn Error + 'static), secrets: &[impl AsRef<str>], label: &str) {
    assert!(
        !secrets.is_empty(),
        "{label}: no secret was declared, so no leak can be found"
    );
    for rendering in renderings(error, label) {
        assert!(
            !rendering.is_empty(),
            "{label}: one rendering of the error is empty"
        );
        for secret in secrets.iter().map(AsRef::as_ref) {
            assert!(
                !rendering.contains(secret),
                "{label}: the secret {secret:?} reached an error rendering: {rendering}"
            );
        }
    }
}

/// Require `error` to be safe in every rendering and in every source: it
/// repeats no secret, and it keeps no raw third-party error.
pub fn assert_safe(error: &(dyn Error + 'static), secrets: &[impl AsRef<str>], label: &str) {
    assert_secret_free(error, secrets, label);
    assert_no_raw_source(error, label);
}

/// Require that no link of `error` is a third-party or transport error.
///
/// A closed category cannot repeat a URL. A raw SDK error kept as its source
/// can, and a caller that walks the chain then prints it.
pub fn assert_no_raw_source(error: &(dyn Error + 'static), label: &str) {
    for link in links(error, label) {
        let raw = [
            ("reqwest::Error", link.is::<reqwest::Error>()),
            ("std::io::Error", link.is::<std::io::Error>()),
            ("rustls::Error", link.is::<rustls::Error>()),
            ("serde_json::Error", link.is::<serde_json::Error>()),
        ];
        for (name, found) in raw {
            assert!(
                !found,
                "{label}: the source chain kept a raw {name}: {link:?}"
            );
        }
    }
}
