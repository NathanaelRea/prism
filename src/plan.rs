use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::Config;
#[cfg(test)]
use crate::json::json_string_field;
use crate::observability;
use crate::plan_run::{
    DEFAULT_OUTPUT_LINES_PER_STEP, PlanExecutorConfig, PlanLaunch, PlanRunMode,
    execute_plan_sequential, load_resumable_plan_run, prepare_plan_plugin_config,
    prepare_plan_run_for_resume, save_plan_run,
};
use crate::process::command_exists;
use crate::repo::Repository;
use crate::util::stable_hash;

const DEFAULT_STEP_NAME: &str = "phase";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanFileSource {
    Explicit,
    Selected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedPlanFile {
    path: PathBuf,
    source: PlanFileSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanExecution {
    cwd: PathBuf,
    plan_path: PathBuf,
    plan_file: String,
    step_name: String,
    start: usize,
    total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
struct PlanModeLaunch {
    cwd: PathBuf,
    tmux_session_name: String,
    shell_command: String,
}

#[allow(dead_code)]
impl PlanModeLaunch {
    fn prepare(cwd: &Path) -> Result<Self, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("resolve prism executable: {error}"))?;
        let command = [
            shell_quote(&exe.to_string_lossy()),
            "--repo".to_string(),
            shell_quote(&cwd.to_string_lossy()),
            "plan".to_string(),
        ]
        .join(" ");
        Ok(Self {
            cwd: cwd.to_path_buf(),
            tmux_session_name: plan_mode_session_name(cwd),
            shell_command: format!(
                "{command}; status=$?; printf '\\n[prism plan mode exited with status %s]\\nPress Enter to close this tmux session. ' \"$status\"; read _; exit \"$status\""
            ),
        })
    }
}

impl PlanExecution {
    pub(crate) fn prepare(
        cwd: &Path,
        config: &Config,
        path: Option<&Path>,
    ) -> Result<Self, String> {
        let selected = select_plan_file(cwd, config, path)?;
        let inferred_total = infer_total_phases(&selected.path)?;
        let plan_file = display_plan_path(cwd, &selected.path);

        let step_range = match selected.source {
            PlanFileSource::Explicit => StepRange::inferred(inferred_total)?,
            PlanFileSource::Selected => StepRange::prompt(inferred_total)?,
        };

        Ok(Self {
            cwd: cwd.to_path_buf(),
            plan_path: selected.path,
            plan_file,
            step_name: step_range.name,
            start: step_range.start,
            total: step_range.total,
        })
    }

    pub(crate) fn launch(&self, repo_root: &Path, mode: PlanRunMode) -> Result<PlanLaunch, String> {
        PlanLaunch::new(
            repo_root,
            &self.cwd,
            &self.plan_path,
            &self.step_name,
            self.start,
            self.total,
            mode,
        )
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[cfg(test)]
    fn tasks(&self) -> Vec<String> {
        (self.start..=self.total)
            .map(|step| build_task(&self.plan_file, &self.step_name, step))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StepRange {
    name: String,
    start: usize,
    total: usize,
}

impl StepRange {
    fn inferred(total: usize) -> Result<Self, String> {
        if total == 0 {
            return Err("could not infer phases; add headings like 'Phase 1'".to_string());
        }
        Ok(Self {
            name: DEFAULT_STEP_NAME.to_string(),
            start: 1,
            total,
        })
    }

    fn prompt(inferred_total: usize) -> Result<Self, String> {
        let name = prompt_string("Step name", DEFAULT_STEP_NAME)?;
        let total = prompt_usize(
            "How many total steps",
            (inferred_total > 0).then_some(inferred_total),
        )?;
        let start = prompt_usize("Where to start", Some(1))?;
        if start > total {
            return Err("start step cannot be greater than total steps".to_string());
        }
        Ok(Self { name, start, total })
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpencodeRunCommand {
    program: String,
    args: Vec<String>,
}

#[cfg(test)]
impl OpencodeRunCommand {
    fn new(config: &Config, task: &str) -> Self {
        Self {
            program: config.tool("opencode"),
            args: vec![
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
                task.to_string(),
            ],
        }
    }

    fn display(&self) -> String {
        format!("$ {} {}", self.program, self.args.join(" "))
    }
}

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

pub fn run_plan_mode(cwd: &Path, config: &Config, path: Option<&Path>) -> Result<(), String> {
    let execution = PlanExecution::prepare(cwd, config, path)?;
    let repo = Repository {
        root: execution.cwd.clone(),
    };
    let launch = PlanLaunch::new(
        &repo.root,
        &execution.cwd,
        &execution.plan_path,
        &execution.step_name,
        execution.start,
        execution.total,
        PlanRunMode::Sequential,
    )?;
    let server_url = match crate::opencode::ensure_opencode_server(
        &repo,
        config,
        "plan",
        &execution.cwd,
    ) {
        Ok(runtime) => Some(runtime.server_url),
        Err(error) => {
            eprintln!(
                "warning: could not start OpenCode server for attach; falling back to direct opencode run: {error}"
            );
            None
        }
    };

    let mut executor = PlanExecutorConfig::new(
        config.tool("opencode"),
        server_url,
        execution.cwd.clone(),
        execution.plan_file.clone(),
    );
    if config.opencode_plan_plugin
        && let Ok(plugin) = prepare_plan_plugin_config(&repo.prism_dir())
    {
        executor = executor.with_plugin_config(plugin);
    }
    observability::with_writable_db(&repo, |conn| {
        let mut persisted = if let Some(mut persisted) = load_resumable_plan_run(conn, &launch)? {
            if !prepare_plan_run_for_resume(conn, &mut persisted, DEFAULT_OUTPUT_LINES_PER_STEP)? {
                return Err("matching plan run is already running".to_string());
            }
            persisted
        } else {
            let persisted = launch.create_run();
            save_plan_run(conn, &persisted)?;
            persisted
        };
        execute_plan_sequential(conn, &mut persisted, &executor, &mut io::stdout())
    })
}

#[allow(dead_code)]
pub fn open_plan_mode(config: &Config, cwd: &Path) -> Result<(), String> {
    let launch = PlanModeLaunch::prepare(cwd)?;
    crate::tmux::attach_or_create_plan_mode(
        config,
        &launch.tmux_session_name,
        &launch.cwd,
        &launch.shell_command,
    )
}

#[cfg(test)]
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

#[cfg(test)]
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

fn select_plan_file(
    cwd: &Path,
    config: &Config,
    path: Option<&Path>,
) -> Result<SelectedPlanFile, String> {
    if let Some(path) = path {
        let plan_path = resolve_path(cwd, path);
        if !plan_path.is_file() {
            return Err(format!("plan file not found: {}", plan_path.display()));
        }
        return Ok(SelectedPlanFile {
            path: plan_path,
            source: PlanFileSource::Explicit,
        });
    }

    if !command_exists(&config.tool("fzf")) {
        return Err("fzf was not found on PATH".to_string());
    }
    let files = list_markdown_files(cwd)?;
    if files.is_empty() {
        return Err("no markdown files found under the selected directory".to_string());
    }
    let selected = choose_with_fzf(config, &files)?;
    Ok(SelectedPlanFile {
        path: cwd.join(selected),
        source: PlanFileSource::Selected,
    })
}

pub(crate) fn select_plan_path(cwd: &Path, config: &Config) -> Result<PathBuf, String> {
    select_plan_file(cwd, config, None).map(|selected| selected.path)
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

pub(crate) fn display_plan_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

pub(crate) fn build_task(plan_file: &str, step_name: &str, step: usize) -> String {
    format!("Implement {plan_file} {step_name} {step}")
}

#[allow(dead_code)]
fn plan_mode_session_name(cwd: &Path) -> String {
    format!("prism-plan-{:016x}", stable_hash(cwd))
}

#[allow(dead_code)]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
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
    use crate::config::Config;

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
    fn prepares_explicit_plan_execution_from_inferred_phases() {
        let dir = unique_temp_dir("prism-plan-execution-test");
        let path = dir.join("plan-arch.md");
        fs::write(
            &path,
            "# Plan\n\n## Phase 1\n\nDo one.\n\n## Phase 2\n\nDo two.\n",
        )
        .unwrap();
        let config = test_config();

        let execution = PlanExecution::prepare(&dir, &config, Some(Path::new("plan-arch.md")))
            .expect("explicit plan execution");

        assert_eq!(execution.plan_path, path);
        assert_eq!(
            execution.tasks(),
            vec![
                "Implement plan-arch.md phase 1".to_string(),
                "Implement plan-arch.md phase 2".to_string(),
            ]
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_explicit_plan_without_phase_headings() {
        let dir = unique_temp_dir("prism-plan-empty-test");
        let path = dir.join("notes.md");
        fs::write(&path, "# Notes\n\nNo numbered phases.\n").unwrap();
        let config = test_config();

        let error = PlanExecution::prepare(&dir, &config, Some(Path::new("notes.md")))
            .expect_err("explicit phase inference should fail");

        assert_eq!(error, "could not infer phases; add headings like 'Phase 1'");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn builds_opencode_json_run_invocation() {
        let mut config = test_config();
        config
            .tools
            .insert("opencode".to_string(), "/bin/opencode-test".to_string());

        let command = OpencodeRunCommand::new(&config, "Implement plan.md phase 3");

        assert_eq!(command.program, "/bin/opencode-test");
        assert_eq!(
            command.args,
            vec![
                "run".to_string(),
                "--format".to_string(),
                "json".to_string(),
                "Implement plan.md phase 3".to_string(),
            ]
        );
        assert_eq!(
            command.display(),
            "$ /bin/opencode-test run --format json Implement plan.md phase 3"
        );
    }

    #[test]
    fn prepares_plan_mode_launch_for_cli_execution_in_tmux() {
        let dir = PathBuf::from("/repo/my project");

        let launch = PlanModeLaunch::prepare(&dir).expect("plan mode launch");

        assert!(launch.tmux_session_name.starts_with("prism-plan-"));
        assert_eq!(launch.cwd, dir);
        assert!(
            launch
                .shell_command
                .contains("--repo '/repo/my project' plan")
        );
        assert!(launch.shell_command.contains("status=$?"));
        assert!(
            launch
                .shell_command
                .contains("[prism plan mode exited with status %s]")
        );
    }

    #[test]
    fn shell_quote_preserves_plan_launch_argument_boundaries() {
        assert_eq!(shell_quote("opencode"), "opencode");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("that's"), "'that'\"'\"'s'");
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

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_config() -> Config {
        let mut config = crate::test_support::test_config();
        config.default_agent = "opencode".to_string();
        config.default_base = Some("main".to_string());
        config.worktree_columns = vec!["url".to_string()];
        config
    }
}
