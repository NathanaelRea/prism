//! Prism process deadlines, teardown grace, capture ceilings, and descriptors.

use std::path::Path;
use std::time::Duration;

use processkit::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessDescriptor {
    pub(crate) name: &'static str,
}

impl ProcessDescriptor {
    pub const fn new(name: &'static str) -> Self {
        Self { name }
    }

    pub fn for_tmux(command: &Command) -> Self {
        let args = command
            .arguments()
            .iter()
            .filter_map(|argument| argument.to_str())
            .collect::<Vec<_>>();
        Self::new(infer_tmux_name(&args))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPolicy {
    Metadata,
    LocalMutation,
    NetworkQuery,
    WorkflowStep,
    TmuxPoll,
    TmuxCapture,
    #[cfg(all(test, unix))]
    Test,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PolicySettings {
    pub(crate) deadline: Duration,
    pub(crate) termination_grace: Duration,
    pub(crate) capture_bytes: usize,
}

impl PolicySettings {
    const fn new(deadline: Duration, capture_bytes: usize) -> Self {
        Self {
            deadline,
            termination_grace: Duration::from_secs(1),
            capture_bytes,
        }
    }
}

impl ProcessPolicy {
    pub(crate) fn settings(self) -> PolicySettings {
        match self {
            Self::Metadata => PolicySettings::new(Duration::from_secs(30), 1024 * 1024),
            Self::LocalMutation => {
                PolicySettings::new(Duration::from_secs(10 * 60), 4 * 1024 * 1024)
            }
            Self::NetworkQuery => PolicySettings::new(Duration::from_secs(5 * 60), 4 * 1024 * 1024),
            Self::WorkflowStep => {
                PolicySettings::new(Duration::from_secs(6 * 60 * 60), 4 * 1024 * 1024)
            }
            Self::TmuxPoll => PolicySettings::new(Duration::from_secs(15), 1024 * 1024),
            Self::TmuxCapture => PolicySettings::new(Duration::from_secs(4), 4 * 1024 * 1024),
            #[cfg(all(test, unix))]
            Self::Test => PolicySettings {
                deadline: Duration::from_millis(250),
                termination_grace: Duration::from_millis(100),
                capture_bytes: 1024,
            },
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::LocalMutation => "local_mutation",
            Self::NetworkQuery => "network_query",
            Self::WorkflowStep => "workflow_step",
            Self::TmuxPoll => "tmux_poll",
            Self::TmuxCapture => "tmux_capture",
            #[cfg(all(test, unix))]
            Self::Test => "test",
        }
    }
}

pub(crate) fn infer_descriptor(command: &Command) -> ProcessDescriptor {
    let program = Path::new(command.program())
        .file_name()
        .and_then(|program| program.to_str())
        .unwrap_or_default();
    let args = command
        .arguments()
        .iter()
        .filter_map(|argument| argument.to_str())
        .collect::<Vec<_>>();
    let name = match program {
        "gh" => match args.as_slice() {
            ["pr", "create", ..] => "gh.pr.create",
            ["pr", "merge", ..] => "gh.pr.merge",
            ["pr", "view", ..] => "gh.pr.view",
            ["pr", "list", ..] => "gh.pr.list",
            ["api", "graphql", ..] => "gh.api.graphql",
            ["run", "list", ..] => "gh.run.list",
            ["run", "view", ..] => "gh.run.view",
            ["auth", "status", ..] => "gh.auth.status",
            _ => "process.other",
        },
        "git" => infer_git_name(&args),
        "tmux" => infer_tmux_name(&args),
        "fzf" => "fzf.select",
        "lazygit" => "lazygit.open",
        "sqlite3" => "sqlite.shell",
        "date" => "system.time.format",
        "open" | "xdg-open" => "browser.open",
        _ => "process.other",
    };
    ProcessDescriptor::new(name)
}

fn infer_git_name(args: &[&str]) -> &'static str {
    let mut index = 0;
    while index < args.len() {
        match args[index] {
            "-C" | "--git-dir" | "--work-tree" | "-c" => index += 2,
            argument if argument.starts_with('-') => index += 1,
            operation => {
                return match (operation, args.get(index + 1).copied()) {
                    ("fetch", _) => "git.fetch",
                    ("push", _) => "git.push",
                    ("pull", _) => "git.pull",
                    ("ls-remote", _) => "git.ls_remote",
                    ("remote", Some("update")) => "git.remote.update",
                    ("remote", Some("get-url")) => "git.remote.get_url",
                    ("status", _) => "git.status",
                    ("show-ref", _) => "git.show_ref",
                    ("worktree", Some("list")) => "git.worktree.list",
                    ("worktree", Some("add")) => "git.worktree.add",
                    ("worktree", Some("remove")) => "git.worktree.remove",
                    ("worktree", Some("prune")) => "git.worktree.prune",
                    ("switch", _) => "git.switch",
                    ("rev-list", _) => "git.rev_list",
                    ("rev-parse", _) => "git.rev_parse",
                    ("add", _) => "git.add",
                    ("commit", _) => "git.commit",
                    ("branch", _) => "git.branch",
                    ("merge-tree", _) => "git.merge_tree",
                    ("merge", _) => "git.merge",
                    _ => "process.other",
                };
            }
        }
    }
    "process.other"
}

fn infer_tmux_name(args: &[&str]) -> &'static str {
    args.iter()
        .find_map(|argument| match *argument {
            "load-buffer" => Some("tmux.buffer.load"),
            "paste-buffer" => Some("tmux.buffer.paste"),
            "list-sessions" => Some("tmux.session.list"),
            "has-session" => Some("tmux.session.exists"),
            "new-session" => Some("tmux.session.create"),
            "attach-session" => Some("tmux.session.attach"),
            "kill-session" => Some("tmux.session.kill"),
            "set-option" => Some("tmux.option.set"),
            "list-windows" => Some("tmux.window.list"),
            "new-window" => Some("tmux.window.create"),
            "move-window" => Some("tmux.window.move"),
            "rename-window" => Some("tmux.window.rename"),
            "resize-window" => Some("tmux.window.resize"),
            "capture-pane" => Some("tmux.pane.capture"),
            "display-message" if args.contains(&"#{pane_start_command}") => {
                Some("tmux.pane.start_command")
            }
            "display-message" => Some("tmux.pane.current_command"),
            "send-keys" => Some("tmux.pane.start_command"),
            _ => None,
        })
        .unwrap_or("process.other")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_use_only_finite_known_labels() {
        assert_eq!(
            infer_descriptor(&Command::new("git").args(["-C", "/secret/repo", "fetch", "origin"])),
            ProcessDescriptor::new("git.fetch")
        );
        assert_eq!(
            infer_descriptor(&Command::new("gh").args(["api", "graphql", "-f", "query=secret"])),
            ProcessDescriptor::new("gh.api.graphql")
        );
        assert_eq!(
            infer_descriptor(&Command::new("/tmp/custom-tool").args(["fetch", "secret"])),
            ProcessDescriptor::new("process.other")
        );
    }
}
