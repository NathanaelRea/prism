use std::path::Path;
use std::process::{Command, Output};

pub(crate) fn run_tmux(real_tmux: &Path, socket: &Path, args: &[&str]) -> Output {
    Command::new(real_tmux)
        .arg("-S")
        .arg(socket)
        .args(args)
        .env_remove("TMUX")
        .output()
        .unwrap_or_else(|error| panic!("run tmux {}: {error}", socket.display()))
}

pub(crate) fn session_names(real_tmux: &Path, socket: &Path) -> Vec<String> {
    let output = run_tmux(
        real_tmux,
        socket,
        &["list-sessions", "-F", "#{session_name}"],
    );
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

pub(crate) fn capture_pane(real_tmux: &Path, socket: &Path, target: &str) -> String {
    let output = run_tmux(real_tmux, socket, &["capture-pane", "-p", "-t", target]);
    assert!(
        output.status.success(),
        "tmux capture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
