use std::fs;
use std::path::{Path, PathBuf};

use crate::repo::Repository;
use crate::util::prism_config_dir;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoEntry {
    pub root: PathBuf,
    pub key: Option<char>,
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
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut key: Option<char> = None;

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line == "[[repos]]" {
            push_entry(&mut entries, &mut path, &mut key);
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let Some(value) = parse_string(value.trim()) else {
            continue;
        };
        match name.trim() {
            "path" => path = Some(PathBuf::from(value)),
            "key" => key = value.chars().next(),
            _ => {}
        }
    }
    push_entry(&mut entries, &mut path, &mut key);
    entries
}

fn push_entry(entries: &mut Vec<RepoEntry>, path: &mut Option<PathBuf>, key: &mut Option<char>) {
    let Some(root) = path.take() else {
        *key = None;
        return;
    };
    if !entries.iter().any(|entry| entry.root == root) {
        entries.push(RepoEntry {
            root,
            key: key.take(),
        });
    }
    *key = None;
}

fn format_entries(entries: &[RepoEntry]) -> String {
    let mut out = String::from(
        "# Prism repositories. Reorder [[repos]] blocks to change the left-panel order.\n# Remove a block to stop tracking a repository. Keys are single digits.\n\n",
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

fn parse_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in value[1..value.len() - 1].chars() {
        if escaped {
            out.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else {
            out.push(ch);
        }
    }
    Some(out)
}

fn escape_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repos_toml_in_order() {
        let entries = parse_entries(
            r#"[[repos]]
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
    fn picks_next_unused_digit_key() {
        let entries = vec![RepoEntry {
            root: PathBuf::from("/one"),
            key: Some('1'),
        }];

        assert_eq!(next_key(&entries), Some('2'));
    }
}
