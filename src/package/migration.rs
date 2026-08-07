use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationPlan {
    pub source: PathBuf,
    pub from_version: u32,
    pub to_version: u32,
    pub before_sha256: String,
    pub migrated: String,
    pub backup: PathBuf,
}

pub struct Migration {
    pub from_version: u32,
    pub to_version: u32,
    pub apply: fn(&str) -> Result<String, String>,
}
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Migrator {
    pub fn new(mut migrations: Vec<Migration>) -> Result<Self, MigrationError> {
        migrations.sort_by_key(|migration| migration.from_version);
        for window in migrations.windows(2) {
            if window[0].to_version != window[1].from_version {
                return Err(MigrationError::InvalidChain);
            }
        }
        Ok(Self { migrations })
    }
    pub fn plan(
        &self,
        source: &Path,
        current: u32,
        target: u32,
    ) -> Result<MigrationPlan, MigrationError> {
        if target <= current {
            return Err(MigrationError::Unsupported { current, target });
        }
        let original = fs::read_to_string(source)?;
        let mut migrated = original.clone();
        let mut version = current;
        while version < target {
            let migration = self
                .migrations
                .iter()
                .find(|migration| migration.from_version == version)
                .ok_or(MigrationError::Unsupported {
                    current: version,
                    target,
                })?;
            migrated = (migration.apply)(&migrated).map_err(MigrationError::Transform)?;
            version = migration.to_version;
        }
        Ok(MigrationPlan {
            source: source.into(),
            from_version: current,
            to_version: target,
            before_sha256: format!("{:x}", Sha256::digest(original.as_bytes())),
            migrated,
            backup: source.with_extension(format!(
                "{}.bak",
                source
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("source")
            )),
        })
    }
    pub fn apply(&self, plan: &MigrationPlan) -> Result<(), MigrationError> {
        let current = fs::read(&plan.source)?;
        if format!("{:x}", Sha256::digest(&current)) != plan.before_sha256 {
            return Err(MigrationError::SourceChanged);
        }
        if plan.backup.exists() {
            return Err(MigrationError::BackupExists(plan.backup.clone()));
        }
        fs::write(&plan.backup, &current)?;
        let candidate = plan.source.with_extension(format!(
            "{}.migration.tmp",
            plan.source
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("source")
        ));
        if let Err(error) =
            fs::write(&candidate, &plan.migrated).and_then(|_| fs::rename(&candidate, &plan.source))
        {
            let _ = fs::remove_file(candidate);
            return Err(error.into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum MigrationError {
    Io(std::io::Error),
    InvalidChain,
    Unsupported { current: u32, target: u32 },
    Transform(String),
    SourceChanged,
    BackupExists(PathBuf),
}
impl fmt::Display for MigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}
impl std::error::Error for MigrationError {}
impl From<std::io::Error> for MigrationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migration_is_dry_run_then_backup() {
        let root = std::env::temp_dir().join(format!("prism-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("workflow.toml");
        fs::write(&source, "schema_version=1\n").unwrap();
        let migrator = Migrator::new(vec![Migration {
            from_version: 1,
            to_version: 2,
            apply: |source| Ok(source.replace("=1", "=2")),
        }])
        .unwrap();
        let plan = migrator.plan(&source, 1, 2).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "schema_version=1\n");
        migrator.apply(&plan).unwrap();
        assert_eq!(fs::read_to_string(&source).unwrap(), "schema_version=2\n");
        assert_eq!(
            fs::read_to_string(plan.backup).unwrap(),
            "schema_version=1\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
