#[cfg(test)]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use crate::process::run_capture;
use crate::util::{prism_config_dir, safe_path_component, stable_hash};

#[cfg(test)]
static TEST_CONFIG_DIRS: OnceLock<Mutex<BTreeMap<PathBuf, PathBuf>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
}

impl Repository {
    pub fn discover(repo_arg: Option<&Path>) -> Result<Self, String> {
        let start = match repo_arg {
            Some(path) => path.to_path_buf(),
            None => {
                std::env::current_dir().map_err(|error| format!("current directory: {error}"))?
            }
        };
        let output = run_capture(
            Command::new("git")
                .arg("-C")
                .arg(&start)
                .args(["rev-parse", "--show-toplevel"]),
        )?;
        let root = PathBuf::from(output.trim());
        if root.as_os_str().is_empty() {
            return Err("not inside a Git repository".to_string());
        }
        Ok(Self { root })
    }

    pub fn prism_dir(&self) -> PathBuf {
        #[cfg(test)]
        if let Some(config_dir) = test_config_dir(&self.root) {
            return prism_repo_dir(&self.root, &config_dir);
        }
        prism_repo_dir(&self.root, &prism_config_dir())
    }

    #[cfg(test)]
    pub fn with_config_dir_for_test(root: PathBuf, config_dir: PathBuf) -> Self {
        let repo = Self { root };
        test_config_dirs()
            .lock()
            .expect("test config dir lock poisoned")
            .insert(repo.root.clone(), config_dir);
        repo
    }
}

#[cfg(test)]
fn test_config_dirs() -> &'static Mutex<BTreeMap<PathBuf, PathBuf>> {
    TEST_CONFIG_DIRS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(test)]
fn test_config_dir(root: &Path) -> Option<PathBuf> {
    test_config_dirs().lock().ok()?.get(root).cloned()
}

fn prism_repo_dir(root: &Path, config_dir: &Path) -> PathBuf {
    let repo_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .map(safe_path_component)
        .unwrap_or_else(|| "repo".to_string());
    let hash = stable_hash(root);
    config_dir
        .join("repos")
        .join(format!("{repo_name}-{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Repository, prism_repo_dir};

    #[test]
    fn prism_dir_uses_user_config_area_not_repo_root() {
        let repo = Repository {
            root: PathBuf::from("/work/my repo"),
        };
        let path = prism_repo_dir(&repo.root, Path::new("/home/me/.config/prism"));

        assert_eq!(
            path,
            PathBuf::from("/home/me/.config/prism/repos/my_repo-76df80f48cebc666")
        );
        assert!(!path.starts_with(&repo.root));
    }
}
