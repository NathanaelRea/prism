# Configuration

Tracked repositories live in `~/.config/prism/repos.toml`.

Each repository entry has a path and may have a digit key. Digit keys are used as `Space <digit>` shortcuts in the TUI.

```toml
[[repos]]
path = "/path/to/repo"
key = "1"
```

Repository-specific Prism config lives under the repository config path shown by `e` from the Repos panel. Common settings include `default_base`, worktree columns, merge method, tools, and prompt templates.

Use `R` from Prism to edit repository order, keys, and tracked repositories.
