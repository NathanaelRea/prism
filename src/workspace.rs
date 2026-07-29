use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::file_persistence::{self, BoxError, FileContents, UpdateOptions};
use crate::observability::{self, LogLevel};
use crate::repo::Repository;
use crate::util::prism_config_dir;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoEntry {
    pub root: PathBuf,
    pub key: Option<char>,
}

#[derive(Clone, Debug)]
pub struct DiscoveredRepoEntry {
    pub repo: Repository,
    pub key: Option<char>,
    pub source_index: usize,
}

#[derive(Debug, Default, Deserialize)]
struct RawRepos {
    repos: Option<Vec<RawRepoEntry>>,
}

#[derive(Debug, Deserialize)]
struct RawRepoEntry {
    path: Option<PathBuf>,
    key: Option<String>,
}

pub fn repos_path() -> PathBuf {
    prism_config_dir().join("repos.toml")
}

#[derive(Debug)]
enum ReposFormatError {
    Utf8(std::string::FromUtf8Error),
    Toml(toml::de::Error),
    Semantic(String),
}

impl fmt::Display for ReposFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8(error) => write!(formatter, "repository file is unreadable text: {error}"),
            Self::Toml(error) => write!(formatter, "repository file has invalid TOML: {error}"),
            Self::Semantic(error) => write!(
                formatter,
                "repository file is semantically invalid: {error}"
            ),
        }
    }
}

impl Error for ReposFormatError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Utf8(error) => Some(error),
            Self::Toml(error) => Some(error),
            Self::Semantic(_) => None,
        }
    }
}

pub fn load_entries() -> Result<Vec<RepoEntry>, String> {
    load_entries_from_path(&repos_path())
}

fn load_entries_from_path(path: &Path) -> Result<Vec<RepoEntry>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse_entries(&text).map_err(|error| format!("load {}: {error}", path.display()))
}

pub fn initialize_entries(entries: &[RepoEntry]) -> Result<Vec<RepoEntry>, String> {
    update_entries(&repos_path(), |current, missing| {
        if missing {
            *current = entries.to_vec();
            Ok((current.clone(), true))
        } else {
            Ok((current.clone(), false))
        }
    })
}

pub fn replace_entries(
    expected: &[RepoEntry],
    replacement: &[RepoEntry],
) -> Result<Vec<RepoEntry>, String> {
    update_entries(&repos_path(), |current, _| {
        if current != expected {
            return Err(ReposFormatError::Semantic(
                "repos.toml changed while the dialog was open; reopen the dialog and retry"
                    .to_string(),
            ));
        }
        *current = replacement.to_vec();
        Ok((current.clone(), true))
    })
}

pub fn ensure_repo_entry(path: &Path) -> Result<(Repository, usize, Vec<RepoEntry>), String> {
    let repo = Repository::discover(Some(path))?;
    let root = repo.root.clone();
    let (index, entries) = ensure_repo_root(&repos_path(), root)?;
    Ok((repo, index, entries))
}

fn ensure_repo_root(path: &Path, root: PathBuf) -> Result<(usize, Vec<RepoEntry>), String> {
    update_entries(path, move |entries, _| {
        if let Some(index) = entries.iter().position(|entry| entry.root == root) {
            return Ok(((index, entries.clone()), false));
        }
        let key = next_key(entries);
        entries.push(RepoEntry { root, key });
        Ok(((entries.len() - 1, entries.clone()), true))
    })
}

pub fn ensure_entries_for_tui(repo_arg: Option<&Path>) -> Result<(Vec<RepoEntry>, usize), String> {
    if let Some(path) = repo_arg {
        let (_, index, entries) = ensure_repo_entry(path)?;
        return Ok((entries, index));
    }

    let entries = load_entries()?;
    if !entries.is_empty() {
        return Ok((entries, 0));
    }
    Ok((entries, 0))
}

pub fn discover_valid_entries(entries: Vec<RepoEntry>) -> Vec<DiscoveredRepoEntry> {
    let mut discovered = Vec::new();
    for (source_index, entry) in entries.into_iter().enumerate() {
        match Repository::discover(Some(&entry.root)) {
            Ok(repo) => discovered.push(DiscoveredRepoEntry {
                repo,
                key: entry.key,
                source_index,
            }),
            Err(error) => observability::emit(observability::EventInput {
                level: LogLevel::Warn,
                target: "workspace",
                action: "skip_repo",
                operation_id: None,
                parent_operation_id: None,
                branch: None,
                session: None,
                message: format!(
                    "skipping configured repository {}: {error}",
                    entry.root.display()
                ),
                data_json: None,
            }),
        }
    }
    discovered
}

pub fn remove_missing_entries(
    entries: Vec<RepoEntry>,
    selected: usize,
) -> Result<(Vec<RepoEntry>, usize), String> {
    let selected_root = entries.get(selected).map(|entry| entry.root.clone());
    let (retained, removed) = update_entries(&repos_path(), |current, missing| {
        if missing {
            return Ok(((current.clone(), Vec::new()), false));
        }
        let removed = current
            .iter()
            .filter(|entry| !entry.root.exists())
            .map(|entry| entry.root.clone())
            .collect::<Vec<_>>();
        current.retain(|entry| entry.root.exists());
        Ok(((current.clone(), removed.clone()), !removed.is_empty()))
    })?;
    for root in removed {
        observability::emit(observability::EventInput {
            level: LogLevel::Info,
            target: "workspace",
            action: "remove_missing_repo",
            operation_id: None,
            parent_operation_id: None,
            branch: None,
            session: None,
            message: format!("removing missing repository {}", root.display()),
            data_json: None,
        });
    }
    let selected = selected_root
        .and_then(|root| retained.iter().position(|entry| entry.root == root))
        .unwrap_or_else(|| selected.min(retained.len().saturating_sub(1)));
    Ok((retained, selected))
}

#[cfg(test)]
fn retain_existing_entries(entries: Vec<RepoEntry>, selected: usize) -> (Vec<RepoEntry>, usize) {
    let selected_root = entries.get(selected).map(|entry| entry.root.clone());
    let retained: Vec<_> = entries
        .into_iter()
        .filter(|entry| entry.root.exists())
        .collect();
    let selected = selected_root
        .and_then(|root| retained.iter().position(|entry| entry.root == root))
        .unwrap_or_else(|| selected.min(retained.len().saturating_sub(1)));
    (retained, selected)
}

pub fn label_for_root(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("repo")
        .to_string()
}

pub fn next_key(entries: &[RepoEntry]) -> Option<char> {
    ('1'..='9').find(|candidate| !entries.iter().any(|entry| entry.key == Some(*candidate)))
}

fn update_entries<T>(
    path: &Path,
    transform: impl FnOnce(&mut Vec<RepoEntry>, bool) -> Result<(T, bool), ReposFormatError>,
) -> Result<T, String> {
    file_persistence::update(path, UpdateOptions::important_toml(), |contents| {
        let missing = matches!(contents, FileContents::Missing);
        let mut entries = match contents {
            FileContents::Missing => Vec::new(),
            FileContents::Present(bytes) => {
                let text = String::from_utf8(bytes)
                    .map_err(|error| Box::new(ReposFormatError::Utf8(error)) as BoxError)?;
                parse_entries(&text).map_err(|error| Box::new(error) as BoxError)?
            }
        };
        let (value, changed) =
            transform(&mut entries, missing).map_err(|error| Box::new(error) as BoxError)?;
        let replacement = changed.then(|| format_entries(&entries).into_bytes());
        Ok((value, replacement))
    })
    .map_err(|error| error.to_string())
}

fn parse_entries(text: &str) -> Result<Vec<RepoEntry>, ReposFormatError> {
    let value = toml::from_str::<toml::Value>(text).map_err(ReposFormatError::Toml)?;
    let raw = value
        .try_into::<RawRepos>()
        .map_err(|error| ReposFormatError::Semantic(error.to_string()))?;
    let mut entries = Vec::new();
    for (index, raw) in raw.repos.unwrap_or_default().into_iter().enumerate() {
        let root = raw.path.ok_or_else(|| {
            ReposFormatError::Semantic(format!("repos entry {} is missing path", index + 1))
        })?;
        if root.as_os_str().is_empty() {
            return Err(ReposFormatError::Semantic(format!(
                "repos entry {} has an empty path",
                index + 1
            )));
        }
        let key = raw
            .key
            .map(|value| {
                let mut chars = value.chars();
                let key = chars.next().filter(|key| ('1'..='9').contains(key));
                if key.is_none() || chars.next().is_some() {
                    Err(ReposFormatError::Semantic(format!(
                        "repos entry {} has invalid key {:?}; expected one digit from 1 through 9",
                        index + 1,
                        value
                    )))
                } else {
                    Ok(key)
                }
            })
            .transpose()?
            .flatten();
        if entries.iter().any(|entry: &RepoEntry| entry.root == root) {
            return Err(ReposFormatError::Semantic(format!(
                "repository path {} is listed more than once",
                root.display()
            )));
        }
        if let Some(key) = key
            && entries.iter().any(|entry| entry.key == Some(key))
        {
            return Err(ReposFormatError::Semantic(format!(
                "repository shortcut {key} is assigned more than once"
            )));
        }
        entries.push(RepoEntry { root, key });
    }
    Ok(entries)
}

fn format_entries(entries: &[RepoEntry]) -> String {
    let mut out = String::from(
        "# Prism repositories. Reorder [[repos]] blocks to change the repo panel order.\n# Remove a block to stop tracking a repository. Keys are Space <digit> shortcuts.\n\n",
    );
    for entry in entries {
        out.push_str("[[repos]]\n");
        out.push_str(&format!(
            "path = \"{}\"\n",
            escape_string(&entry.root.display().to_string())
        ));
        if let Some(key) = entry.key {
            out.push_str(&format!("key = \"{}\"\n", escape_string(&key.to_string())));
        }
        out.push('\n');
    }
    out
}

fn escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_repos_toml_in_order() {
        let entries = parse_entries(
            r#"# comment
[[repos]]
path = "/one"
key = "2"

[[repos]]
path = "/two"
key = "1"
"#,
        )
        .unwrap();

        assert_eq!(entries[0].root, PathBuf::from("/one"));
        assert_eq!(entries[0].key, Some('2'));
        assert_eq!(entries[1].root, PathBuf::from("/two"));
        assert_eq!(entries[1].key, Some('1'));
    }

    #[test]
    fn semantically_invalid_repos_are_rejected() {
        let error = parse_entries(
            r#"[[repos]]
path = "/tmp/repo \"quoted\""
key = "9"

[[repos]]
key = "1"

[[repos]]
path = "/tmp/repo \"quoted\""
key = "8"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("missing path"));
    }

    #[test]
    fn picks_next_unused_digit_key() {
        let entries = vec![RepoEntry {
            root: PathBuf::from("/one"),
            key: Some('1'),
        }];

        assert_eq!(next_key(&entries), Some('2'));
    }

    #[test]
    fn discover_valid_entries_skips_missing_repositories() {
        let temp = unique_temp_dir("prism-workspace-discover-test");
        let repo_path = temp.join("repo");
        fs::create_dir_all(&repo_path).unwrap();
        run(Command::new("git").arg("-C").arg(&repo_path).args(["init"]));

        let entries = vec![
            RepoEntry {
                root: repo_path.clone(),
                key: Some('1'),
            },
            RepoEntry {
                root: temp.join("missing"),
                key: Some('2'),
            },
        ];

        let discovered = discover_valid_entries(entries);
        let expected_repo_path = fs::canonicalize(&repo_path).unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].repo.root, expected_repo_path);
        assert_eq!(discovered[0].key, Some('1'));
        assert_eq!(discovered[0].source_index, 0);

        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn removes_missing_entries_and_preserves_selected_repository() {
        let temp = unique_temp_dir("prism-workspace-cleanup-test");
        let first = temp.join("first");
        let selected = temp.join("selected");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&selected).unwrap();
        let entries = vec![
            RepoEntry {
                root: first.clone(),
                key: Some('1'),
            },
            RepoEntry {
                root: temp.join("missing"),
                key: Some('2'),
            },
            RepoEntry {
                root: selected.clone(),
                key: Some('3'),
            },
        ];

        let (retained, selected_index) = retain_existing_entries(entries, 2);

        assert_eq!(retained.len(), 2);
        assert_eq!(selected_index, 1);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn concurrent_repository_additions_retain_both_entries() {
        let temp = unique_temp_dir("prism-workspace-concurrent-test");
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("repos.toml");
        let first_path = path.clone();
        let second_path = path.clone();

        let first = thread::spawn(move || {
            ensure_repo_root(&first_path, PathBuf::from("/first")).unwrap();
        });
        let second = thread::spawn(move || {
            ensure_repo_root(&second_path, PathBuf::from("/second")).unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();

        let entries = load_entries_from_path(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.root == Path::new("/first"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.root == Path::new("/second"))
        );
        assert!(crate::file_persistence::adjacent_lock_path(&path).exists());
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn invalid_repos_toml_is_not_overwritten_by_a_mutator() {
        let temp = unique_temp_dir("prism-workspace-invalid-test");
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("repos.toml");
        let invalid = "[[repos]\npath = '/old'\n";
        fs::write(&path, invalid).unwrap();

        let error = ensure_repo_root(&path, PathBuf::from("/new")).unwrap_err();

        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("invalid TOML"));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn semantically_invalid_repos_toml_is_not_overwritten_by_a_mutator() {
        let temp = unique_temp_dir("prism-workspace-semantic-invalid-test");
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("repos.toml");
        let invalid = "[[repos]]\npath = '/old'\nkey = '12'\n";
        fs::write(&path, invalid).unwrap();

        let error = ensure_repo_root(&path, PathBuf::from("/new")).unwrap_err();

        assert!(error.contains("semantically invalid"));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn unreadable_repos_path_is_reported_and_not_replaced() {
        let temp = unique_temp_dir("prism-workspace-unreadable-test");
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("repos.toml");
        fs::create_dir(&path).unwrap();

        let load_error = load_entries_from_path(&path).unwrap_err();
        let mutation_error = ensure_repo_root(&path, PathBuf::from("/new")).unwrap_err();

        assert!(load_error.contains(&path.display().to_string()));
        assert!(mutation_error.contains(&path.display().to_string()));
        assert!(path.is_dir());
        fs::remove_dir_all(temp).unwrap();
    }

    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "command failed: {:?}\nstdout: {}\nstderr: {}",
            command,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }
}
