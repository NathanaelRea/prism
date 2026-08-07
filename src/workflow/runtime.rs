use std::time::{SystemTime, UNIX_EPOCH};

use super::definition::DefinitionCatalog;
use super::operations::{DefinitionSnapshot, WorkflowOperations};
use crate::WorkflowOperationError;

/// Register the catalog's current definitions as immutable snapshots for future runs.
///
/// This is runtime snapshot registration, not resource installation. Callers rediscover the
/// filesystem before invoking this function, so adding or editing a loose TOML is immediately
/// reflected in future launches while existing runs keep their pinned snapshot.
pub async fn register_catalog_snapshots(
    operations: &WorkflowOperations,
    catalog: &DefinitionCatalog,
) -> Result<(), CatalogRegistrationError> {
    let now = now_ms();
    for definition in catalog.list() {
        let snapshot = catalog.compile(&definition.id)?;
        let body = serde_json::to_string(&snapshot)
            .map_err(|error| CatalogRegistrationError::Serialization(error.to_string()))?;
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
pub enum CatalogRegistrationError {
    Definition(super::definition::DefinitionError),
    Operation(WorkflowOperationError),
    Serialization(String),
}

impl std::fmt::Display for CatalogRegistrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Definition(error) => error.fmt(formatter),
            Self::Operation(error) => error.fmt(formatter),
            Self::Serialization(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CatalogRegistrationError {}

impl From<super::definition::DefinitionError> for CatalogRegistrationError {
    fn from(value: super::definition::DefinitionError) -> Self {
        Self::Definition(value)
    }
}

impl From<WorkflowOperationError> for CatalogRegistrationError {
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
