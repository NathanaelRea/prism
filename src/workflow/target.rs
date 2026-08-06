#![allow(dead_code)] // Targets are selected by the generalized worker after cutover.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::run::{ExecutionWorkspaceId, RepositoryId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ExecutionTargetDescriptor {
    pub id: String,
    pub local: bool,
    pub confined: bool,
    pub supports_continuation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WorkspaceRef {
    pub id: ExecutionWorkspaceId,
    pub repository_id: Option<RepositoryId>,
    pub generation: i64,
    pub base_revision: String,
}

#[derive(Clone, Default)]
pub(crate) struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn signal(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

pub(crate) trait ExecutionTarget: Send + Sync {
    fn describe(&self) -> ExecutionTargetDescriptor;
    fn workspace_path(&self, workspace: &WorkspaceRef) -> Result<Option<PathBuf>, String>;
}

pub(crate) fn local_descriptor() -> ExecutionTargetDescriptor {
    ExecutionTargetDescriptor {
        id: "local".to_string(),
        local: true,
        // Local subprocesses currently execute with effective OS-user authority.
        confined: false,
        supports_continuation: false,
    }
}

pub(crate) struct LocalTarget {
    roots: BTreeMap<ExecutionWorkspaceId, PathBuf>,
}

impl LocalTarget {
    pub(crate) fn new(roots: BTreeMap<ExecutionWorkspaceId, PathBuf>) -> Result<Self, String> {
        for path in roots.values() {
            if !path.is_absolute() {
                return Err(format!(
                    "LocalTarget workspace path must be absolute: {}",
                    path.display()
                ));
            }
        }
        Ok(Self { roots })
    }

    pub(crate) fn single(id: ExecutionWorkspaceId, path: &Path) -> Result<Self, String> {
        Self::new(BTreeMap::from([(id, path.to_path_buf())]))
    }
}

impl ExecutionTarget for LocalTarget {
    fn describe(&self) -> ExecutionTargetDescriptor {
        local_descriptor()
    }

    fn workspace_path(&self, workspace: &WorkspaceRef) -> Result<Option<PathBuf>, String> {
        self.roots
            .get(&workspace.id)
            .cloned()
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "workspace '{}' is not mapped on LocalTarget",
                    workspace.id.as_str()
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTarget;

    impl ExecutionTarget for FakeTarget {
        fn describe(&self) -> ExecutionTargetDescriptor {
            ExecutionTargetDescriptor {
                id: "fake".to_string(),
                local: false,
                confined: true,
                supports_continuation: false,
            }
        }

        fn workspace_path(&self, _workspace: &WorkspaceRef) -> Result<Option<PathBuf>, String> {
            Ok(None)
        }
    }

    fn assert_target_contract(target: &dyn ExecutionTarget, expects_local_path: bool) {
        let workspace = WorkspaceRef {
            id: ExecutionWorkspaceId("opaque-workspace-id".to_string()),
            repository_id: Some(RepositoryId("opaque-repository-id".to_string())),
            generation: 7,
            base_revision: "immutable-revision".to_string(),
        };
        let path = target.workspace_path(&workspace).unwrap();
        assert_eq!(path.is_some(), expects_local_path);
        let cancellation = CancellationToken::default();
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        assert!(cancellation.is_cancelled());
        assert!(!target.describe().id.is_empty());
    }

    #[test]
    fn fake_non_local_target_passes_target_neutral_contract() {
        assert_target_contract(&FakeTarget, false);
        assert!(!FakeTarget.describe().local);
        assert!(FakeTarget.describe().confined);
    }

    #[test]
    fn local_target_passes_the_same_contract_at_the_path_boundary() {
        let id = ExecutionWorkspaceId("opaque-workspace-id".to_string());
        let target = LocalTarget::single(id, Path::new("/tmp")).unwrap();
        assert_target_contract(&target, true);
        assert!(target.describe().local);
        assert!(!target.describe().confined);
    }
}
