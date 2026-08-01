use std::collections::BTreeMap;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::Instant;

use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::observability;
use crate::process::{
    ProcessDescriptor, ProcessOutput, ProcessPolicy, run_output_allow_failure_named,
    run_output_named, run_status_inherited_named,
};
use crate::repo::Repository;

pub(crate) const LIST_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.list");
pub(crate) const VERSION_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.version");
pub(crate) const SWITCH_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.switch");
#[allow(dead_code)]
pub(crate) const REMOVE_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.remove");
pub(crate) const APPROVALS_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.approvals");
#[allow(dead_code)]
pub(crate) const LOGS_PROCESS: ProcessDescriptor = ProcessDescriptor::new("wt.logs");

pub(crate) const HOOK_LOG_TAIL_BYTES: u64 = 64 * 1024;
pub(crate) const HOOK_LOG_TAIL_LINES: usize = 200;
pub(crate) const MINIMUM_VERSION: &str = "0.58.0";
pub(crate) const TESTED_CURRENT_VERSION: &str = "0.71.0";

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemoveOutcome {
    pub path: PathBuf,
    pub branch: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailureKind {
    ApprovalRequired,
    Hook,
    Git,
    MalformedOutput,
    UnsupportedSchema,
    Process,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorktrunkSchema {
    V1,
    V2,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorktrunkSnapshot {
    pub schema: WorktrunkSchema,
    pub by_path: BTreeMap<PathBuf, WorktrunkWorktreeFacts>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WorktrunkWorktreeFacts {
    pub dev_server: Option<DevServerObservation>,
    pub vars: BTreeMap<String, Value>,
    pub extra_columns: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DevServerObservation {
    pub url: String,
    pub listening: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HookLogEntry {
    pub path: PathBuf,
    pub branch: String,
    pub source: String,
    pub hook_type: Option<String>,
    pub name: String,
    pub modified_at: String,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObservationQuality {
    NeverLoaded,
    Refreshing,
    Fresh,
    Stale {
        last_success: Instant,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorktrunkVersion {
    pub raw: String,
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre_release: bool,
}

impl WorktrunkVersion {
    pub(crate) fn supported(&self) -> bool {
        (self.major, self.minor, self.patch) > (0, 58, 0)
            || (self.major, self.minor, self.patch) == (0, 58, 0) && !self.pre_release
    }
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

    fn unsupported_schema(command: &Command, output: &ProcessOutput, schema: &Value) -> Self {
        Self {
            kind: FailureKind::UnsupportedSchema,
            command: observability::command_display(command),
            summary: format!("unsupported Worktrunk JSON schema {schema}"),
            stdout: bounded_context(&output.stdout),
            stderr: bounded_context(&output.stderr),
            approval_hint: None,
        }
    }

    pub(crate) fn safe_summary(&self) -> String {
        self.summary.clone()
    }

    fn with_approval_hint(mut self, repo: &Repository, config: &Config) -> Self {
        if self.approval_required() {
            self.approval_hint = Some(format!(
                "This repo has Worktrunk project commands that must be approved before Prism can manage worktrees.\n\nRun:\n{}",
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct RemoveRequest<'a> {
    pub repo: &'a Repository,
    pub config: &'a Config,
    pub path: &'a Path,
}

#[derive(Deserialize)]
struct RawSwitchOutcome {
    action: String,
    branch: String,
    path: PathBuf,
    #[serde(default)]
    created_branch: bool,
}

#[derive(Deserialize)]
struct RawRemoveOutcome {
    kind: String,
    path: PathBuf,
    #[serde(default)]
    branch: Option<String>,
    branch_deleted: bool,
}

#[derive(Deserialize)]
struct Schema2Envelope {
    schema: Value,
    items: Vec<Value>,
}

#[derive(Deserialize)]
struct Schema1Item {
    path: PathBuf,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    url_active: Option<bool>,
    #[serde(default)]
    vars: BTreeMap<String, Value>,
    #[serde(default)]
    columns: BTreeMap<String, Value>,
    #[serde(flatten)]
    fields: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct Schema2Item {
    worktree: Schema2Worktree,
    #[serde(default)]
    dev_server: Option<Schema2DevServer>,
    #[serde(default)]
    vars: BTreeMap<String, Value>,
    #[serde(default)]
    display: Option<Schema2Display>,
}

#[derive(Deserialize)]
struct Schema2Worktree {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Schema2DevServer {
    url: String,
    #[serde(default)]
    listening: Option<bool>,
}

#[derive(Default, Deserialize)]
struct Schema2Display {
    #[serde(default)]
    columns: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct HookLogEnvelope {
    #[serde(default)]
    hook_output: Vec<HookLogJson>,
}

#[derive(Deserialize)]
struct HookLogJson {
    path: PathBuf,
    #[serde(default)]
    branch: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    hook_type: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    modified_at: Value,
    #[serde(default)]
    size: u64,
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

pub(crate) fn remove_worktree(
    request: RemoveRequest<'_>,
) -> Result<RemoveOutcome, WorktrunkFailure> {
    let expected_path = normalize_path(request.path);
    let mut command = remove_command(request);
    let output = run_output_named(&mut command, ProcessPolicy::LocalMutation, REMOVE_PROCESS)
        .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if !output.status.success() {
        return Err(WorktrunkFailure::from_output(&command, &output)
            .with_approval_hint(request.repo, request.config));
    }
    parse_remove_output(&command, &output, &expected_path)
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

pub(crate) fn observe_repository(
    repo: &Repository,
    config: &Config,
) -> Result<WorktrunkSnapshot, WorktrunkFailure> {
    let mut command = list_command(repo, config);
    let output = run_output_named(&mut command, ProcessPolicy::Metadata, LIST_PROCESS)
        .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if !output.status.success() {
        return Err(WorktrunkFailure::from_output(&command, &output));
    }
    parse_list_output(&command, &output, &config.worktree_columns)
}

pub(crate) fn observe_hook_logs(
    repo: &Repository,
    config: &Config,
) -> Result<Vec<HookLogEntry>, WorktrunkFailure> {
    let mut command = logs_command(repo, config);
    let output = run_output_named(&mut command, ProcessPolicy::Metadata, LOGS_PROCESS)
        .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if !output.status.success() {
        return Err(WorktrunkFailure::from_output(&command, &output));
    }
    let envelope = serde_json::from_str::<HookLogEnvelope>(&output.stdout)
        .map_err(|error| WorktrunkFailure::malformed(&command, &output, error))?;
    Ok(envelope
        .hook_output
        .into_iter()
        .map(|entry| HookLogEntry {
            path: entry.path,
            branch: entry.branch,
            source: entry.source,
            hook_type: entry.hook_type,
            name: entry.name,
            modified_at: match entry.modified_at {
                Value::String(value) => value,
                Value::Number(value) => value.to_string(),
                Value::Null => String::new(),
                value => value.to_string(),
            },
            size: entry.size,
        })
        .collect())
}

pub(crate) fn read_hook_log_tail(repo: &Repository, path: &Path) -> Result<Vec<String>, String> {
    let root = repo.root.join(".git/wt/logs");
    let root = root
        .canonicalize()
        .map_err(|error| format!("Worktrunk log root is unavailable: {error}"))?;
    if path
        .symlink_metadata()
        .map_err(|error| format!("Worktrunk log is unavailable: {error}"))?
        .file_type()
        .is_symlink()
    {
        return Err("Worktrunk log path is a symlink".to_string());
    }
    let path = path
        .canonicalize()
        .map_err(|error| format!("Worktrunk log is unavailable: {error}"))?;
    if !path.starts_with(&root) {
        return Err("Worktrunk log path is outside the repository log root".to_string());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("open Worktrunk log: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect Worktrunk log: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("Worktrunk log is not a regular file".to_string());
    }
    let start = metadata.len().saturating_sub(HOOK_LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek Worktrunk log: {error}"))?;
    let mut bytes = Vec::new();
    file.take(HOOK_LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read Worktrunk log: {error}"))?;
    if start > 0
        && let Some(first_newline) = bytes.iter().position(|byte| *byte == b'\n')
    {
        bytes.drain(..=first_newline);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines().map(sanitize_log_line).collect::<Vec<_>>();
    if lines.len() > HOOK_LOG_TAIL_LINES {
        lines.drain(..lines.len() - HOOK_LOG_TAIL_LINES);
    }
    Ok(lines)
}

fn sanitize_log_line(line: &str) -> String {
    let mut clean = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut escaped = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || escaped && next == '\\' {
                            break;
                        }
                        escaped = next == '\u{1b}';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if ch == '\t' {
            clean.push_str("    ");
        } else if !ch.is_control() {
            clean.push(ch);
        }
    }
    clean
}

#[cfg(test)]
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

pub(crate) fn associate_snapshot<T>(
    snapshot: &WorktrunkSnapshot,
    sessions: impl IntoIterator<Item = (PathBuf, T)>,
) -> BTreeMap<T, WorktrunkWorktreeFacts>
where
    T: Ord,
{
    let mut normalized = BTreeMap::<PathBuf, Option<&WorktrunkWorktreeFacts>>::new();
    for (path, facts) in &snapshot.by_path {
        let path = normalize_path(path);
        normalized
            .entry(path)
            .and_modify(|entry| *entry = None)
            .or_insert(Some(facts));
    }
    sessions
        .into_iter()
        .filter_map(|(path, key)| {
            normalized
                .get(&normalize_path(&path))
                .and_then(|facts| *facts)
                .cloned()
                .map(|facts| (key, facts))
        })
        .collect()
}

pub(crate) fn paths_equivalent(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

pub(crate) fn projected_columns(facts: &WorktrunkWorktreeFacts) -> BTreeMap<String, String> {
    facts.extra_columns.clone()
}

pub(crate) fn detect_version(config: &Config) -> Result<WorktrunkVersion, WorktrunkFailure> {
    let mut command = version_command(config);
    let output = run_output_named(&mut command, ProcessPolicy::Metadata, VERSION_PROCESS)
        .map_err(|error| WorktrunkFailure::process(&command, error))?;
    if !output.status.success() {
        return Err(WorktrunkFailure::from_output(&command, &output));
    }
    parse_version(&output.stdout).ok_or_else(|| WorktrunkFailure {
        kind: FailureKind::MalformedOutput,
        command: observability::command_display(&command),
        summary: "could not parse Worktrunk version".to_string(),
        stdout: bounded_context(&output.stdout),
        stderr: bounded_context(&output.stderr),
        approval_hint: None,
    })
}

pub(crate) fn ensure_supported_version(config: &Config) -> Result<WorktrunkVersion, String> {
    let version = detect_version(config).map_err(|error| error.to_string())?;
    if version.supported() {
        Ok(version)
    } else {
        Err(format!(
            "Worktrunk {} is unsupported; Prism requires Worktrunk 0.58.0 or newer. Upgrade Worktrunk and retry",
            version.raw
        ))
    }
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

fn remove_command(request: RemoveRequest<'_>) -> Command {
    let mut command = Command::new(request.config.tool(&request.config.worktree_command));
    command
        .arg("-C")
        .arg(&request.repo.root)
        .arg("remove")
        .args([
            "--foreground",
            "--force",
            "--no-delete-branch",
            "--format=json",
            "--",
        ])
        .arg(request.path);
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

fn logs_command(repo: &Repository, config: &Config) -> Command {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command
        .arg("-C")
        .arg(&repo.root)
        .args(["config", "state", "logs", "--format=json"]);
    command
}

fn version_command(config: &Config) -> Command {
    let mut command = Command::new(config.tool(&config.worktree_command));
    command.arg("--version");
    command
}

fn parse_list_output(
    command: &Command,
    output: &ProcessOutput,
    configured_columns: &[String],
) -> Result<WorktrunkSnapshot, WorktrunkFailure> {
    let value = serde_json::from_str::<Value>(&output.stdout)
        .map_err(|error| WorktrunkFailure::malformed(command, output, error))?;
    match value {
        Value::Array(items) => parse_schema1_items(items, configured_columns)
            .map(|by_path| WorktrunkSnapshot {
                schema: WorktrunkSchema::V1,
                by_path,
            })
            .map_err(|error| WorktrunkFailure::malformed(command, output, error)),
        Value::Object(_) => {
            let envelope = serde_json::from_value::<Schema2Envelope>(value)
                .map_err(|error| WorktrunkFailure::malformed(command, output, error))?;
            if envelope.schema != 2 {
                return Err(WorktrunkFailure::unsupported_schema(
                    command,
                    output,
                    &envelope.schema,
                ));
            }
            parse_schema2_items(envelope.items, configured_columns)
                .map(|by_path| WorktrunkSnapshot {
                    schema: WorktrunkSchema::V2,
                    by_path,
                })
                .map_err(|error| WorktrunkFailure::malformed(command, output, error))
        }
        _ => Err(WorktrunkFailure::malformed(
            command,
            output,
            "expected a schema-1 array or schema-2 envelope",
        )),
    }
}

fn parse_schema1_items(
    items: Vec<Value>,
    configured_columns: &[String],
) -> Result<BTreeMap<PathBuf, WorktrunkWorktreeFacts>, serde_json::Error> {
    let mut by_path = BTreeMap::new();
    let mut ambiguous = std::collections::BTreeSet::new();
    for value in items {
        if value.get("path").is_none() {
            continue;
        }
        let raw = serde_json::from_value::<Schema1Item>(value)?;
        let mut columns = BTreeMap::new();
        for (key, value) in &raw.fields {
            collect_column(&mut columns, key, value);
        }
        if configured_columns.iter().any(|column| column == "ci")
            && let Some(ci) = raw.fields.get("ci").and_then(Value::as_object)
        {
            let status = ci.get("status").and_then(Value::as_str).unwrap_or_default();
            let number = ci
                .get("number")
                .and_then(Value::as_u64)
                .map(|number| format!("#{number}"))
                .unwrap_or_else(|| "ci".to_string());
            columns.insert(
                "ci".to_string(),
                if status.is_empty() {
                    number
                } else {
                    format!("{number}:{status}")
                },
            );
        }
        for (key, value) in &raw.columns {
            collect_column(&mut columns, key, value);
        }
        let dev_server = raw
            .url
            .filter(|url| !url.is_empty())
            .map(|url| DevServerObservation {
                url,
                listening: raw.url_active,
            });
        project_canonical_columns(&mut columns, dev_server.as_ref(), &raw.vars);
        insert_unique_path(
            &mut by_path,
            &mut ambiguous,
            raw.path,
            WorktrunkWorktreeFacts {
                dev_server,
                vars: raw.vars,
                extra_columns: columns,
            },
        );
    }
    Ok(by_path)
}

fn parse_schema2_items(
    items: Vec<Value>,
    _configured_columns: &[String],
) -> Result<BTreeMap<PathBuf, WorktrunkWorktreeFacts>, serde_json::Error> {
    let mut by_path = BTreeMap::new();
    let mut ambiguous = std::collections::BTreeSet::new();
    for value in items {
        if value.get("worktree").is_none() {
            continue;
        }
        let raw = serde_json::from_value::<Schema2Item>(value)?;
        let dev_server = raw.dev_server.map(|server| DevServerObservation {
            url: server.url,
            listening: server.listening,
        });
        let mut columns = BTreeMap::new();
        for (key, value) in raw.display.unwrap_or_default().columns {
            collect_column(&mut columns, &key, &value);
        }
        project_canonical_columns(&mut columns, dev_server.as_ref(), &raw.vars);
        insert_unique_path(
            &mut by_path,
            &mut ambiguous,
            raw.worktree.path,
            WorktrunkWorktreeFacts {
                dev_server,
                vars: raw.vars,
                extra_columns: columns,
            },
        );
    }
    Ok(by_path)
}

fn insert_unique_path(
    by_path: &mut BTreeMap<PathBuf, WorktrunkWorktreeFacts>,
    ambiguous: &mut std::collections::BTreeSet<PathBuf>,
    path: PathBuf,
    facts: WorktrunkWorktreeFacts,
) {
    if ambiguous.contains(&path) || by_path.remove(&path).is_some() {
        ambiguous.insert(path);
    } else {
        by_path.insert(path, facts);
    }
}

fn project_canonical_columns(
    columns: &mut BTreeMap<String, String>,
    dev_server: Option<&DevServerObservation>,
    vars: &BTreeMap<String, Value>,
) {
    if let Some(server) = dev_server {
        columns.insert("url".to_string(), server.url.clone());
        columns.insert("dev_server.url".to_string(), server.url.clone());
        if let Some(listening) = server.listening {
            let value = listening.to_string();
            columns.insert("url_active".to_string(), value.clone());
            columns.insert("dev_server.listening".to_string(), value);
        }
    }
    for (key, value) in vars {
        collect_column(columns, &format!("vars.{key}"), value);
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn parse_version(output: &str) -> Option<WorktrunkVersion> {
    let raw = output.lines().find(|line| !line.trim().is_empty())?.trim();
    let token = raw.split_whitespace().find_map(|token| {
        let token = token.trim_start_matches('v');
        token
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
            .then_some(token)
    })?;
    let mut parts = token.split(|character: char| !character.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some(WorktrunkVersion {
        raw: raw.to_string(),
        major,
        minor,
        patch,
        pre_release: token.contains('-'),
    })
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

fn parse_remove_output(
    command: &Command,
    output: &ProcessOutput,
    expected_path: &Path,
) -> Result<RemoveOutcome, WorktrunkFailure> {
    let mut raw = serde_json::from_str::<Vec<RawRemoveOutcome>>(&output.stdout)
        .map_err(|error| WorktrunkFailure::malformed(command, output, error))?;
    if raw.len() != 1 {
        return Err(WorktrunkFailure::malformed(
            command,
            output,
            format_args!("expected one removal result, received {}", raw.len()),
        ));
    }
    let raw = raw.pop().expect("length checked");
    if raw.kind != "worktree" {
        return Err(WorktrunkFailure::malformed(
            command,
            output,
            format_args!("unsupported removal kind {:?}", raw.kind),
        ));
    }
    if raw.path != expected_path {
        return Err(WorktrunkFailure::malformed(
            command,
            output,
            format_args!(
                "removed path {} did not match requested path {}",
                raw.path.display(),
                expected_path.display()
            ),
        ));
    }
    if raw.branch_deleted {
        return Err(WorktrunkFailure::malformed(
            command,
            output,
            "Worktrunk reported deleting the branch despite --no-delete-branch",
        ));
    }
    Ok(RemoveOutcome {
        path: raw.path,
        branch: raw.branch,
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
    fn hook_log_tail_is_bounded_sanitized_and_confined() {
        let temp = unique_temp_dir("prism-worktrunk-log-tail");
        let root = temp.join("repo");
        let logs = root.join(".git/wt/logs");
        fs::create_dir_all(&logs).unwrap();
        let log = logs.join("hook.log");
        let mut body = (0..250)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        body.push_str("\n\u{1b}[31mred\u{1b}[0m\tend\u{7}\n");
        fs::write(&log, body).unwrap();
        let repo = Repository::with_config_dir_for_test(root, temp.join("config"));

        let tail = read_hook_log_tail(&repo, &log).unwrap();

        assert_eq!(tail.len(), HOOK_LOG_TAIL_LINES);
        assert_eq!(tail.last().unwrap(), "red    end");
        assert!(!tail.join("\n").contains('\u{1b}'));
        let outside = temp.join("outside.log");
        fs::write(&outside, "secret").unwrap();
        assert!(
            read_hook_log_tail(&repo, &outside)
                .unwrap_err()
                .contains("outside")
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, logs.join("escape.log")).unwrap();
            assert!(
                read_hook_log_tail(&repo, &logs.join("escape.log"))
                    .unwrap_err()
                    .contains("symlink")
            );
        }
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn hook_log_inventory_accepts_floor_numeric_timestamps() {
        let envelope = serde_json::from_str::<HookLogEnvelope>(
            r#"{"hook_output":[{"path":"/repo/.git/wt/logs/hook.log","modified_at":1720000000}]}"#,
        )
        .unwrap();
        assert_eq!(
            envelope.hook_output[0].modified_at,
            serde_json::json!(1720000000)
        );
    }

    #[test]
    #[ignore = "requires PRISM_TEST_WORKTRUNK pointing to a real Worktrunk binary"]
    fn real_worktrunk_create_observe_remove_smoke() {
        let wt = std::env::var("PRISM_TEST_WORKTRUNK")
            .expect("PRISM_TEST_WORKTRUNK must point to Worktrunk");
        let temp = unique_temp_dir("prism real worktrunk smoke");
        let root = temp.join("repo with spaces");
        fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.name", "Prism Test"],
            vec!["config", "user.email", "prism@example.invalid"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let repo = Repository::with_config_dir_for_test(root, temp.join("prism-config"));
        let mut config = crate::test_support::test_config();
        config.tools.insert("wt".to_string(), wt);
        let created = switch_worktree(SwitchRequest {
            repo: &repo,
            config: &config,
            branch: "ci/real-smoke",
            create: true,
            base: Some("main"),
        })
        .unwrap();
        let snapshot = observe_repository(&repo, &config).unwrap();
        assert!(snapshot.by_path.contains_key(&created.path));
        let removed = remove_worktree(RemoveRequest {
            repo: &repo,
            config: &config,
            path: &created.path,
        })
        .unwrap();
        assert_eq!(removed.path, created.path);
        assert!(!created.path.exists());
        let _ = fs::remove_dir_all(temp);
    }

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
    fn remove_arguments_use_foreground_force_without_branch_deletion_and_exact_path() {
        let repo = Repository::with_config_dir_for_test(
            PathBuf::from("/repo/space and ünicode"),
            PathBuf::from("/config"),
        );
        let config = crate::test_support::test_config();
        let path = PathBuf::from("/repo/worktrees/--feature λ");
        let command = remove_command(RemoveRequest {
            repo: &repo,
            config: &config,
            path: &path,
        });

        assert_eq!(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            vec![
                "-C",
                "/repo/space and ünicode",
                "remove",
                "--foreground",
                "--force",
                "--no-delete-branch",
                "--format=json",
                "--",
                "/repo/worktrees/--feature λ",
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
    fn parses_exact_remove_result_and_rejects_unsafe_results() {
        let path = Path::new("/repo/worktree");
        let removed = parse_remove_fixture(
            r#"[{"branch":"feat/topic","branch_deleted":false,"kind":"worktree","path":"/repo/worktree"}]"#,
            path,
        )
        .unwrap();
        assert_eq!(removed.path, path);
        assert_eq!(removed.branch.as_deref(), Some("feat/topic"));

        let wrong_path = parse_remove_fixture(
            r#"[{"branch":"feat/topic","branch_deleted":false,"kind":"worktree","path":"/repo/other"}]"#,
            path,
        )
        .unwrap_err();
        assert_eq!(wrong_path.kind, FailureKind::MalformedOutput);

        let deleted_branch = parse_remove_fixture(
            r#"[{"branch":"feat/topic","branch_deleted":true,"kind":"worktree","path":"/repo/worktree"}]"#,
            path,
        )
        .unwrap_err();
        assert_eq!(deleted_branch.kind, FailureKind::MalformedOutput);
    }

    #[test]
    fn remove_failure_classifies_approval_and_preserves_exact_path_argument() {
        let temp = unique_temp_dir("prism-wt-remove-approval");
        fs::create_dir_all(&temp).unwrap();
        let wt = temp.join("wt");
        let args = temp.join("args");
        write_executable(
            &wt,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' 'repo needs approval to execute commands; cannot prompt in non-interactive mode' >&2\nexit 1\n",
                args.display()
            ),
        );
        let mut config = crate::test_support::test_config();
        config
            .tools
            .insert("wt".to_string(), wt.display().to_string());
        let repo = Repository::with_config_dir_for_test(temp.join("repo"), temp.join("config"));
        let path = temp.join("--worktree with space");

        let failure = remove_worktree(RemoveRequest {
            repo: &repo,
            config: &config,
            path: &path,
        })
        .unwrap_err();

        assert!(failure.approval_required());
        assert!(failure.to_string().contains("config approvals add"));
        assert_eq!(
            fs::read_to_string(args).unwrap().lines().last(),
            Some(path.to_string_lossy().as_ref())
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_and_normalizes_schema_1_fixtures() {
        let full = parse_list_fixture(include_str!(
            "../tests/fixtures/worktrunk/schema1-full.json"
        ))
        .unwrap();
        let minimal = parse_list_fixture(include_str!(
            "../tests/fixtures/worktrunk/schema1-minimal.json"
        ))
        .unwrap();

        assert_eq!(full.schema, WorktrunkSchema::V1);
        let facts = &full.by_path[Path::new("/redacted/repo.feat-typed-observation")];
        assert_eq!(
            facts.dev_server,
            Some(DevServerObservation {
                url: "http://localhost:43117".to_string(),
                listening: Some(true),
            })
        );
        assert_eq!(facts.vars["attempt"], Value::from(3));
        assert_eq!(facts.extra_columns["url_active"], "true");
        assert_eq!(facts.extra_columns["ci.status"], "passed");
        assert_eq!(facts.extra_columns["Ticket"], "PRISM-42");
        assert_eq!(minimal.by_path.len(), 1);
    }

    #[test]
    fn parses_and_normalizes_schema_2_fixtures() {
        let full = parse_list_fixture(include_str!(
            "../tests/fixtures/worktrunk/schema2-full.json"
        ))
        .unwrap();
        let minimal = parse_list_fixture(include_str!(
            "../tests/fixtures/worktrunk/schema2-minimal.json"
        ))
        .unwrap();

        assert_eq!(full.schema, WorktrunkSchema::V2);
        let facts = &full.by_path[Path::new("/redacted/repo.feat-typed-observation")];
        assert_eq!(facts.extra_columns["url"], "http://localhost:43117");
        assert_eq!(facts.extra_columns["dev_server.listening"], "true");
        assert_eq!(facts.extra_columns["vars.enabled"], "true");
        assert_eq!(facts.extra_columns["Ticket"], "PRISM-42");
        assert_eq!(minimal.by_path.len(), 3);
        assert!(
            minimal
                .by_path
                .values()
                .all(|facts| facts.dev_server.is_none())
        );
    }

    #[test]
    fn unknown_schema_and_malformed_items_fail_closed() {
        let unsupported = parse_list_fixture(r#"{"schema":3,"items":[]}"#).unwrap_err();
        assert_eq!(unsupported.kind, FailureKind::UnsupportedSchema);

        let malformed = parse_list_fixture(
            r#"[{"path":"/valid","url":"http://localhost:3000"},{"path":42},{"kind":"branch"}]"#,
        )
        .unwrap_err();
        assert_eq!(malformed.kind, FailureKind::MalformedOutput);
    }

    #[test]
    fn duplicate_exact_paths_are_omitted_as_ambiguous() {
        let snapshot = parse_list_fixture(
            r#"[{"path":"/same","url":"http://localhost:3000"},{"path":"/same","url":"http://localhost:4000"}]"#,
        )
        .unwrap();
        assert!(snapshot.by_path.is_empty());
    }

    #[test]
    fn path_association_uses_canonical_paths_and_rejects_ambiguity() {
        let temp = unique_temp_dir("prism-wt-path-association");
        let real = temp.join("real");
        let alias = temp.join("alias");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let facts = WorktrunkWorktreeFacts {
            extra_columns: BTreeMap::from([("url".to_string(), "one".to_string())]),
            ..WorktrunkWorktreeFacts::default()
        };
        let snapshot = WorktrunkSnapshot {
            schema: WorktrunkSchema::V1,
            by_path: BTreeMap::from([(real.clone(), facts.clone())]),
        };
        assert_eq!(
            associate_snapshot(&snapshot, [(alias.clone(), "session")])["session"],
            facts
        );

        let ambiguous = WorktrunkSnapshot {
            schema: WorktrunkSchema::V1,
            by_path: BTreeMap::from([(real, facts.clone()), (alias.clone(), facts)]),
        };
        assert!(associate_snapshot(&ambiguous, [(alias, "session")]).is_empty());
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn parses_supported_worktrunk_versions() {
        let floor = parse_version("worktrunk 0.58.0\n").unwrap();
        let current = parse_version("wt 1.2.3-beta.1\n").unwrap();
        assert!(floor.supported());
        assert!(current.supported());
        assert!(!parse_version("worktrunk 0.57.9").unwrap().supported());
        assert!(
            !parse_version("worktrunk 0.58.0-alpha.1")
                .unwrap()
                .supported()
        );
        assert!(parse_version("worktrunk development").is_none());
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

    fn parse_list_fixture(stdout: &str) -> Result<WorktrunkSnapshot, WorktrunkFailure> {
        let mut command = Command::new("wt");
        command.arg("list");
        let output = ProcessOutput {
            status: success_status(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            stdout_total_bytes: stdout.len() as u64,
            stdout_truncated: false,
            stderr_total_bytes: 0,
            stderr_truncated: false,
        };
        parse_list_output(&command, &output, &[])
    }

    fn parse_remove_fixture(
        stdout: &str,
        expected_path: &Path,
    ) -> Result<RemoveOutcome, WorktrunkFailure> {
        let mut command = Command::new("wt");
        command.arg("remove");
        let output = ProcessOutput {
            status: success_status(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            stdout_total_bytes: stdout.len() as u64,
            stdout_truncated: false,
            stderr_total_bytes: 0,
            stderr_truncated: false,
        };
        parse_remove_output(&command, &output, expected_path)
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
