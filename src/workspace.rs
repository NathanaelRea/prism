use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

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

pub fn load_entries() -> Vec<RepoEntry> {
    let Ok(text) = fs::read_to_string(repos_path()) else {
        return Vec::new();
    };
    parse_entries(&text)
}

pub fn save_entries(entries: &[RepoEntry]) -> Result<(), String> {
    let path = repos_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create Prism config dir: {error}"))?;
    }
    fs::write(path, format_entries(entries)).map_err(|error| format!("write repos.toml: {error}"))
}

pub fn ensure_repo_entry(path: &Path) -> Result<(Repository, usize, Vec<RepoEntry>), String> {
    let repo = Repository::discover(Some(path))?;
    let mut entries = load_entries();
    if let Some(index) = entries.iter().position(|entry| entry.root == repo.root) {
        return Ok((repo, index, entries));
    }
    let key = next_key(&entries);
    entries.push(RepoEntry {
        root: repo.root.clone(),
        key,
    });
    save_entries(&entries)?;
    Ok((repo, entries.len() - 1, entries))
}

pub fn ensure_entries_for_tui(repo_arg: Option<&Path>) -> Result<(Vec<RepoEntry>, usize), String> {
    if let Some(path) = repo_arg {
        let (_, index, entries) = ensure_repo_entry(path)?;
        return Ok((entries, index));
    }

    let entries = load_entries();
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

fn parse_entries(text: &str) -> Vec<RepoEntry> {
    let Ok(raw) = toml::from_str::<RawRepos>(text) else {
        return Vec::new();
    };
    raw.repos
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let root = entry.path?;
            let key = entry.key.and_then(|value| value.chars().next());
            Some(RepoEntry { root, key })
        })
        .fold(Vec::new(), |mut entries, entry| {
            if !entries.iter().any(|existing| existing.root == entry.root) {
                entries.push(entry);
            }
            entries
        })
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
        );

        assert_eq!(entries[0].root, PathBuf::from("/one"));
        assert_eq!(entries[0].key, Some('2'));
        assert_eq!(entries[1].root, PathBuf::from("/two"));
        assert_eq!(entries[1].key, Some('1'));
    }

    #[test]
    fn parses_repos_toml_escaped_strings_and_skips_missing_fields() {
        let entries = parse_entries(
            r#"[[repos]]
path = "/tmp/repo \"quoted\""
key = "9"

[[repos]]
key = "1"

[[repos]]
path = "/tmp/repo \"quoted\""
key = "8"
"#,
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].root, PathBuf::from("/tmp/repo \"quoted\""));
        assert_eq!(entries[0].key, Some('9'));
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
