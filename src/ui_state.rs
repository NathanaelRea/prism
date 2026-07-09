use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tui::WorktreeListMode;
use crate::util::prism_config_dir;

#[derive(Debug, Default, Deserialize, Serialize)]
struct UiState {
    worktree_list_mode: Option<String>,
}

pub(crate) fn path() -> PathBuf {
    prism_config_dir().join("ui-state.toml")
}

pub(crate) fn load_from_path(path: &Path) -> Option<WorktreeListMode> {
    let text = fs::read_to_string(path).ok()?;
    let state = toml::from_str::<UiState>(&text).ok()?;
    parse_worktree_list_mode(state.worktree_list_mode.as_deref())
}

pub(crate) fn save_to_path(path: &Path, mode: WorktreeListMode) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create Prism config dir: {error}"))?;
    }
    let state = UiState {
        worktree_list_mode: Some(worktree_list_mode_label(mode).to_string()),
    };
    let text =
        toml::to_string_pretty(&state).map_err(|error| format!("serialize UI state: {error}"))?;
    fs::write(path, text).map_err(|error| format!("write ui-state.toml: {error}"))
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

        assert_eq!(load_from_path(&path), None);

        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, "worktree_list_mode = \"sideways\"\n").unwrap();
        assert_eq!(load_from_path(&path), None);

        fs::write(&path, "not toml").unwrap();
        assert_eq!(load_from_path(&path), None);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_and_load_worktree_list_mode() {
        let dir = unique_temp_dir("prism-ui-state-save-test");
        let path = dir.join("nested/ui-state.toml");

        save_to_path(&path, WorktreeListMode::Global).unwrap();
        assert_eq!(load_from_path(&path), Some(WorktreeListMode::Global));

        save_to_path(&path, WorktreeListMode::Repo).unwrap();
        assert_eq!(load_from_path(&path), Some(WorktreeListMode::Repo));

        let _ = fs::remove_dir_all(dir);
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{unique}", std::process::id()))
    }
}
