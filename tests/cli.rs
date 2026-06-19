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
fn unknown_argument_fails_with_stderr() {
    let temp = TempDir::new("unknown-arg");
    let output = run(["--definitely-not-real"], temp.path(), temp.path());

    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("prism: unknown argument: --definitely-not-real"));
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
