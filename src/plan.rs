use std::fs;
use std::path::Path;
use std::process::Command;

use crate::config::Config;
use crate::process::run_status;
use crate::repo::Repository;

pub fn infer_total_phases(path: &Path) -> Result<usize, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("read plan file: {error}"))?;
    let mut max_phase = 0;
    for line in text.lines() {
        let trimmed = line.trim_start_matches('#').trim_start();
        let Some(rest) = trimmed.strip_prefix("Phase ") else {
            continue;
        };
        let digits = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect::<String>();
        if let Ok(phase) = digits.parse::<usize>() {
            max_phase = max_phase.max(phase);
        }
    }
    Ok(max_phase)
}

pub fn run_plan_cli(repo: &Repository, config: &Config, path: &Path) -> Result<(), String> {
    let plan_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo.root.join(path)
    };
    if !plan_path.is_file() {
        return Err(format!("plan file not found: {}", plan_path.display()));
    }
    let total = infer_total_phases(&plan_path)?;
    if total == 0 {
        return Err("could not infer phases; add headings like 'Phase 1'".to_string());
    }
    run_status(
        Command::new(config.tool("codex_plan"))
            .arg("--file")
            .arg(&plan_path)
            .arg("--step-name")
            .arg("phase")
            .arg("--total")
            .arg(total.to_string())
            .arg("--start")
            .arg("1")
            .arg("--parallel")
            .arg("false")
            .current_dir(&repo.root),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_total_phases_from_markdown_headings() {
        let path = std::env::temp_dir().join(format!("prism-plan-test-{}.md", std::process::id()));
        fs::write(
            &path,
            "# Plan\n\n## Phase 1\n\nDo one.\n\n### Phase 3: later\n\nDo three.\n",
        )
        .unwrap();
        let total = infer_total_phases(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(total, 3);
    }
}
