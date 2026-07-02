use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "prism-cli-test-{name}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn prism() -> Command {
    Command::new(env!("CARGO_BIN_EXE_prism"))
}

fn run<I, S>(args: I, cwd: &Path, config_home: &Path) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = prism();
    command
        .args(args)
        .current_dir(cwd)
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", config_home);
    command.output().expect("run prism")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn canonical_display(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn init_repo(path: &Path) {
    fs::create_dir_all(path).expect("create repo dir");
    let output = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(path)
        .output()
        .expect("git init");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn help_prints_usage_without_repo() {
    let temp = TempDir::new("help");
    let output = run(["--help"], temp.path(), temp.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("Usage:\n  prism"));
    assert!(stdout(&output).contains("auto run-plan <plan.md>"));
    assert!(stderr(&output).is_empty());
}

#[test]
fn version_prints_package_version_without_repo() {
    let temp = TempDir::new("version");
    let output = run(["--version"], temp.path(), temp.path());

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output).trim(),
        format!("prism {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(stderr(&output).is_empty());
}

#[test]
fn config_prints_effective_repo_config() {
    let temp = TempDir::new("config");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["config"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains(&format!("repo_root = {}", canonical_display(&repo))));
    assert!(stdout.contains("default_agent = opencode"));
    assert!(stdout.contains("default_base = main"));
}

#[test]
fn config_discovery_commands_print_templates_schema_and_paths() {
    let temp = TempDir::new("config-discovery");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let example = run(["config", "example"], &repo, &config_home);
    assert!(example.status.success(), "{}", stderr(&example));
    let example_stdout = stdout(&example);
    assert!(example_stdout.contains("#:schema https://raw.githubusercontent.com/"));
    assert!(example_stdout.contains("[ui]"));
    assert!(example_stdout.contains("# [worktrees]"));

    let schema = run(["config", "schema"], &repo, &config_home);
    assert!(schema.status.success(), "{}", stderr(&schema));
    let schema_stdout = stdout(&schema);
    assert!(schema_stdout.contains(r#""title": "Prism Config""#));
    assert!(schema_stdout.contains(r#""merge_method""#));

    let paths = run(["config", "paths"], &repo, &config_home);
    assert!(paths.status.success(), "{}", stderr(&paths));
    let paths_stdout = stdout(&paths);
    assert!(paths_stdout.contains(&format!(
        "user_config = {}",
        config_home.join("prism/config.toml").display()
    )));
    assert!(paths_stdout.contains("repo_config = "));
    assert!(paths_stdout.contains("schema_url = https://raw.githubusercontent.com/"));
}

#[test]
#[cfg(unix)]
fn doctor_reports_repository_and_tool_status() {
    let temp = TempDir::new("doctor");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    init_repo(&repo);
    install_shim(&bin, "gh");
    install_shim(&bin, "tmux");
    install_shim(&bin, "wt");
    install_shim(&bin, "opencode");

    let mut command = prism();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = command
        .arg("doctor")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &config_home)
        .env("PATH", path)
        .output()
        .expect("run prism doctor");

    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("Prism doctor"));
    assert!(stdout.contains(&format!("repo: {}", canonical_display(&repo))));
    assert!(stdout.contains("default agent: opencode"));
    assert!(stdout.contains("checks: pre_pr=0 pre_push=0 review_fix=0"));
}

#[test]
fn db_path_prints_repo_database_path() {
    let temp = TempDir::new("db-path");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["db", "path"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let path = stdout(&output);
    assert!(
        path.trim()
            .starts_with(&config_home.join("prism/repos").display().to_string())
    );
    assert!(path.trim().ends_with("/prism.db"));
}

#[test]
#[cfg(unix)]
fn db_without_arguments_launches_sqlite3_with_initialized_database() {
    let temp = TempDir::new("db-shell");
    let repo = temp.path().join("repo with spaces");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("sqlite3-args");
    init_repo(&repo);
    install_sqlite3_db_asserting_shim(&bin, &marker);

    let mut command = prism();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = command
        .arg("db")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &config_home)
        .env("PATH", path)
        .output()
        .expect("run prism db");

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stderr(&output).is_empty());
    let db_path = fs::read_to_string(marker).expect("read sqlite3 marker");
    assert!(db_path.trim().ends_with("/prism.db"));
    assert!(Path::new(db_path.trim()).exists());
    assert_db_has_tables(
        db_path.trim(),
        [
            "agent_state",
            "auto_event",
            "auto_output_line",
            "auto_run",
            "auto_step_run",
            "event",
            "hidden_session",
            "metadata",
            "opencode_runtime",
            "plan_output_line",
            "plan_run",
            "plan_step_run",
            "pr_cache",
            "pr_details_cache",
            "startup_phase",
            "startup_run",
            "task_metadata",
        ],
    );
}

#[test]
#[cfg(unix)]
fn db_without_arguments_reports_missing_sqlite3() {
    let temp = TempDir::new("db-shell-missing-sqlite3");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    init_repo(&repo);
    install_git_proxy_shim(&bin);

    let output = prism()
        .arg("db")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &config_home)
        .env("PATH", &bin)
        .output()
        .expect("run prism db");

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("sqlite3 not found; install sqlite3"));
}

#[test]
#[cfg(unix)]
fn db_query_rejects_writes_after_shell_initializes_database() {
    let temp = TempDir::new("db-query-readonly");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("sqlite3-args");
    init_repo(&repo);
    install_sqlite3_db_asserting_shim(&bin, &marker);

    let mut init_command = prism();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let init_output = init_command
        .arg("db")
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &config_home)
        .env("PATH", path)
        .output()
        .expect("initialize prism db");
    assert!(init_output.status.success(), "{}", stderr(&init_output));

    let output = run(
        ["db", "insert into plan_run(id) values ('not-allowed')"],
        &repo,
        &config_home,
    );

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("readonly database"));
}

#[test]
#[cfg(unix)]
fn db_whitespace_query_stays_non_interactive() {
    let temp = TempDir::new("db-query-whitespace");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    let marker = temp.path().join("sqlite3-args");
    init_repo(&repo);
    install_sqlite3_db_asserting_shim(&bin, &marker);

    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = prism()
        .args(["db", "   "])
        .current_dir(&repo)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &config_home)
        .env("PATH", path)
        .output()
        .expect("run whitespace db query");

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("prism:"));
    assert!(!marker.exists());
}

#[test]
fn unknown_argument_fails_with_stderr() {
    let temp = TempDir::new("unknown-arg");
    let output = run(["--definitely-not-real"], temp.path(), temp.path());

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("prism: unknown argument: --definitely-not-real"));
}

#[test]
fn auto_run_plan_without_path_fails_before_repo_discovery() {
    let temp = TempDir::new("auto-run-plan-missing-path");
    let output = run(["auto", "run-plan"], temp.path(), temp.path());

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("prism: auto run-plan requires a plan path"));
}

#[test]
fn auto_run_plan_without_phase_headings_fails_before_launch_gates() {
    let temp = TempDir::new("auto-run-plan-no-phases");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    fs::write(repo.join("plan.md"), "# Notes\n\nNo phases yet.\n").expect("write plan");

    let output = run(["auto", "run-plan", "plan.md"], &repo, &config_home);

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("could not infer phases"));
}

#[test]
fn config_outside_git_repo_fails_with_stderr() {
    let temp = TempDir::new("outside-git");
    let output = run(["config"], temp.path(), temp.path());

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("prism:"));
}

#[cfg(unix)]
fn install_shim(bin: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(bin).expect("create shim bin");
    let path = bin.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"{name} test\"; fi\n"),
    )
    .expect("write shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod shim");
}

fn assert_db_has_tables<const N: usize>(db_path: &str, expected: [&str; N]) {
    let conn = rusqlite::Connection::open(db_path).expect("open db");
    let mut statement = conn
        .prepare("select name from sqlite_master where type = 'table'")
        .expect("prepare table list");
    let actual = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query table list")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("read table list");

    for table in expected {
        assert!(
            actual.contains(table),
            "missing table {table}; found {actual:?}"
        );
    }
}

#[cfg(unix)]
fn install_sqlite3_db_asserting_shim(bin: &Path, marker: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(bin).expect("create shim bin");
    let path = bin.join("sqlite3");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             test \"$#\" -eq 1 || exit 11\n\
             test -f \"$1\" || exit 12\n\
             printf '%s\\n' \"$1\" > \"{}\"\n",
            marker.display()
        ),
    )
    .expect("write sqlite3 shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod shim");
}

#[cfg(unix)]
fn install_git_proxy_shim(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(bin).expect("create shim bin");
    let path = bin.join("git");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nPATH='{}' exec git \"$@\"\n",
            std::env::var("PATH").unwrap_or_default()
        ),
    )
    .expect("write git shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod shim");
}
