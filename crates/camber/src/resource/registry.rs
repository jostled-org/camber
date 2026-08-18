//! The frozen, ordered set of resources one runtime coordinates.

use super::Resource;
use super::entry::ResourceEntry;
use crate::RuntimeError;
use std::sync::Arc;

/// Every registered resource, in registration order, frozen for the life of the
/// runtime.
///
/// Shared rather than owned by one coordinator: the startup pass, the periodic
/// coordinator, the teardown pass, and `/health` all read the same entries, and
/// a resource's published health would mean nothing if they read copies.
pub(crate) type ResourceRegistry = Arc<[Arc<ResourceEntry>]>;

/// Freeze the registered resources into the entries every phase reads, or
/// refuse a registration that cannot be reported on.
///
/// # Errors
///
/// Returns [`RuntimeError::InvalidArgument`] naming the offending resource when
/// any name is blank or registered twice.
pub(crate) fn freeze(resources: Vec<Box<dyn Resource>>) -> Result<ResourceRegistry, RuntimeError> {
    validate_names(&resources)?;
    // `Arc<[T]>: FromIterator<T>` over an exact-size map allocates once, where
    // collecting through a `Vec` allocates twice and memcpys.
    Ok(resources
        .into_iter()
        .map(|resource| Arc::new(ResourceEntry::new(Arc::from(resource))))
        .collect())
}

/// Accept a resource registry whose every entry can be told apart, or refuse it.
///
/// A registered name is the only identity a health entry, an operator event,
/// and a lifecycle failure have for the resource behind them. A blank name
/// identifies nothing and a repeated one identifies two things, so both are
/// refused before the runtime that would report under them is established —
/// while refusing still costs nothing but the caller's registration.
fn validate_names(resources: &[Box<dyn Resource>]) -> Result<(), RuntimeError> {
    let mut seen = std::collections::HashSet::with_capacity(resources.len());
    for resource in resources {
        match resource.name().trim() {
            "" => {
                return Err(RuntimeError::InvalidArgument(
                    "resource name must not be blank".into(),
                ));
            }
            name if seen.insert(name) => {}
            name => {
                return Err(RuntimeError::InvalidArgument(
                    format!("resource name {name:?} is registered more than once").into_boxed_str(),
                ));
            }
        }
    }
    Ok(())
}
