//! Process-isolated, versioned extension boundary.

pub mod broker;
pub mod host;
pub mod operations;
pub mod registry;

pub use broker::{
    BrokerFuture, BrokeredHostDispatcher, EffectLedger, PreparedEffect, ProtectedEffectBackend,
};
pub use host::{
    AllowlistedHostDispatcher, ExtensionClient, ExtensionHostError, ExtensionSupervisor,
    HostDispatcher, HostFuture, HostLimits, HostOperationServices, NoHostOperations,
};
pub use operations::{DiagnosticReport, ExtensionOperations};
pub use registry::{DescriptorRegistry, RegistryError};
