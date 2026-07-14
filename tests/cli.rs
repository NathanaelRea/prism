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
    assert!(stdout(&output).contains("debug --help"));
    assert!(stderr(&output).is_empty());
}

#[test]
fn debug_help_prints_without_repo() {
    let temp = TempDir::new("debug-help");
    let output = run(["debug", "--help"], temp.path(), temp.path());

    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("Usage:\n  prism [--repo <path>] debug paths"));
    assert!(stdout.contains("debug logs"));
    assert!(stdout.contains("--log-level trace"));
    assert!(stderr(&output).is_empty());
}

#[test]
fn db_help_prints_without_repo() {
    let temp = TempDir::new("db-help");
    let output = run(["db", "--help"], temp.path(), temp.path());

    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = stdout(&output);
    assert!(stdout.contains("Usage:\n  prism [--repo <path>] db"));
    assert!(stdout.contains("db path"));
    assert!(stdout.contains("db <read-only-sql>"));
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
    assert!(example_stdout.contains("default_agent = \"opencode\""));
    assert!(example_stdout.contains("[worktrees]"));
    assert!(example_stdout.contains("auto_implement ="));

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

#[test]
#[cfg(unix)]
#[ignore = "requires PRISM_TEST_OPENCODE and PRISM_TEST_TMUX real binaries"]
fn real_prism_opencode_tmux_stack_ensures_reusable_agent_session() {
    let opencode = std::env::var("PRISM_TEST_OPENCODE")
        .expect("set PRISM_TEST_OPENCODE to a real OpenCode binary");
    let tmux = std::env::var("PRISM_TEST_TMUX").expect("set PRISM_TEST_TMUX to a real tmux binary");
    let temp = TempDir::new("real-agent-stack");
    let repo = temp.path().join("repo");
    let worktree = temp.path().join("feature");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    let opencode_home = temp.path().join("opencode-home");
    let opencode_data = temp.path().join("opencode-data");
    let opencode_config = temp.path().join("opencode-config");
    let tmux_socket = format!("prism-e2e-{}", std::process::id());
    for path in [&bin, &opencode_home, &opencode_data, &opencode_config] {
        fs::create_dir_all(path).expect("create E2E directory");
    }
    init_repo(&repo);
    run_git(&repo, &["config", "user.email", "prism@example.com"]);
    run_git(&repo, &["config", "user.name", "Prism E2E"]);
    fs::write(repo.join("README.md"), "Prism E2E\n").expect("write initial file");
    run_git(&repo, &["add", "README.md"]);
    run_git(&repo, &["commit", "-m", "initial"]);
    run_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature/e2e",
            worktree.to_str().expect("UTF-8 worktree path"),
        ],
    );
    let worktree = fs::canonicalize(worktree).expect("canonicalize worktree path");

    let real_home = std::env::var("HOME").unwrap_or_default();
    write_executable(
        &bin.join("opencode"),
        &format!(
            "#!/bin/sh\nexport HOME={}\nexport MISE_DATA_DIR={}\nexport npm_config_cache={}\nexport OPENCODE_CONFIG_DIR={}\nexport OPENCODE_DISABLE_AUTOUPDATE=true\nexport OPENCODE_DISABLE_DEFAULT_PLUGINS=true\nexport OPENCODE_DISABLE_LSP_DOWNLOAD=true\nexport OPENCODE_DISABLE_MODELS_FETCH=true\nexport XDG_DATA_HOME={}\nexec {} \"$@\"\n",
            shell_quote(&opencode_home.display().to_string()),
            shell_quote(&format!("{real_home}/.local/share/mise")),
            shell_quote(&format!("{real_home}/.npm")),
            shell_quote(&opencode_config.display().to_string()),
            shell_quote(&opencode_data.display().to_string()),
            shell_quote(&opencode),
        ),
    );
    write_executable(
        &bin.join("tmux"),
        &format!(
            "#!/bin/sh\nexec {} -L {} \"$@\"\n",
            shell_quote(&tmux),
            shell_quote(&tmux_socket),
        ),
    );
    let prism_config_dir = config_home.join("prism");
    fs::create_dir_all(&prism_config_dir).expect("create Prism config directory");
    fs::write(
        prism_config_dir.join("config.toml"),
        format!(
            "default_agent = \"opencode\"\ndefault_base = \"main\"\nopencode_port_base = 43000\nopencode_port_span = 1000\n\n[tools]\nopencode = \"{}\"\ntmux = \"{}\"\n",
            toml_escape(&bin.join("opencode").display().to_string()),
            toml_escape(&bin.join("tmux").display().to_string()),
        ),
    )
    .expect("write Prism config");
    let cleanup = FullStackCleanup {
        tmux: bin.join("tmux"),
        repo: repo.clone(),
        config_home: config_home.clone(),
    };

    let first = run_agent_ensure(&repo, &config_home);
    assert!(first.status.success(), "{}", stderr(&first));
    assert!(stderr(&first).is_empty(), "{}", stderr(&first));
    let first_stdout = stdout(&first);
    assert!(first_stdout.contains("branch = feature/e2e"));
    assert!(first_stdout.contains(&format!("worktree = {}", worktree.display())));
    assert!(first_stdout.contains("running = true"));
    let tmux_session = output_value(&first_stdout, "tmux_session");
    let opencode_session = output_value(&first_stdout, "opencode_session_id");
    let opencode_server_pid = output_value(&first_stdout, "opencode_server_pid");
    assert!(!opencode_session.is_empty());
    assert!(!opencode_server_pid.is_empty());

    let sessions =
        run_output(Command::new(&cleanup.tmux).args(["list-sessions", "-F", "#{session_name}"]));
    assert!(sessions.lines().any(|name| name == tmux_session));
    let windows = run_output(Command::new(&cleanup.tmux).args([
        "list-windows",
        "-t",
        tmux_session,
        "-F",
        "#{window_index}:#{window_name}",
    ]));
    assert!(windows.lines().any(|window| window == "1:opencode"));
    let first_pane_pid = run_output(Command::new(&cleanup.tmux).args([
        "display-message",
        "-p",
        "-t",
        &format!("{tmux_session}:1"),
        "#{pane_pid}",
    ]));

    let second = run_agent_ensure(&repo, &config_home);
    assert!(second.status.success(), "{}", stderr(&second));
    let second_stdout = stdout(&second);
    assert_eq!(output_value(&second_stdout, "tmux_session"), tmux_session);
    assert_eq!(
        output_value(&second_stdout, "opencode_session_id"),
        opencode_session
    );
    assert_eq!(
        output_value(&second_stdout, "opencode_server_pid"),
        opencode_server_pid
    );
    let second_pane_pid = run_output(Command::new(&cleanup.tmux).args([
        "display-message",
        "-p",
        "-t",
        &format!("{tmux_session}:1"),
        "#{pane_pid}",
    ]));
    assert_eq!(second_pane_pid, first_pane_pid);

    let db_path = run(["db", "path"], &repo, &config_home);
    assert!(db_path.status.success(), "{}", stderr(&db_path));
    let conn = rusqlite::Connection::open(stdout(&db_path).trim()).expect("open Prism database");
    let server_pid = conn
        .query_row(
            "select server_pid from opencode_runtime where branch = 'feature/e2e'",
            [],
            |row| row.get::<_, Option<u32>>(0),
        )
        .expect("read OpenCode server PID");
    assert!(server_pid.is_some());
}

#[cfg(unix)]
struct FullStackCleanup {
    tmux: PathBuf,
    repo: PathBuf,
    config_home: PathBuf,
}

#[cfg(unix)]
impl Drop for FullStackCleanup {
    fn drop(&mut self) {
        let _ = Command::new(&self.tmux).arg("kill-server").status();
        let db_path = run(["db", "path"], &self.repo, &self.config_home);
        if db_path.status.success()
            && let Ok(conn) = rusqlite::Connection::open(stdout(&db_path).trim())
            && let Ok(mut statement) = conn.prepare(
                "select server_pid, server_port from opencode_runtime where server_pid is not null",
            )
            && let Ok(processes) =
                statement.query_map([], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u16>(1)?)))
        {
            for (pid, port) in processes.flatten() {
                terminate_test_opencode(pid, port);
            }
        }
    }
}

#[cfg(unix)]
fn terminate_test_opencode(pid: u32, port: u16) {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output();
    let Ok(output) = output else {
        return;
    };
    let command = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !command.contains("opencode")
        || !command.contains("serve")
        || !command.contains(&format!("--port {port}"))
    {
        return;
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    for _ in 0..20 {
        if unsafe { libc::kill(pid as i32, 0) } != 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

#[cfg(unix)]
fn run_agent_ensure(repo: &Path, config_home: &Path) -> Output {
    prism()
        .args(["--repo", repo.to_str().expect("UTF-8 repo path")])
        .args(["agent", "ensure", "--branch", "feature/e2e"])
        .env("XDG_CONFIG_HOME", config_home)
        .env("HOME", config_home)
        .output()
        .expect("run prism agent ensure")
}

#[cfg(unix)]
fn output_value<'a>(output: &'a str, key: &str) -> &'a str {
    output
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key} = ")))
        .unwrap_or_else(|| panic!("missing {key} in output: {output}"))
}

#[cfg(unix)]
fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn run_output(command: &mut Command) -> String {
    let output = command.output().expect("run command");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod executable");
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
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
