use std::time::{SystemTime, UNIX_EPOCH};

use super::definition::DefinitionCatalog;
use super::operations::{DefinitionSnapshot, WorkflowOperations};
use crate::WorkflowOperationError;

/// Register discovered loose and package workflows as immutable compiled snapshots.
///
/// Definitions reach this seam through the same catalog used for user-owned resources; there are
/// no hidden launch definitions or privileged implementation registrations here.
pub async fn install(
    operations: &WorkflowOperations,
    catalog: &DefinitionCatalog,
) -> Result<(), CatalogInstallError> {
    let now = now_ms();
    for definition in catalog.list() {
        let snapshot = catalog.compile(&definition.id)?;
        let body = serde_json::to_string(&snapshot)
            .map_err(|error| CatalogInstallError::Serialization(error.to_string()))?;
        operations
            .register_definition(DefinitionSnapshot {
                id: &snapshot.digest,
                name: &snapshot.definition.name,
                revision: &definition.revision,
                source: &definition.path.to_string_lossy(),
                trusted: snapshot.trusted,
                body_json: &body,
                digest: &snapshot.digest,
                now_unix_ms: now,
            })
            .await?;
    }
    Ok(())
}

#[derive(Debug)]
pub enum CatalogInstallError {
    Definition(super::definition::DefinitionError),
    Operation(WorkflowOperationError),
    Serialization(String),
}

impl std::fmt::Display for CatalogInstallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definition(error) => error.fmt(formatter),
            Self::Operation(error) => error.fmt(formatter),
            Self::Serialization(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CatalogInstallError {}

impl From<super::definition::DefinitionError> for CatalogInstallError {
    fn from(value: super::definition::DefinitionError) -> Self {
        Self::Definition(value)
    }
}

impl From<WorkflowOperationError> for CatalogInstallError {
    fn from(value: WorkflowOperationError) -> Self {
        Self::Operation(value)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
