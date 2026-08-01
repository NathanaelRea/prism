use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::observability;
use crate::process::{
    ProcessDescriptor, ProcessOutput, ProcessPolicy, run_capture_named,
    run_output_allow_failure_named, run_output_named, run_status_inherited_named,
};
use crate::repo::Repository;

pub(crate) const LIST_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.list");
pub(crate) const SWITCH_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.switch");
#[allow(dead_code)]
pub(crate) const REMOVE_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.remove");
pub(crate) const APPROVALS_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.approvals");
#[allow(dead_code)]
pub(crate) const LOGS_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.logs");

static APPROVAL_FAILURE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?is)needs\s+approval.*cannot\s+prompt.*non[- ]interactive").unwrap()
});
static HOOK_FAILURE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(?:hook|command)\b.*\bfailed\b").unwrap());
static GIT_FAILURE_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)(?:fatal:|\bgit\b.*\bfailed\b)").unwrap());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalStatus {
    NotWorktrunk,
    Approved,
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchAction {
    Created,
    Existing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SwitchOutcome {
    pub action: SwitchAction,
    pub path: PathBuf,
    pub branch: String,
    pub created_branch: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    ApprovalRequired,
    Hook,
    Git,
    MalformedOutput,
    Process,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktrunkFailure {
    pub kind: FailureKind,
    command: String,
    summary: String,
    stdout: Box<str>,
    stderr: Box<str>,
    approval_hint: Option<String>,
}

impl WorktrunkFailure {
    pub(crate) fn approval_required(&self) -> bool {
        self.kind == FailureKind::ApprovalRequired
    }

    fn from_output(command: &Command, output: &ProcessOutput) -> Self {
        let combined = format!("{}\n{}", output.stdout, output.stderr);
        let kind = classify_failure(&combined);
        let summary = if kind == FailureKind::ApprovalRequired {
            safe_summary(combined.trim())
        } else {
            safe_summary(&process_failure_message(output))
        };
        Self {
            kind,
            command: observability::command_display(command),
            summary,
            stdout: bounded_context(&output.stdout),
            stderr: bounded_context(&output.stderr),
            approval_hint: None,
        }
    }

    fn malformed(command: &Command, output: &ProcessOutput, error: impl fmt::Display) -> Self {
        Self {
            kind: FailureKind::MalformedOutput,
            command: observability::command_display(command),
            summary: format!("invalid Worktrunk JSON output: {error}"),
            stdout: bounded_context(&output.stdout),
            stderr: bounded_context(&output.stderr),
            approval_hint: None,
        }
    }

    fn process(command: &Command, error: String) -> Self {
        let command_display = observability::command_display(command);
        let summary = error
            .strip_prefix(&format!("{command_display}: "))
            .unwrap_or(&error)
            .to_string();
        Self {
            kind: FailureKind::Process,
            command: command_display,
            summary,
            stdout: Box::default(),
            stderr: Box::default(),
            approval_hint: None,
        }
    }

    fn with_approval_hint(mut self, repo: &Repository, config: &Config) -> Self {
        if self.approval_required() {
            self.approval_hint = Some(format!(
                "This repo has Worktrunk project commands that must be approved before Prism can create worktrees.\n\nRun:\n{}",
                approval_command_display(repo, config)
            ));
        }
        self
    }
}

impl fmt::Display for WorktrunkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.command.is_empty() {
            formatter.write_str(&self.summary)?;
        } else {
            write!(formatter, "{}: {}", self.command, self.summary)?;
        }
        if let Some(hint) = &self.approval_hint {
            write!(formatter, "\n\n{hint}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SwitchRequest<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
    pub branch: &'a str,
    pub create: bool,
    pub base: Option<&'a str>,
}

#[derive(Deserialize)]
struct RawSwitchOutcome {
    action: String,
    branch: String,
    path: PathBuf,
    #[serde(default)]
    created_branch: bool,
}

pub(crate) fn switch_worktree(
    request: SwitchRequest<'_>,
) -> Result<SwitchOutcome, WorktrunkFailure> {
    let mut command = switch_command(request);
    let output = run_output_named(&mut command, ProcessPolicy::LocalMutation, SWITCH_PROCESS)
        .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if !output.status.success() {
        return Err(WorktrunkFailure::from_output(&command, &output)
            .with_approval_hint(request.repo, request.config));
    }
    parse_switch_output(&command, &output)
}

pub(crate) fn approval_status(
    repo: &Repository,
    config: &Config,
) -> Result<ApprovalStatus, WorktrunkFailure> {
    if config.worktree_command != "wt" {
        return Ok(ApprovalStatus::NotWorktrunk);
    }
    let mut command = approvals_command(repo, config);
    let output =
        run_output_allow_failure_named(&mut command, ProcessPolicy::Metadata, APPROVALS_PROCESS)
            .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if output.status.success() {
        return Ok(ApprovalStatus::Approved);
    }
    let failure = WorktrunkFailure::from_output(&command, &output);
    if failure.approval_required() {
        Ok(ApprovalStatus::Pending)
    } else {
        Err(failure)
    }
}

pub(crate) fn run_approval_prompt(repo: &Repository, config: &Config) -> Result<(), String> {
    run_status_inherited_named(&mut approvals_command(repo, config), APPROVALS_PROCESS)
}

pub(crate) fn approval_command_display(repo: &Repository, config: &Config) -> String {
    observability::command_display(&approvals_command(repo, config))
}

pub(crate) fn is_approval_failure(output: &str) -> bool {
    APPROVAL_FAILURE_RE.is_match(output)
}

pub(crate) fn list_columns(
    repo: &Repository,
    config: &Config,
) -> Result<BTreeMap<PathBuf, BTreeMap<String, String>>, String> {
    let raw = run_capture_named(
        &mut list_command(repo, config),
        ProcessPolicy::Metadata,
        LIST_PROCESS,
    )?;
    let mut by_path = BTreeMap::new();
    for object in crate::json::json_top_level_objects(&raw) {
        let Some(path) = crate::json::json_string_field(object, "path") else {
            continue;
        };
        let mut columns = discover_columns(object);
        for column in &config.worktree_columns {
            if let Some(value) = column_value(object, column) {
                columns.insert(column.clone(), value);
            }
        }
        by_path.insert(PathBuf::from(path), columns);
    }
    Ok(by_path)
}

pub(crate) fn discover_columns(object: &str) -> BTreeMap<String, String> {
    let Ok(value) = serde_json::from_str::<Value>(object) else {
        return BTreeMap::new();
    };
    let mut columns = BTreeMap::new();
    let Some(fields) = value.as_object() else {
        return columns;
    };
    for (key, value) in fields {
        if key != "path" {
            collect_column(&mut columns, key, value);
        }
    }
    columns
}

fn collect_column(columns: &mut BTreeMap<String, String>, key: &str, value: &Value) {
    match value {
        Value::String(value) if !value.is_empty() => {
            columns.insert(key.to_string(), value.clone());
        }
        Value::Bool(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Number(value) => {
            columns.insert(key.to_string(), value.to_string());
        }
        Value::Object(fields) => {
            for (field, value) in fields {
                collect_column(columns, &format!("{key}.{field}"), value);
            }
        }
        Value::String(_) | Value::Array(_) | Value::Null => {}
    }
}

fn column_value(object: &str, column: &str) -> Option<String> {
    if let Some(key) = column.strip_prefix("vars.") {
        return crate::json::json_object_field(object, "vars")
            .and_then(|vars| crate::json::json_string_field(vars, key));
    }
    if let Some((object_key, field_key)) = column.split_once('.') {
        return crate::json::json_object_field(object, object_key)
            .and_then(|inner| crate::json::json_string_field(inner, field_key));
    }
    crate::json::json_string_field(object, column)
        .or_else(|| crate::json::json_bool_field(object, column).map(|value| value.to_string()))
        .or_else(|| {
            (column == "ci")
                .then(|| crate::json::json_object_field(object, "ci"))
                .flatten()
                .map(|ci| {
                    let status = crate::json::json_string_field(ci, "status").unwrap_or_default();
                    let number = crate::json::json_u64_field(ci, "number")
                        .map(|number| format!("#{number}"))
                        .unwrap_or_else(|| "ci".to_string());
                    if status.is_empty() {
                        number
                    } else {
                        format!("{number}:{status}")
                    }
                })
        })
}

fn switch_command(request: SwitchRequest<'_>) -> Command {
    let mut command = Command::new(request.config.tool(&request.config.worktree_command));
    command.arg("-C").arg(&request.repo.root).arg("switch");
    if request.create {
        command.arg("--create");
    }
    command.args(["--no-cd", "--format=json"]);
    if let Some(base) = request.base.map(str::trim).filter(|base| !base.is_empty()) {
        command.arg("--base").arg(base);
    }
    command.arg(request.branch);
    command
}

fn list_command(repo: &Repository, config: &Config) -> Command {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command
        .arg("-C")
        .arg(&repo.root)
        .args(["list", "--format=json"]);
    command
}

fn approvals_command(repo: &Repository, config: &Config) -> Command {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command
        .arg("-C")
        .arg(&repo.root)
        .args(["config", "approvals", "add"]);
    command
}

fn parse_switch_output(
    command: &Command,
    output: &ProcessOutput,
) -> Result<SwitchOutcome, WorktrunkFailure> {
    let raw = serde_json::from_str::<RawSwitchOutcome>(&output.stdout)
        .map_err(|error| WorktrunkFailure::malformed(command, output, error))?;
    let action = match raw.action.as_str() {
        "created" => SwitchAction::Created,
        "existing" | "already_at" => SwitchAction::Existing,
        action => {
            return Err(WorktrunkFailure::malformed(
                command,
                output,
                format_args!("unsupported switch action {action:?}"),
            ));
        }
    };
    Ok(SwitchOutcome {
        action,
        path: raw.path,
        branch: raw.branch,
        created_branch: raw.created_branch,
    })
}

fn classify_failure(output: &str) -> FailureKind {
    if is_approval_failure(output) {
        FailureKind::ApprovalRequired
    } else if HOOK_FAILURE_RE.is_match(output) {
        FailureKind::Hook
    } else if GIT_FAILURE_RE.is_match(output) {
        FailureKind::Git
    } else {
        FailureKind::Process
    }
}

fn process_failure_message(output: &ProcessOutput) -> String {
    first_non_empty_line(&output.stderr)
        .or_else(|| first_non_empty_line(&output.stdout))
        .unwrap_or_else(|| format!("exited with {}", output.status))
}

fn first_non_empty_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn bounded_context(output: &str) -> Box<str> {
    crate::util::truncate(output, 2_000).into_boxed_str()
}

fn safe_summary(output: &str) -> String {
    observability::redact_freeform(output, 2_000)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn switch_arguments_preserve_special_values_without_shell_evaluation() {
        let repo = Repository::with_config_dir_for_test(
            PathBuf::from("/repo/space and ünicode"),
            PathBuf::from("/config"),
        );
        let config = crate::test_support::test_config();
        let command = switch_command(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: "feat/topic with space;λ",
            create: true,
            base: Some("--release/base"),
        });

        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "-C",
                "/repo/space and ünicode",
                "switch",
                "--create",
                "--no-cd",
                "--format=json",
                "--base",
                "--release/base",
                "feat/topic with space;λ",
            ]
        );
    }

    #[test]
    fn parses_created_and_existing_switch_fixtures() {
        let created = parse_fixture(
            r#"{"action":"created","branch":"feat/topic","path":"/repo.feat-topic","created_branch":true,"base_branch":"main"}"#,
        )
        .unwrap();
        let existing = parse_fixture(
            r#"{"action":"existing","branch":"feat/topic","path":"/repo.feat-topic"}"#,
        )
        .unwrap();
        let already_at = parse_fixture(
            r#"{"action":"already_at","branch":"feat/topic","path":"/repo.feat-topic"}"#,
        )
        .unwrap();

        assert_eq!(created.action, SwitchAction::Created);
        assert!(created.created_branch);
        assert_eq!(existing.action, SwitchAction::Existing);
        assert!(!existing.created_branch);
        assert_eq!(already_at.action, SwitchAction::Existing);
    }

    #[test]
    fn classifies_compatibility_failures_and_malformed_output() {
        assert_eq!(
            classify_failure(
                "repo needs approval to execute commands; cannot prompt in non-interactive mode"
            ),
            FailureKind::ApprovalRequired
        );
        assert_eq!(classify_failure("pre-start hook failed"), FailureKind::Hook);
        assert_eq!(
            classify_failure("fatal: invalid reference"),
            FailureKind::Git
        );
        assert_eq!(
            parse_fixture("not json").unwrap_err().kind,
            FailureKind::MalformedOutput
        );
        assert!(!is_approval_failure("All commands already approved"));
        assert!(!is_approval_failure(
            "cannot prompt in non-interactive mode before it needs approval"
        ));
    }

    #[test]
    fn switch_process_classifies_hook_and_git_failures() {
        assert_eq!(
            run_failure_fixture("printf '%s\\n' 'pre-start hook failed' >&2\nexit 1\n"),
            FailureKind::Hook
        );
        assert_eq!(
            run_failure_fixture("printf '%s\\n' 'fatal: invalid reference' >&2\nexit 1\n"),
            FailureKind::Git
        );
    }

    #[test]
    fn successful_switch_accepts_warnings_on_stderr() {
        let temp = unique_temp_dir("prism-wt-warning");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(
            &wt,
            "#!/bin/sh\nprintf '%s' '{\"action\":\"created\",\"branch\":\"feat/test\",\"path\":\"/repo.feat-test\",\"created_branch\":true}'\nprintf '%s\\n' 'warning from hook' >&2\n",
        );
        let mut config = crate::test_support::test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));

        let outcome = switch_worktree(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: "feat/test",
            create: true,
            base: None,
        })
        .unwrap();

        assert_eq!(outcome.action, SwitchAction::Created);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn switch_capture_is_bounded() {
        let temp = unique_temp_dir("prism-wt-bounded");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(
            &wt,
            "#!/bin/sh\ni=0\nwhile [ $i -lt 1200000 ]; do printf x; i=$((i + 1)); done\ni=0\nwhile [ $i -lt 5000 ]; do printf y >&2; i=$((i + 1)); done\nprintf '%s\\n' ' hook failed' >&2\nexit 1\n",
        );
        let mut config = crate::test_support::test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));

        let failure = switch_worktree(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: "feat/test",
            create: true,
            base: None,
        })
        .unwrap_err();

        assert!(failure.stdout.len() <= 2_000);
        assert!(failure.stderr.len() <= 2_000);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn switch_does_not_evaluate_shell_syntax_in_branch_argument() {
        let temp = unique_temp_dir("prism-wt-no-shell");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        let args = temp.join("args");
        let marker = temp.join("evaluated");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{{\"action\":\"created\",\"branch\":\"feat/test\",\"path\":\"/repo.feat-test\",\"created_branch\":true}}'\n",
                args.display()
            ),
        );
        let mut config = crate::test_support::test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let branch = format!("feat/$(touch {})", marker.display());

        switch_worktree(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: &branch,
            create: true,
            base: None,
        })
        .unwrap();

        assert!(!marker.exists());
        assert_eq!(
            fs::read_to_string(args).unwrap().lines().last(),
            Some(branch.as_str())
        );
        let _ = fs::remove_dir_all(temp);
    }

    fn parse_fixture(stdout: &str) -> Result<SwitchOutcome, WorktrunkFailure> {
        let mut command = Command::new("wt");
        command.arg("switch");
        let output = ProcessOutput {
            status: success_status(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            stdout_total_bytes: stdout.len() as u64,
            stdout_truncated: false,
            stderr_total_bytes: 0,
            stderr_truncated: false,
        };
        parse_switch_output(&command, &output)
    }

    fn run_failure_fixture(body: &str) -> FailureKind {
        let temp = unique_temp_dir("prism-wt-failure");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        write_executable(&wt, &format!("#!/bin/sh\n{body}"));
        let mut config = crate::test_support::test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let failure = switch_worktree(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: "feat/test",
            create: true,
            base: None,
        })
        .unwrap_err();
        let _ = fs::remove_dir_all(temp);
        failure.kind
    }

    #[cfg(unix)]
    fn success_status() -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(0)
    }

    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{id}"))
    }
}
