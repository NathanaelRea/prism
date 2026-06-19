use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::json::json_string_field;
use crate::process::command_exists;
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

pub fn run_plan_cli(repo: &Repository, config: &Config, path: Option<&Path>) -> Result<(), String> {
    run_plan_mode(&repo.root, config, path)
}

pub fn run_plan_mode(cwd: &Path, config: &Config, path: Option<&Path>) -> Result<(), String> {
    let plan_path = select_plan_file(cwd, config, path)?;
    let plan_file = display_plan_path(cwd, &plan_path);
    let inferred_total = infer_total_phases(&plan_path)?;

    let (step_name, total, start) = if path.is_some() {
        if inferred_total == 0 {
            return Err("could not infer phases; add headings like 'Phase 1'".to_string());
        }
        ("phase".to_string(), inferred_total, 1)
    } else {
        let step_name = prompt_string("Step name", "phase")?;
        let total = prompt_usize(
            "How many total steps",
            (inferred_total > 0).then_some(inferred_total),
        )?;
        let start = prompt_usize("Where to start", Some(1))?;
        if start > total {
            return Err("start step cannot be greater than total steps".to_string());
        }
        (step_name, total, start)
    };

    for step in start..=total {
        let task = build_task(&plan_file, &step_name, step);
        println!("\n==> {task}\n");
        run_opencode_step(config, cwd, &task)?;
    }
    Ok(())
}

fn run_opencode_step(config: &Config, cwd: &Path, task: &str) -> Result<(), String> {
    println!("$ opencode run --format json {task}");
    let mut child = Command::new(config.tool("opencode"))
        .arg("run")
        .arg("--format")
        .arg("json")
        .arg(task)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("opencode: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "open opencode stdout".to_string())?;
    let reader = BufReader::new(stdout);
    for line in reader.lines() {
        let line = line.map_err(|error| format!("read opencode output: {error}"))?;
        for rendered in render_opencode_event(&line) {
            println!("{rendered}");
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("wait for opencode: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("opencode run exited with {status}"))
    }
}

fn render_opencode_event(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    if !raw.starts_with('{') {
        return vec![raw.to_string()];
    }

    let event_type = json_string_field(raw, "type")
        .or_else(|| json_string_field(raw, "event"))
        .unwrap_or_else(|| "event".to_string());
    let mut lines = Vec::new();
    for key in [
        "command", "text", "content", "message", "output", "error", "summary", "title", "name",
        "status", "path",
    ] {
        let Some(value) = json_string_field(raw, key) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        if matches!(key, "text" | "content" | "message") {
            lines.push(format!("[{event_type}] {value}"));
        } else {
            lines.push(format!("[{event_type}] {key}: {value}"));
        }
    }

    if lines.is_empty() || event_should_include_raw(raw, &event_type) {
        lines.push(format!("[{event_type}] json: {raw}"));
    }
    lines
}

fn event_should_include_raw(raw: &str, event_type: &str) -> bool {
    let event_type = event_type.to_ascii_lowercase();
    event_type.contains("tool")
        || event_type.contains("call")
        || event_type.contains("command")
        || event_type.contains("patch")
        || raw.contains("\"tool\"")
        || raw.contains("\"input\"")
        || raw.contains("\"arguments\"")
}

fn select_plan_file(cwd: &Path, config: &Config, path: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = path {
        let plan_path = resolve_path(cwd, path);
        if !plan_path.is_file() {
            return Err(format!("plan file not found: {}", plan_path.display()));
        }
        return Ok(plan_path);
    }

    if !command_exists(&config.tool("fzf")) {
        return Err("fzf was not found on PATH".to_string());
    }
    let files = list_markdown_files(cwd)?;
    if files.is_empty() {
        return Err("no markdown files found under the selected directory".to_string());
    }
    let selected = choose_with_fzf(config, &files)?;
    Ok(cwd.join(selected))
}

fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn list_markdown_files(cwd: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_markdown_files(cwd, cwd, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_markdown_files(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name == ".git" {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("stat {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_markdown_files(root, &path, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .map(|extension| extension.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        {
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            files.push(relative);
        }
    }
    Ok(())
}

fn choose_with_fzf(config: &Config, files: &[PathBuf]) -> Result<PathBuf, String> {
    let input = files
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut child = Command::new(config.tool("fzf"))
        .arg("--prompt")
        .arg("Plan file> ")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("fzf: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input.as_bytes())
            .map_err(|error| format!("write fzf input: {error}"))?;
    } else {
        return Err("open fzf stdin".to_string());
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for fzf: {error}"))?;
    if !output.status.success() {
        return Err("no plan file selected".to_string());
    }
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if selected.is_empty() {
        return Err("no plan file selected".to_string());
    }
    Ok(PathBuf::from(selected))
}

fn display_plan_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn build_task(plan_file: &str, step_name: &str, step: usize) -> String {
    format!("Implement {plan_file} {step_name} {step}")
}

fn prompt_string(label: &str, default: &str) -> Result<String, String> {
    print!("{label} [{default}]: ");
    io::stdout()
        .flush()
        .map_err(|error| format!("flush stdout: {error}"))?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| format!("read stdin: {error}"))?;
    let input = input.trim();
    if input.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(input.to_string())
    }
}

fn prompt_usize(label: &str, default: Option<usize>) -> Result<usize, String> {
    loop {
        if let Some(default) = default {
            print!("{label} [{default}]: ");
        } else {
            print!("{label}: ");
        }
        io::stdout()
            .flush()
            .map_err(|error| format!("flush stdout: {error}"))?;
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|error| format!("read stdin: {error}"))?;
        let input = input.trim();
        if input.is_empty() {
            if let Some(default) = default {
                return Ok(default);
            }
        } else if let Ok(value) = input.parse::<usize>()
            && value > 0
        {
            return Ok(value);
        }
        println!("Please enter a positive number.");
    }
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

    #[test]
    fn renders_opencode_text_events() {
        let lines = render_opencode_event(
            r#"{"type":"message","message":"working on phase 1","status":"running"}"#,
        );

        assert!(lines.contains(&"[message] working on phase 1".to_string()));
        assert!(lines.contains(&"[message] status: running".to_string()));
    }

    #[test]
    fn renders_tool_events_with_raw_json_fallback() {
        let raw = r#"{"type":"tool.call","name":"bash","input":{"command":"cargo test"},"status":"running"}"#;
        let lines = render_opencode_event(raw);

        assert!(lines.contains(&"[tool.call] name: bash".to_string()));
        assert!(lines.contains(&"[tool.call] command: cargo test".to_string()));
        assert!(
            lines
                .iter()
                .any(|line| line == &format!("[tool.call] json: {raw}"))
        );
    }
}
