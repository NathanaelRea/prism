//! Discovery and immutable retention for user-owned Prism resources.

mod identity;
mod store;
mod trust;

pub use identity::{
    DiscoveredResource, QualifiedIdentity, ResourceError, ResourceKind, ResourceScope, discover,
    ensure_global_drop_in_directories,
};
pub use store::{
    ContentRevision, ContentStore, CorruptBlob, DanglingReference, Reference, StoreAudit,
};
pub use trust::{TrustRecord, TrustStore};
