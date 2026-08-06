mod common;
#[path = "../src/persistence/cli_test_support.rs"]
mod persistence_test_support;

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::CompactTempDir as TempDir;

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
        .env("HOME", config_home)
        .env("PRISM_RUNTIME_DIR", config_home.join("runtime"));
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

fn contains_file_named(path: &Path, name: &str) -> bool {
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry.file_name() == OsStr::new(name)
            || entry.file_type().is_ok_and(|kind| kind.is_dir())
                && contains_file_named(&entry.path(), name)
    })
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
    assert!(stdout.contains("debug record"));
    assert!(stdout.contains("--log-level trace"));
    assert!(stderr(&output).is_empty());
}

#[test]
fn debug_record_requires_a_running_tui_without_touching_sqlite() {
    let temp = TempDir::new("debug-record-no-tui");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(
        ["debug", "record", "--before", "0", "--after", "0"],
        &repo,
        &config_home,
    );

    assert!(!output.status.success());
    assert!(stderr(&output).contains("no running Prism TUI recorder found"));
    assert!(!contains_file_named(&config_home, "prism.db"));
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
fn list_json_inspects_git_without_creating_prism_state() {
    let temp = TempDir::new("list-json-readonly");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    fs::write(repo.join("untracked.txt"), "dirty\n").unwrap();

    let output = run(["list", "--json"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["repositories"][0]["root"], canonical_display(&repo));
    assert_eq!(value["repositories"][0]["worktrees"][0]["git"]["dirty"], 1);
    assert!(!contains_file_named(&config_home, "prism.db"));
    assert!(!contains_file_named(&config_home, "run-markers"));
}

#[test]
#[cfg(unix)]
fn list_is_read_only_and_never_uses_network_tools() {
    let temp = TempDir::new("list-existing-db-readonly");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    let forbidden = temp.path().join("network-command");
    init_repo(&repo);
    fs::create_dir_all(config_home.join("prism")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    write_executable(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \" $* \" in *\" fetch \"*) printf fetch > {}; exit 97;; esac\nexec /usr/bin/git \"$@\"\n",
            shell_quote(&forbidden.display().to_string())
        ),
    );
    write_executable(
        &bin.join("gh"),
        &format!(
            "#!/bin/sh\nprintf gh > {}\nexit 98\n",
            shell_quote(&forbidden.display().to_string())
        ),
    );
    fs::write(
        config_home.join("prism/config.toml"),
        format!(
            "[tools]\ngit = \"{}\"\ngh = \"{}\"\n",
            toml_escape(&bin.join("git").display().to_string()),
            toml_escape(&bin.join("gh").display().to_string())
        ),
    )
    .unwrap();
    let initialized = run(["db", "path"], &repo, &config_home);
    assert!(initialized.status.success(), "{}", stderr(&initialized));
    let db_path = PathBuf::from(stdout(&initialized).trim());
    let before = fs::read(&db_path).unwrap();

    let output = run(["list", "--json"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_str::<serde_json::Value>(&stdout(&output)).unwrap();
    assert_eq!(fs::read(&db_path).unwrap(), before);
    assert!(!forbidden.exists());
    assert!(!config_home.join("runtime/worker.sock").exists());
}

#[test]
fn list_preserves_tracked_order_and_reports_missing_repositories() {
    let temp = TempDir::new("list-tracked-order");
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    let missing = temp.path().join("missing");
    let outside = temp.path().join("outside");
    let config_home = temp.path().join("xdg");
    init_repo(&first);
    init_repo(&second);
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(config_home.join("prism")).unwrap();
    fs::write(
        config_home.join("prism/repos.toml"),
        format!(
            "[[repos]]\npath = \"{}\"\nkey = \"2\"\n[[repos]]\npath = \"{}\"\n[[repos]]\npath = \"{}\"\nkey = \"1\"\n",
            second.display(),
            missing.display(),
            first.display()
        ),
    )
    .unwrap();

    let output = run(["list", "--json"], &outside, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["repositories"][0]["root"], canonical_display(&second));
    assert_eq!(value["repositories"][1]["root"], canonical_display(&first));
    assert_eq!(value["repositories"][0]["shortcut"], "2");
    assert_eq!(value["warnings"][0]["code"], "repository_discovery_failed");
    assert!(stderr(&output).contains(&missing.display().to_string()));
}

#[test]
fn list_accepts_command_local_repo_and_keeps_json_stdout_clean() {
    let temp = TempDir::new("list-command-local-repo");
    let repo = temp.path().join("repo");
    let outside = temp.path().join("outside");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    fs::create_dir_all(&outside).unwrap();

    let output = run(
        [
            "list",
            "--json",
            "--repo",
            repo.to_str().expect("UTF-8 repository path"),
        ],
        &outside,
        &config_home,
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["repositories"][0]["root"], canonical_display(&repo));
}

#[test]
fn status_defaults_to_current_worktree() {
    let temp = TempDir::new("status-current-worktree");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["status"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains(&format!("worktree = {}", canonical_display(&repo))));
    assert!(!contains_file_named(&config_home, "prism.db"));
}

#[test]
fn daemon_status_reports_stopped_without_starting_it() {
    let temp = TempDir::new("daemon-status-stopped");
    let output = prism()
        .args(["daemon", "status", "--json"])
        .current_dir(temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("HOME", temp.path())
        .env("PRISM_RUNTIME_DIR", temp.runtime_path())
        .output()
        .expect("run daemon status");

    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["daemon"]["state"], "stopped");
}

#[test]
#[cfg(unix)]
fn daemon_status_reports_invalid_runtime_configuration_distinctly() {
    use std::os::unix::ffi::OsStrExt;

    let temp = TempDir::new("daemon-status-invalid");
    let mut runtime = temp.path().join("runtime");
    while runtime.as_os_str().as_bytes().len() + b"/worker.sock".len() <= 103 {
        runtime.push("x");
    }
    let output = prism()
        .args(["daemon", "status", "--json"])
        .current_dir(temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("HOME", temp.path())
        .env("PRISM_RUNTIME_DIR", &runtime)
        .output()
        .expect("run daemon status");

    assert!(!output.status.success());
    assert!(stderr(&output).contains("103 bytes"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("PRISM_RUNTIME_DIR"),
        "{}",
        stderr(&output)
    );
    assert!(!runtime.exists());
}

#[test]
fn daemon_status_with_long_tmpdir_uses_the_explicit_compact_runtime() {
    let temp = TempDir::new("daemon-status-long-tmpdir");
    let long_tmpdir = temp.path().join("x".repeat(160));
    let runtime = temp.runtime_path().to_path_buf();
    let output = prism()
        .args(["daemon", "status", "--json"])
        .current_dir(temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("HOME", temp.path())
        .env("TMPDIR", long_tmpdir)
        .env("PRISM_RUNTIME_DIR", &runtime)
        .output()
        .expect("run daemon status");

    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["daemon"]["state"], "stopped");
    assert!(!runtime.exists());
}

#[test]
fn list_does_not_project_or_control_legacy_repository_runs() {
    let temp = TempDir::new("managed-plan-control");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    let repo = fs::canonicalize(repo).unwrap();
    let path_output = run(["db", "path"], &repo, &config_home);
    assert!(path_output.status.success(), "{}", stderr(&path_output));
    let db_path = PathBuf::from(stdout(&path_output).trim());
    persistence_test_support::install_plan_control_fixture(&db_path, &repo).unwrap();

    let listed = run(["list", "--all", "--json"], &repo, &config_home);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let value: serde_json::Value = serde_json::from_str(&stdout(&listed)).unwrap();
    assert_eq!(value["repositories"][0]["workflows"], serde_json::json!([]));

    let paused = run(["pause", "plan:plan-control-12345678"], &repo, &config_home);
    assert!(!paused.status.success());
    assert_eq!(
        persistence_test_support::plan_control_state(&db_path).unwrap(),
        ("queued".to_string(), 0, "queued".to_string())
    );
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
    assert!(stdout.contains("default_harness = opencode"));
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
    assert!(example_stdout.contains("[notifications]"));
    assert!(example_stdout.contains("enabled = true"));
    assert!(example_stdout.contains("completed = false"));
    assert!(example_stdout.contains("default_harness = \"opencode\""));
    assert!(example_stdout.contains("[worktrees]"));
    assert!(example_stdout.contains("auto_implement ="));

    let schema = run(["config", "schema"], &repo, &config_home);
    assert!(schema.status.success(), "{}", stderr(&schema));
    let schema_stdout = stdout(&schema);
    assert!(schema_stdout.contains(r#""title": "Prism Config""#));
    assert!(schema_stdout.contains(r#""merge_method""#));
    assert!(schema_stdout.contains(r#""notifications""#));

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
    assert!(stdout.contains("selected harness: opencode"));
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
fn database_migrations_and_schema_match_the_canonical_contract() {
    let temp = TempDir::new("db-contract");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["db", "path"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    assert_canonical_database_contract(stdout(&output).trim());
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
}

#[test]
fn repository_command_completes_only_its_own_run_marker() {
    let temp = TempDir::new("clean-run-marker");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["db", "path"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let db_path = PathBuf::from(stdout(&output).trim());
    let (run_id, status, finished) =
        persistence_test_support::latest_startup_run(&db_path).unwrap();
    assert_eq!(status, "ok");
    assert!(finished.is_some());
    let marker = db_path
        .parent()
        .unwrap()
        .join("run-markers")
        .join(format!("{run_id}.run"));
    let marker = fs::read_to_string(marker).unwrap();
    assert!(marker.contains("status=complete\n"));
    assert!(marker.contains("exit_status=ok\n"));
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
fn debug_integrity_reports_healthy_database_read_only() {
    let temp = TempDir::new("debug-integrity-healthy");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    let path_output = run(["db", "path"], &repo, &config_home);
    assert!(path_output.status.success(), "{}", stderr(&path_output));
    let path = stdout(&path_output).trim().to_string();
    let before = fs::read(&path).expect("read database before integrity check");

    let output = run(["debug", "integrity"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    assert!(output.contains(&format!("path = {path}")));
    assert!(output.contains("journal_mode = wal"));
    assert!(output.contains("main_bytes = "));
    assert!(output.contains("wal_bytes = "));
    assert!(output.contains("shm_bytes = "));
    assert!(output.contains("integrity_check:\n  ok"));
    assert!(output.contains("foreign_key_check:\n  ok"));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn debug_info_reports_passive_checkpoint_facts() {
    let temp = TempDir::new("debug-info-wal");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);

    let output = run(["debug", "info"], &repo, &config_home);

    assert!(output.status.success(), "{}", stderr(&output));
    let output = stdout(&output);
    assert!(output.contains("database_main_bytes = "));
    assert!(output.contains("database_wal_bytes = "));
    assert!(output.contains("database_shm_bytes = "));
    assert!(output.contains("wal_checkpoint_passive_busy = "));
    assert!(output.contains("wal_checkpoint_passive_log_frames = "));
    assert!(output.contains("wal_checkpoint_passive_checkpointed_frames = "));
}

#[test]
fn debug_integrity_reports_corruption_and_preserves_original_bytes() {
    let temp = TempDir::new("debug-integrity-corrupt");
    let repo = temp.path().join("repo");
    let config_home = temp.path().join("xdg");
    init_repo(&repo);
    let path_output = run(["db", "path"], &repo, &config_home);
    assert!(path_output.status.success(), "{}", stderr(&path_output));
    let path = PathBuf::from(stdout(&path_output).trim());
    let corrupt = b"this is not a sqlite database";
    fs::write(&path, corrupt).expect("corrupt database");

    let output = run(["debug", "integrity"], &repo, &config_home);

    assert!(!output.status.success());
    let output_text = stdout(&output);
    assert!(output_text.contains(&format!("path = {}", path.display())));
    assert!(output_text.contains("integrity_check:\n  ERROR:"));
    assert!(stderr(&output).contains("not a database"));
    assert_eq!(fs::read(&path).unwrap(), corrupt);
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
            "default_harness = \"opencode\"\ndefault_base = \"main\"\nopencode_port_base = 43000\nopencode_port_span = 1000\n\n[harnesses.opencode]\nadapter = \"opencode\"\nprogram = \"{}\"\n\n[tools]\ntmux = \"{}\"\n",
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
    let session_id = output_value(&first_stdout, "session_id");
    let runtime_process_id = output_value(&first_stdout, "runtime_process_id");
    assert!(!session_id.is_empty());
    assert!(!runtime_process_id.is_empty());

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
    assert_eq!(output_value(&second_stdout, "session_id"), session_id);
    assert_eq!(
        output_value(&second_stdout, "runtime_process_id"),
        runtime_process_id
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
    let server_pid = persistence_test_support::opencode_server_pid(
        Path::new(stdout(&db_path).trim()),
        "feature/e2e",
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
            && let Ok(processes) =
                persistence_test_support::opencode_processes(Path::new(stdout(&db_path).trim()))
        {
            for (pid, port) in processes {
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
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then [ \"{name}\" = wt ] && echo \"worktrunk 0.58.0\" || echo \"{name} test\"; fi\n"
        ),
    )
    .expect("write shim");
    let mut permissions = fs::metadata(&path).expect("shim metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod shim");
}

fn assert_canonical_database_contract(db_path: &str) {
    let (migrations, schema_fingerprint) =
        persistence_test_support::database_contract(Path::new(db_path))
            .expect("read canonical database contract");
    assert_eq!(
        migrations,
        include_str!("fixtures/sql/migrations.txt").trim()
    );
    assert_eq!(
        schema_fingerprint,
        include_str!("fixtures/sql/schema.sha256").trim()
    );
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
             test \"$#\" -eq 7 || exit 11\n\
             test \"$1\" = '-cmd' || exit 13\n\
             test \"$2\" = '.timeout 5000' || exit 14\n\
             test \"$3\" = '-cmd' || exit 15\n\
             test \"$4\" = 'PRAGMA foreign_keys=ON;' || exit 16\n\
             test \"$5\" = '-cmd' || exit 17\n\
             test \"$6\" = 'PRAGMA synchronous=FULL;' || exit 18\n\
             test -f \"$7\" || exit 12\n\
             printf '%s\\n' \"$7\" > \"{}\"\n",
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
