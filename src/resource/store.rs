use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::ResourceError;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ContentRevision(String);

impl ContentRevision {
    pub fn digest(bytes: &[u8]) -> Self {
        Self(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ResourceError> {
        let value = value.into();
        let digest = value
            .strip_prefix("sha256:")
            .ok_or_else(|| ResourceError::InvalidRevision(value.clone()))?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ResourceError::InvalidRevision(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
    fn hex(&self) -> &str {
        &self.0[7..]
    }
}

impl fmt::Display for ContentRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Reference {
    pub owner: String,
    pub revision: ContentRevision,
}

pub struct ContentStore {
    root: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StoreAudit {
    pub blobs: usize,
    pub references: usize,
    pub orphaned: Vec<ContentRevision>,
    pub corrupt: Vec<CorruptBlob>,
    pub dangling: Vec<DanglingReference>,
    pub invalid_entries: Vec<PathBuf>,
}

impl StoreAudit {
    pub fn healthy(&self) -> bool {
        self.corrupt.is_empty() && self.dangling.is_empty() && self.invalid_entries.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CorruptBlob {
    pub expected: ContentRevision,
    pub actual: ContentRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DanglingReference {
    pub owner: String,
    pub revision: ContentRevision,
}

impl ContentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn retain(&self, bytes: &[u8]) -> Result<ContentRevision, ResourceError> {
        let revision = ContentRevision::digest(bytes);
        let path = self.blob_path(&revision);
        if path.is_file() {
            let actual = ContentRevision::digest(&fs::read(&path)?);
            if actual != revision {
                return Err(ResourceError::DigestMismatch {
                    expected: revision.to_string(),
                    actual: actual.to_string(),
                });
            }
            return Ok(revision);
        }
        let parent = path.parent().expect("blob path has parent");
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}.{}.tmp", revision.hex(), std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match fs::rename(&temporary, &path) {
            Ok(()) => {}
            Err(_error) if path.is_file() => {
                let _ = fs::remove_file(&temporary);
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }
        sync_directory(parent)?;
        Ok(revision)
    }

    pub fn load(&self, revision: &ContentRevision) -> Result<Vec<u8>, ResourceError> {
        let bytes = fs::read(self.blob_path(revision))?;
        let actual = ContentRevision::digest(&bytes);
        if &actual != revision {
            return Err(ResourceError::DigestMismatch {
                expected: revision.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(bytes)
    }

    pub fn add_reference(&self, reference: &Reference) -> Result<(), ResourceError> {
        if !self.blob_path(&reference.revision).is_file() {
            return Err(ResourceError::InvalidRevision(
                reference.revision.to_string(),
            ));
        }
        let path = self.reference_path(&reference.owner, &reference.revision)?;
        fs::create_dir_all(path.parent().expect("reference path parent"))?;
        fs::write(path, reference.revision.as_str())?;
        Ok(())
    }

    pub fn remove_reference(&self, reference: &Reference) -> Result<(), ResourceError> {
        match fs::remove_file(self.reference_path(&reference.owner, &reference.revision)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn references(&self, revision: &ContentRevision) -> Result<Vec<String>, ResourceError> {
        let root = self.root.join("references");
        if !root.is_dir() {
            return Ok(Vec::new());
        }
        let mut owners = Vec::new();
        for owner in fs::read_dir(root)? {
            let owner = owner?;
            if owner.path().join(revision.hex()).is_file() {
                owners.push(owner.file_name().to_string_lossy().into_owned());
            }
        }
        owners.sort();
        Ok(owners)
    }

    /// Audits retained bytes without repairing or collecting anything. References are treated as
    /// retention roots even when their target is missing, so a broken store is always visible to
    /// doctor tooling instead of being silently normalized by garbage collection.
    pub fn audit(&self) -> Result<StoreAudit, ResourceError> {
        let mut audit = StoreAudit::default();
        let mut referenced = BTreeSet::new();
        let references = self.root.join("references");
        if references.is_dir() {
            for owner in sorted_entries(&references)? {
                if !owner.file_type()?.is_dir() {
                    audit.invalid_entries.push(owner.path());
                    continue;
                }
                let owner_name = owner.file_name().to_string_lossy().into_owned();
                for entry in sorted_entries(&owner.path())? {
                    if !entry.file_type()?.is_file() {
                        audit.invalid_entries.push(entry.path());
                        continue;
                    }
                    let Some(hex) = entry.file_name().to_str().map(str::to_owned) else {
                        audit.invalid_entries.push(entry.path());
                        continue;
                    };
                    let Ok(revision) = ContentRevision::parse(format!("sha256:{hex}")) else {
                        audit.invalid_entries.push(entry.path());
                        continue;
                    };
                    audit.references += 1;
                    referenced.insert(revision.clone());
                    if !matches!(
                        fs::read_to_string(entry.path()),
                        Ok(contents) if contents == revision.as_str()
                    ) {
                        audit.invalid_entries.push(entry.path());
                    } else if !self.blob_path(&revision).is_file() {
                        audit.dangling.push(DanglingReference {
                            owner: owner_name.clone(),
                            revision,
                        });
                    }
                }
            }
        }

        let blobs = self.root.join("sha256");
        if blobs.is_dir() {
            for entry in sorted_entries(&blobs)? {
                if !entry.file_type()?.is_file() {
                    audit.invalid_entries.push(entry.path());
                    continue;
                }
                let Some(hex) = entry.file_name().to_str().map(str::to_owned) else {
                    audit.invalid_entries.push(entry.path());
                    continue;
                };
                let Ok(expected) = ContentRevision::parse(format!("sha256:{hex}")) else {
                    audit.invalid_entries.push(entry.path());
                    continue;
                };
                audit.blobs += 1;
                let actual = ContentRevision::digest(&fs::read(entry.path())?);
                if actual != expected {
                    audit.corrupt.push(CorruptBlob { expected, actual });
                } else if !referenced.contains(&expected) {
                    audit.orphaned.push(expected);
                }
            }
        }
        audit.orphaned.sort();
        audit
            .corrupt
            .sort_by(|left, right| left.expected.cmp(&right.expected));
        audit.dangling.sort_by(|left, right| {
            (&left.owner, &left.revision).cmp(&(&right.owner, &right.revision))
        });
        audit.invalid_entries.sort();
        Ok(audit)
    }

    pub fn collect_unreferenced(
        &self,
        protected: &BTreeSet<ContentRevision>,
    ) -> Result<Vec<ContentRevision>, ResourceError> {
        let blobs = self.root.join("sha256");
        if !blobs.is_dir() {
            return Ok(Vec::new());
        }
        let mut removed = Vec::new();
        for entry in fs::read_dir(blobs)? {
            let entry = entry?;
            let Some(hex) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(revision) = ContentRevision::parse(format!("sha256:{hex}")) else {
                continue;
            };
            if !protected.contains(&revision) && self.references(&revision)?.is_empty() {
                fs::remove_file(entry.path())?;
                removed.push(revision);
            }
        }
        removed.sort();
        Ok(removed)
    }

    fn blob_path(&self, revision: &ContentRevision) -> PathBuf {
        self.root.join("sha256").join(revision.hex())
    }

    fn reference_path(
        &self,
        owner: &str,
        revision: &ContentRevision,
    ) -> Result<PathBuf, ResourceError> {
        if owner.is_empty()
            || owner.contains('/')
            || owner.contains('\\')
            || owner == "."
            || owner == ".."
        {
            return Err(ResourceError::InvalidIdentity(owner.into()));
        }
        Ok(self
            .root
            .join("references")
            .join(owner)
            .join(revision.hex()))
    }
}

fn sorted_entries(path: &Path) -> Result<Vec<fs::DirEntry>, ResourceError> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn sync_directory(path: &Path) -> Result<(), ResourceError> {
    crate::durability::sync_directory(path, crate::durability::DurabilityIntent::Standard)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn referenced_content_survives_collection() {
        let root = std::env::temp_dir().join(format!("prism-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = ContentStore::new(&root);
        let kept = store.retain(b"kept").unwrap();
        let dropped = store.retain(b"dropped").unwrap();
        store
            .add_reference(&Reference {
                owner: "run-1".into(),
                revision: kept.clone(),
            })
            .unwrap();
        assert_eq!(
            store.collect_unreferenced(&BTreeSet::new()).unwrap(),
            vec![dropped]
        );
        assert_eq!(store.load(&kept).unwrap(), b"kept");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audit_reports_orphans_corruption_and_dangling_references_without_mutation() {
        let root = std::env::temp_dir().join(format!("prism-store-audit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let store = ContentStore::new(&root);
        let orphan = store.retain(b"orphan").unwrap();
        let corrupt = store.retain(b"corrupt").unwrap();
        let kept = store.retain(b"kept").unwrap();
        store
            .add_reference(&Reference {
                owner: "run-1".into(),
                revision: kept,
            })
            .unwrap();
        fs::write(store.blob_path(&corrupt), b"changed").unwrap();
        let missing = ContentRevision::digest(b"missing");
        let path = store.reference_path("run-2", &missing).unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, missing.as_str()).unwrap();
        fs::write(root.join("sha256/not-a-digest"), b"invalid").unwrap();

        let audit = store.audit().unwrap();
        assert!(!audit.healthy());
        assert_eq!(audit.blobs, 3);
        assert_eq!(audit.references, 2);
        assert_eq!(audit.orphaned, vec![orphan]);
        assert_eq!(audit.corrupt[0].expected, corrupt);
        assert_eq!(audit.dangling[0].revision, missing);
        assert_eq!(audit.invalid_entries.len(), 1);
        assert!(store.blob_path(&corrupt).is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
