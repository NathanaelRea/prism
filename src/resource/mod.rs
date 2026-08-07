//! Discovery and immutable retention for user-owned Prism resources.

mod identity;
mod store;
mod trust;

pub use identity::{
    DiscoveredResource, QualifiedIdentity, ResourceError, ResourceKind, ResourceScope, discover,
};
pub use store::{
    ContentRevision, ContentStore, CorruptBlob, DanglingReference, Reference, StoreAudit,
};
pub use trust::{TrustRecord, TrustStore};
