use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{ContentRevision, ResourceError, ResourceScope};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub repository: String,
    pub revision: String,
    pub trusted_at_unix_seconds: u64,
}

pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn is_trusted(
        &self,
        scope: ResourceScope,
        repository: Option<&Path>,
        revision: &ContentRevision,
    ) -> Result<bool, ResourceError> {
        if scope == ResourceScope::Global {
            return Ok(true);
        }
        let repository = repository.ok_or_else(|| ResourceError::InvalidSource {
            path: PathBuf::new(),
            message: "repository scope requires a repository path".into(),
        })?;
        let key = canonical_repository(repository)?;
        Ok(self
            .load()?
            .get(&key)
            .is_some_and(|record| record.revision == revision.as_str()))
    }

    pub fn trust(
        &self,
        repository: &Path,
        revision: &ContentRevision,
        now: std::time::SystemTime,
    ) -> Result<TrustRecord, ResourceError> {
        let key = canonical_repository(repository)?;
        let record = TrustRecord {
            repository: key.clone(),
            revision: revision.to_string(),
            trusted_at_unix_seconds: now
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        let mut records = self.load()?;
        records.insert(key, record.clone());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let encoded =
            serde_json::to_vec_pretty(&records).map_err(|error| ResourceError::InvalidSource {
                path: self.path.clone(),
                message: error.to_string(),
            })?;
        fs::write(&self.path, encoded)?;
        Ok(record)
    }

    fn load(&self) -> Result<BTreeMap<String, TrustRecord>, ResourceError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|error| ResourceError::InvalidSource {
                    path: self.path.clone(),
                    message: error.to_string(),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error.into()),
        }
    }
}

fn canonical_repository(repository: &Path) -> Result<String, ResourceError> {
    Ok(fs::canonicalize(repository)?.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repository_trust_is_exact_revision_while_global_is_owned() {
        let root = std::env::temp_dir().join(format!("prism-trust-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let store = TrustStore::new(root.join("trust.json"));
        let old = ContentRevision::digest(b"old");
        let changed = ContentRevision::digest(b"changed");
        store
            .trust(&root, &old, std::time::SystemTime::now())
            .unwrap();
        assert!(
            store
                .is_trusted(ResourceScope::Repository, Some(&root), &old)
                .unwrap()
        );
        assert!(
            !store
                .is_trusted(ResourceScope::Repository, Some(&root), &changed)
                .unwrap()
        );
        assert!(
            store
                .is_trusted(ResourceScope::Global, None, &changed)
                .unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
