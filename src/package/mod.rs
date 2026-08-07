//! Packages of editable resources backed by immutable retained revisions.

mod install;
mod manifest;
mod migration;
mod source;
mod working_copy;

pub use install::{InstallError, InstallOutcome, PackageInstaller, bootstrap_standard_pack};
pub use manifest::{
    Dependency, ExtensionArtifact, LockedPackage, PackageLock, PackageManifest, PackageResource,
    PackageSource, PackageValidationError, ResourceType, TargetArtifact,
};
pub use migration::{Migration, MigrationError, MigrationPlan, Migrator};
pub use source::{ResolvedSource, SourceLimits, SourceResolver};
pub use working_copy::{FileUpdate, MergeConflict, UpdatePlan, WorkingCopy, WorkingCopyError};
