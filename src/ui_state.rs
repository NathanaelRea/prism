use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::file_persistence::{self, BoxError, FileContents, UpdateOptions};
use crate::tui::WorktreeListMode;
use crate::util::prism_config_dir;

#[derive(Debug, Default, Deserialize, Serialize)]
struct UiState {
    worktree_list_mode: Option<String>,
}

pub(crate) fn path() -> PathBuf {
    prism_config_dir().join("ui-state.toml")
}

pub(crate) fn load_from_path(path: &Path) -> Result<Option<WorktreeListMode>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse_state(&text).map_err(|error| format!("load {}: {error}", path.display()))
}

pub(crate) fn save_to_path(path: &Path, mode: WorktreeListMode) -> Result<(), String> {
    file_persistence::update(path, UpdateOptions::ui_state(), |contents| {
        if let FileContents::Present(bytes) = contents {
            let text = String::from_utf8(bytes).map_err(|error| Box::new(error) as BoxError)?;
            parse_state(&text).map_err(|error| {
                Box::new(io::Error::new(io::ErrorKind::InvalidData, error)) as BoxError
            })?;
        }
        let state = UiState {
            worktree_list_mode: Some(worktree_list_mode_label(mode).to_string()),
        };
        let text = toml::to_string_pretty(&state).map_err(|error| Box::new(error) as BoxError)?;
        Ok(((), Some(text.into_bytes())))
    })
    .map_err(|error| error.to_string())
}

fn parse_state(text: &str) -> Result<Option<WorktreeListMode>, String> {
    let value =
        toml::from_str::<toml::Value>(text).map_err(|error| format!("invalid TOML: {error}"))?;
    let state = value
        .try_into::<UiState>()
        .map_err(|error| format!("semantically invalid UI state: {error}"))?;
    match state.worktree_list_mode.as_deref() {
        None => Ok(None),
        value => parse_worktree_list_mode(value)
            .ok_or_else(|| {
                "semantically invalid UI state: worktree_list_mode must be 'repo' or 'all'"
                    .to_string()
            })
            .map(Some),
    }
}

fn parse_worktree_list_mode(value: Option<&str>) -> Option<WorktreeListMode> {
    match value?.trim() {
        "repo" => Some(WorktreeListMode::Repo),
        "all" | "global" => Some(WorktreeListMode::Global),
        _ => None,
    }
}

fn worktree_list_mode_label(mode: WorktreeListMode) -> &'static str {
    match mode {
        WorktreeListMode::Repo => "repo",
        WorktreeListMode::Global => "all",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn load_missing_or_invalid_state_uses_no_mode() {
        let dir = unique_temp_dir("prism-ui-state-invalid-test");
        let path = dir.join("ui-state.toml");

        assert_eq!(load_from_path(&path).unwrap(), None);

        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "worktree_list_mode = \"sideways\"\n").unwrap();
        assert!(
            load_from_path(&path)
                .unwrap_err()
                .contains("semantically invalid")
        );

        fs::write(&path, "not toml").unwrap();
        assert!(load_from_path(&path).unwrap_err().contains("invalid TOML"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_and_load_worktree_list_mode() {
        let dir = unique_temp_dir("prism-ui-state-save-test");
        let path = dir.join("nested/ui-state.toml");

        save_to_path(&path, WorktreeListMode::Global).unwrap();
        assert_eq!(
            load_from_path(&path).unwrap(),
            Some(WorktreeListMode::Global)
        );

        save_to_path(&path, WorktreeListMode::Repo).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), Some(WorktreeListMode::Repo));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_does_not_overwrite_invalid_ui_state() {
        let dir = unique_temp_dir("prism-ui-state-preserve-invalid-test");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ui-state.toml");
        let invalid = "worktree_list_mode = 42\n";
        fs::write(&path, invalid).unwrap();

        let error = save_to_path(&path, WorktreeListMode::Repo).unwrap_err();

        assert!(error.contains(&path.display().to_string()));
        assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        fs::remove_dir_all(dir).unwrap();
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{unique}", std::process::id()))
    }
}
