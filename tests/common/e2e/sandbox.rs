use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use super::tmux::{run_tmux, session_names};
use super::tools::{assert_no_unsupported_events, read_events};

pub(crate) struct E2eSandbox {
    root: PathBuf,
    pub(crate) bin: PathBuf,
    pub(crate) config_home: PathBuf,
    pub(crate) home: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) repo: PathBuf,
    pub(crate) origin: PathBuf,
    pub(crate) worktrees: PathBuf,
    pub(crate) controller_socket: PathBuf,
    pub(crate) prism_socket: PathBuf,
    pub(crate) runtime_dir: PathBuf,
    pub(crate) real_git: PathBuf,
    pub(crate) real_tmux: PathBuf,
    keep: bool,
}

impl E2eSandbox {
    pub(crate) fn new(label: &str) -> Self {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let safe_label = label
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>();
        let root = env::temp_dir().join(format!(
            "prism-e2e-{safe_label}-{:x}-{unique:x}-{id:x}",
            std::process::id()
        ));
        let real_git = find_command("git").expect("E2E tests require git");
        let real_tmux = find_command("tmux").expect("E2E tests require tmux");
        let sandbox = Self {
            bin: root.join("bin"),
            config_home: root.join("config"),
            home: root.join("home"),
            state: root.join("state"),
            repo: root.join("repo"),
            origin: root.join("origin.git"),
            worktrees: root.join("worktrees"),
            controller_socket: root.join("controller-tmux/tmux.sock"),
            prism_socket: root.join("prism-tmux/tmux.sock"),
            runtime_dir: root.join("runtime"),
            keep: env::var_os("PRISM_E2E_KEEP").is_some(),
            root,
            real_git,
            real_tmux,
        };
        sandbox.initialize();
        sandbox
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn events_path(&self) -> PathBuf {
        self.state.join("events.jsonl")
    }

    pub(crate) fn events(&self) -> Vec<Value> {
        read_events(&self.events_path())
    }

    pub(crate) fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_STATE_HOME", self.root.join("xdg-state"))
            .env("XDG_RUNTIME_DIR", self.root.join("xdg-runtime"))
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("PRISM_RUNTIME_DIR", &self.runtime_dir)
            .env("TMUX_TMPDIR", self.root.join("prism-tmux"))
            .env("PRISM_E2E_ROOT", &self.root)
            .env("TERM", "xterm-256color")
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("PATH", env::var_os("PATH").unwrap_or_default());
        command
    }

    pub(crate) fn git(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = self.command(&self.real_git);
        command.current_dir(cwd).args(args);
        command.output().expect("run git")
    }

    pub(crate) fn git_stdout(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.git(cwd, args);
        assert!(
            output.status.success(),
            "git {args:?} failed in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    pub(crate) fn assert_clean_adapters(&self) {
        assert_no_unsupported_events(&self.events_path());
    }

    pub(crate) fn prism_tmux_sessions(&self) -> Vec<String> {
        session_names(&self.real_tmux, &self.prism_socket)
    }

    fn initialize(&self) {
        for path in [
            &self.bin,
            &self.config_home,
            &self.home,
            &self.state,
            &self.worktrees,
            &self.runtime_dir,
            &self.root.join("cache"),
            &self.root.join("xdg-state"),
            &self.root.join("xdg-runtime"),
            self.controller_socket.parent().unwrap(),
            self.prism_socket.parent().unwrap(),
        ] {
            fs::create_dir_all(path)
                .unwrap_or_else(|error| panic!("create {}: {error}", path.display()));
        }
        fs::write(
            self.state.join("real-git"),
            self.real_git.as_os_str().as_encoded_bytes(),
        )
        .expect("record real git path");
        fs::write(
            self.state.join("real-tmux"),
            self.real_tmux.as_os_str().as_encoded_bytes(),
        )
        .expect("record real tmux path");
        fs::write(self.events_path(), "").expect("create event log");
        self.install_adapters();
        self.initialize_git();
        self.write_prism_config();
    }

    fn install_adapters(&self) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-support/e2e/shims");
        for name in [
            "_common.py",
            "git-proxy",
            "gh",
            "wt",
            "harness",
            "opencode",
            "tmux",
        ] {
            let target = self.bin.join(name);
            fs::copy(source.join(name), &target)
                .unwrap_or_else(|error| panic!("install {name} adapter: {error}"));
            let mut permissions = fs::metadata(&target).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&target, permissions).unwrap();
        }
    }

    fn initialize_git(&self) {
        let origin_url = self.origin_url();
        fs::write(
            self.root.join("gitconfig"),
            format!(
                "[user]\n\tname = Prism E2E\n\temail = prism-e2e@example.invalid\n[init]\n\tdefaultBranch = main\n[protocol \"file\"]\n\tallow = always\n[url \"file://{}\"]\n\tinsteadOf = {}\n",
                self.origin.display(),
                origin_url
            ),
        )
        .expect("write isolated git config");
        run_checked(
            self.command(&self.real_git)
                .args(["init", "--bare"])
                .arg(&self.origin),
        );
        run_checked(self.command(&self.real_git).args(["init"]).arg(&self.repo));
        fs::write(self.repo.join("README.md"), "# Prism E2E\n").expect("write seed file");
        run_checked(
            self.command(&self.real_git)
                .current_dir(&self.repo)
                .args(["add", "README.md"]),
        );
        run_checked(
            self.command(&self.real_git)
                .current_dir(&self.repo)
                .args(["commit", "-m", "initial"]),
        );
        run_checked(self.command(&self.real_git).current_dir(&self.repo).args([
            "remote",
            "add",
            "origin",
            &origin_url,
        ]));
        run_checked(
            self.command(&self.real_git)
                .current_dir(&self.repo)
                .args(["push", "-u", "origin", "main"]),
        );
    }

    fn write_prism_config(&self) {
        let config_dir = self.config_home.join("prism");
        fs::create_dir_all(&config_dir).expect("create Prism config directory");
        let quoted = |path: &Path| toml::Value::String(path.display().to_string()).to_string();
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "default_harness = \"e2e\"\ndefault_base = \"main\"\nworktree_command = \"wt\"\n\n[harnesses.e2e]\nadapter = \"generic\"\ninteractive_command = [{harness}, \"interactive\", \"{{prompt}}\"]\ninteractive_prompt_transport = \"argument\"\nheadless_command = [{harness}, \"headless\"]\nheadless_prompt_transport = \"stdin\"\n\n[tools]\ngit = {git}\ngh = {gh}\nwt = {wt}\ntmux = {tmux}\n",
                harness = quoted(&self.bin.join("harness")),
                git = quoted(&self.bin.join("git-proxy")),
                gh = quoted(&self.bin.join("gh")),
                wt = quoted(&self.bin.join("wt")),
                tmux = quoted(&self.bin.join("tmux")),
            ),
        )
        .expect("write Prism config");
    }

    fn origin_url(&self) -> String {
        "https://github.com/prism-e2e/project.git".to_string()
    }

    fn print_debug_state(&self) {
        eprintln!("Prism E2E sandbox preserved at {}", self.root.display());
        let events = fs::read_to_string(self.events_path()).unwrap_or_default();
        eprintln!("--- adapter events ---\n{events}");
        let worktrees = self.git_stdout(&self.repo, &["worktree", "list", "--porcelain"]);
        eprintln!("--- git worktrees ---\n{worktrees}");
        let refs = self.git_stdout(&self.repo, &["show-ref"]);
        eprintln!("--- refs ---\n{refs}");
        self.print_tmux_state("controller", &self.controller_socket);
        self.print_tmux_state("prism", &self.prism_socket);
    }

    fn print_tmux_state(&self, label: &str, socket: &Path) {
        let panes = run_tmux(
            &self.real_tmux,
            socket,
            &[
                "list-panes",
                "-a",
                "-F",
                "#{session_name}:#{window_index}.#{pane_index}",
            ],
        );
        if !panes.status.success() {
            eprintln!("--- {label} tmux ---\nnot running");
            return;
        }
        let targets = String::from_utf8_lossy(&panes.stdout);
        eprintln!("--- {label} tmux panes ---\n{targets}");
        for target in targets.lines() {
            let capture = run_tmux(
                &self.real_tmux,
                socket,
                &["capture-pane", "-p", "-S", "-", "-t", target],
            );
            eprintln!(
                "--- {label} pane {target} ---\n{}",
                String::from_utf8_lossy(&capture.stdout)
            );
        }
    }
}

impl Drop for E2eSandbox {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.keep = true;
            self.print_debug_state();
        }
        let _ = run_tmux(&self.real_tmux, &self.controller_socket, &["kill-server"]);
        let _ = run_tmux(&self.real_tmux, &self.prism_socket, &["kill-server"]);
        if !self.keep {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn run_checked(command: &mut Command) {
    let display = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {display}: {error}"));
    assert!(
        output.status.success(),
        "{display} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn find_command(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?).find_map(|directory| {
        let candidate = directory.join(name);
        candidate.is_file().then_some(candidate)
    })
}
