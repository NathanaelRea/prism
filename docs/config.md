# Configuration

Tracked repositories live in `~/.config/prism/repos.toml`.

Global Prism settings live in `~/.config/prism/config.toml`. Open `Space c`, then choose global settings to edit this file and reload configuration.

Prism uses the first non-empty value of `VISUAL` and `EDITOR` when opening
configuration files. These values use Prism's command-word grammar, so quoted
arguments retain their boundaries and are launched directly rather than through
a shell. For example, `VISUAL='code --wait --profile "Prism Work"'` launches
`code` with those three arguments followed by the configuration path. Without
either variable, Prism tries `nvim`, `vim`, then `vi`.

Repository terminals and tmux terminal windows use a non-empty `SHELL`, falling
back to `/bin/sh`. Shell command strings that Prism must pass to tmux use POSIX
single-argument quoting; configurable tools and editors remain direct-argv
integrations.

Run `prism config example` to print the complete default config with active values, `prism config schema` to print the JSON Schema used by TOML editor tooling, and `prism config paths` to inspect the active config paths and schema URL.

Each repository entry has a path and may have a digit key. Digit keys are used as `Space <digit>` shortcuts in the TUI.

```toml
[[repos]]
path = "/path/to/repo"
key = "1"
```

Repository-specific Prism config lives under the repository destination in `Space c`. Common settings include `default_base`, layout width, worktree columns, merge method, tools, and prompt templates. Harness selection and definitions are global-only.

Per-repository Prism state also lives under that repository config directory, not inside the project repository. The state database is named `prism.db` and stores worktree session metadata, harness runtime records, change-request cache data, and observability records. Prompt Workflow Runs are stored durably in the user-scoped `workflow.db` owned by the Prism Worker.

Use the tracked repositories/keybindings destination under `Space c` to edit repository order, keys, and tracked repositories.

```toml
#:schema https://raw.githubusercontent.com/NathanaelRea/prism/main/schemas/config.schema.json

default_base = "main"
merge_method = "squash"

# Prism starts shared repository OpenCode servers on deterministic ports in this range.
opencode_port_base = 41000
opencode_port_span = 1000

# Default false keeps OpenCode servers warm after Prism exits.
opencode_shutdown_owned_servers = false

[layout]
sidebar_width = 56

[ui]
icon_style = "unicode" # or "nerd-font"

[notifications]
enabled = true
needs_input = true
completed = false
failed = true

[worktrees]
columns = []

[tools]

[remote_hosts."git.example.com"]
provider = "forgejo"
credential_env = "FORGEJO_TOKEN" # variable name only, never the token

[prompt_templates]
# Workflow prompts live directly in editable prompt-first Workflow TOML files.
```

The `#:schema` line is an optional TOML comment. Prism ignores it, while Taplo-compatible TOML language servers can use it for completions, descriptions, enum values, and type validation.

Prism treats `main` as the default branch by default. The default branch is not polled or shown as a change-request branch.

Prism uses squash merges for change requests by default. Set `merge_method` to
`merge` or `rebase` when the selected provider supports that method. GitLab does
not expose rebase through Prism's merge-request merge operation.

## Remote Hosts

Prism recognizes `github.com`, `gitlab.com`, and `codeberg.org` without configuration. Codeberg uses the Forgejo adapter. Other hostnames are never probed until they are explicitly mapped:

```toml
[remote_hosts."git.example.com"]
provider = "forgejo" # github, gitlab, or forgejo
web_url = "https://git.example.com"
api_url = "https://git.example.com/api/v1" # optional
credential_env = "FORGEJO_TOKEN" # environment variable name, not its value
```

Mappings inherit from the user config into repository config; a repository mapping with the same hostname replaces the inherited mapping. HTTPS is required by default. For a trusted development host only, set `allow_http = true` and use explicit `http://` base URLs.

GitHub uses `gh` for authentication and transport. GitLab uses `glab`. Forgejo reads a token only from the configured environment variable. Prism does not store token values in TOML or SQLite and does not probe unknown hosts.

`prism doctor` reports the resolved provider, canonical host/project, transport,
authentication availability, capabilities, and Forgejo version when reachable.
Current capability exceptions are intentional:

- GitHub review submission remains available through `gh`; GitLab and Forgejo
  review submission is not exposed as a generic operation.
- GitLab CI traces and policy depend on project permissions and product tier.
  Rebase cannot be selected through GitLab's merge-request merge operation.
- Forgejo and Codeberg review-conversation resolution and merge queues are
  unsupported. Actions logs are conditional on repository Actions availability;
  external status providers do not imply log access.
- Prism discovers Forgejo's API version and paging settings at runtime. Read
  observations retain unknown states, while create and merge are currently
  qualified for Forgejo majors 9 through 16; other majors fail closed.

See [Remote Hosting](remote-hosting.md) for the full capability matrix,
qualified server versions, authentication commands, Codeberg CI limitations,
and doctor/debug troubleshooting.

## Workflow health and retention

`prism workflow validate` checks prompt-first source, graph structure, Trigger
resolution, and explicit Agent selections. `prism workflow history --json`
reports durable runs and lifecycle Attempts through the stable JSON envelope.
`prism doctor` reports discovered Workflow provenance and validation failures.
`prism debug paths` prints the user-wide Workflow database and Worker locations.

The Worker retains immutable Workflow snapshots, pinned external Trigger bytes,
prepared state, fresh Agent Session identity/final text, lifecycle events, and
durable remote-lane cooldowns. An incompatible pre-cutover Workflow database is
backed up once and replaced; generalized source in the user Workflow directory
is archived rather than reinterpreted.

Repository `.prism/workflows` and `.prism/triggers` resources are ignored until
their exact combined revision is trusted. Preview with
`prism --repo <path> workflow trust-repository`; apply explicitly with `--apply`.
Any resource edit invalidates that trust. External Triggers execute with the
user's full OS authority and are not sandboxed.

## Desktop notifications

Desktop notifications are enabled by default for sessions waiting for input and for failures; `failed` also covers sessions that need to be restarted. Successful completion notifications default to off. The global switch and category switches may be overridden globally or in a repository config. Reloading global or repository config through `Space c` changes subsequent notifications without reporting sessions that are already blocked or finished.

The Prism Worker observes interactive Agent Sessions and owns a durable notification outbox. The first observation establishes a baseline, so starting or upgrading Prism does not replay existing attention states. Newer state changes supersede obsolete pending notifications, and notifications expire after ten minutes rather than arriving as a stale burst. `backend_accepted_unix_ms` records when the platform backend accepted a notification; desktop systems do not report whether a user saw it.

On Linux, the worker uses the desktop notification service directly and does not run `notify-send` or detect a desktop environment. Notifications therefore continue while Prism is attached to tmux or the dashboard is closed. GNOME, KDE, and similar desktops normally provide a notification server. Minimal Wayland compositors such as Hyprland and Sway require a daemon such as `mako`, `swaync`, or `dunst`. A missing server is non-fatal and delivery is retried until the notification expires.

On macOS, the worker forwards notifications to the most recently connected Prism dashboard, which asks its terminal to notify through the OSC 9 terminal protocol. Notification Center therefore lists the terminal, such as Ghostty, as the sender; enable notifications for that terminal and check its Focus and display preferences. The terminal must support OSC 9. Notifications remain pending while no dashboard is connected and expire after ten minutes. Headless and container sessions have no terminal notification service; an SSH client's support depends on whether it forwards the control sequence.

Notifications are best effort and contain only the repository label, branch, and state description. OpenCode provides semantic input, completion, and failure observations. Other interactive harnesses currently provide tmux process-liveness observations, so Prism can report their completion but cannot infer input-required state by scraping terminal output.

## Harnesses

Harnesses are configured only in `~/.config/prism/config.toml`. OpenCode, Codex CLI, Claude Code, and Pi have built-in definitions using their standard executable names. OpenCode remains the default:

```toml
default_harness = "opencode"

[harnesses.opencode]
program = "opencode"
```

When `default_harness` has not been configured, the first interactive TUI startup lists the installed built-in harnesses and saves the selection. Prism writes only `default_harness`; other settings continue to use built-in defaults. Non-interactive startup cannot prompt and retains the OpenCode fallback.

Prism owns each built-in adapter's structured-output, prompt, session, and protocol flags. `program` may select an executable path or wrapper. `arguments` may contain adapter-approved options such as model, sandbox, permission, or tool settings, but Prism rejects protocol-critical overrides.

Choose Harness selection under `Space c` to switch the global default. The chooser lists the four built-ins and configured generic harnesses; the current harness is shown in dark gray and cannot be selected. It can also add a generic harness by collecting its interactive command, optional initial-prompt transport, and optional headless command. Prompt transports that do not match the command's placeholders are disabled. The built-in IDs `opencode`, `codex`, `claude`, and `pi` are reserved: each always selects its matching adapter, and built-in adapters cannot be aliased under custom IDs.

Select another built-in harness without repeating its standard program:

```toml
default_harness = "codex"
```

Add a harness table only to override its executable or pass adapter-approved arguments:

```toml
default_harness = "codex"

[harnesses.codex]
program = "/opt/bin/codex"
arguments = ["--sandbox", "workspace-write", "--ask-for-approval", "on-request"]
```

Codex uses JSONL from `codex exec --json`; Claude uses print-mode stream JSON; Pi uses JSON print mode. Prism preserves each tool's approval and sandbox defaults unless explicit adapter arguments change them. Current supported-version diagnostics are Codex 0.145.0+, Claude 2.1.214+, Pi 0.81.1+, and current stable OpenCode.

Aider does not expose a reliable interactive initial-prompt contract, so it remains a generic adapter with optional plain-text headless execution:

```toml
[harnesses.aider]
adapter = "generic"
interactive_command = ["aider"]
headless_command = ["aider", "--message", "{prompt}"]
headless_prompt_transport = "argument"
```

Google Antigravity CLI is supported through a generic interactive configuration only. Its official `agy` CLI has interactive resume but no documented headless automation or structured-output contract, so Prism does not advertise a named managed adapter:

```toml
[harnesses.antigravity]
adapter = "generic"
interactive_command = ["agy"]
```

An arbitrary command can provide the generic interactive floor. Commands are arrays so prompts and shell metacharacters remain single arguments and are never evaluated by a shell:

```toml
default_harness = "company-agent"

[harnesses.company-agent]
adapter = "generic"
interactive_command = ["company-agent"]
headless_command = ["company-agent", "run", "--prompt", "{prompt}"]
headless_prompt_transport = "argument"
output_format = "text"
```

Generic headless prompt transport may be `argument`, `stdin`, or `temp-file`. `{prompt}` or `{prompt_file}` must occupy one complete array item. Generic interactive initial prompts require an explicit `argument` or `temp-file` transport; Prism does not guess terminal readiness or paste into unknown harnesses. Generic managed runs report bounded plain text and process exit status, not structured tool/session state.

When the global harness changes, opening an existing Worktree Session offers `Migrate`, `Later`, and `Keep`. `Migrate` retires its old tmux generation; `Later` asks again next time; `Keep` pins the old harness. Press `M` in the Worktrees panel to migrate a pinned session explicitly.

The previous keys are intentionally rejected. Replace this:

```toml
default_agent = "opencode"
[tools]
opencode = "/opt/bin/opencode"
[agents.opencode]
command = "opencode run --format json"
prompt_mode = "argument"
```

with:

```toml
default_harness = "opencode"
[harnesses.opencode]
program = "/opt/bin/opencode"
```

Prism shares one local OpenCode server across worktree sessions that use the same harness in a repository. Each worktree keeps an independent native OpenCode session and tmux client. `opencode_port_base` and `opencode_port_span` define the deterministic local port range used for repository servers. By default Prism keeps servers warm after the TUI exits; set `opencode_shutdown_owned_servers = true` to send SIGTERM to shared OpenCode servers that this Prism process spawned, disconnecting every worktree client using them.

`[layout] sidebar_width` controls the Status/Repos/Worktrees sidebar width in terminal columns. Values are bounded to `20..=120`. When the terminal is too narrow, Prism reduces the configured width so the main panel keeps usable space; this preserves the board layout instead of strictly honoring a width that would hide the main panel.

`[ui] icon_style` controls TUI status glyphs. `unicode` is the portable default. `nerd-font` uses richer Nerd Font glyphs for pull requests, merge state, Git status, and CI, and requires a Nerd Font configured in your terminal.

`[worktrees] columns` controls the visible extra columns in the TUI worktree list. There are no extra columns enabled by default. Columns are shown in the configured order after Prism's built-in worktree indicators. Missing values render as a compact placeholder so neighboring columns stay aligned.

Columns are read from `wt list --format=json`. Common names include `url`, `url_active`, `ci.status`, and `vars.<name>`, such as `vars.localdev`:

```toml
[worktrees]
columns = ["url", "url_active", "ci.status", "vars.localdev"]
```

Use the worktree columns destination under `Space c` to open the selected repository's selector. The selector lists configured columns first and then discovered `wt` column keys, so you can enable/disable columns and move enabled columns up/down without editing TOML directly.

## Worktrunk Environments

Prism requires Worktrunk 0.58.0 or newer and currently tests against 0.71.0. Worktrunk project configuration belongs in the managed repository's `.config/wt.toml`; Prism reads the same machine output as standalone `wt list` and does not duplicate that configuration.

Worktrunk's personal worktree path policy belongs in its user configuration. Choose Worktrunk configuration under `Space c` to discover and open that file through Worktrunk. If the file is missing, Prism offers to create it with `wt config create`; Prism never parses or writes the file itself. The dialog makes explicit that changes affect both Prism and standalone `wt` commands. Use Worktrunk's top-level `worktree-path` for a global policy or its user-level `[projects."<identifier>"]` table for a personal repository override.

Run long-lived development servers from a background `post-start` hook, not a blocking `pre-start` hook. `wt step tether` makes Worktrunk responsible for terminating the process tree when the worktree is removed:

```toml
[post-start]
dev = "wt step tether -- pnpm dev -- --port {{ branch | hash_port }}"

[list]
url = "http://localhost:{{ branch | hash_port }}"
```

The stable `hash_port` value is owned by Worktrunk. The URL appears in standalone `wt list` and in Prism's Worktree Session details. Prism reports Worktrunk's listening, not-listening, unknown, or stale observation and opens a known HTTP(S) URL with plain `o`; it does not own the server or infer liveness from a hook log. URL columns remain opt-in through the `Space c` worktree columns destination and may use `url` and `url_active`.

Worktrunk 0.58.0 emits the schema-1 bare array. Newer Worktrunk can emit schema 1 or the schema-2 envelope according to its `[list] json-schema` setting. Prism normalizes both without changing the user's setting. An unknown schema fails closed: the last successful observation remains visible as stale with a safe error instead of being replaced by empty columns. Facts join a Worktree Session only by repository and exact normalized worktree path, never by branch name.

Worktrunk owns project-command approval. Prism never supplies `--yes`; when commands are new or changed, approve them interactively with the action Prism offers or with `wt config approvals add` after reviewing them.

Press `L` in the Worktrees panel to choose a Worktrunk hook log. Prism displays Worktrunk's branch label, source, hook type, name, size, and modification time, then reads only a bounded sanitized tail from a regular file below `.git/wt/logs`. A branch-label match is only a picker preference, log bodies are not persisted or included in diagnostics, and a log file does not prove that a hook or development process is running.

## Database Access

Use `prism db` commands to inspect a repository's local Prism state:

```sh
prism db
prism db path
prism db "select name from sqlite_schema where type = 'table' order by name"
prism db 'select id, status from plan_run order by updated_unix_ms desc'
prism debug integrity
```

Bare `prism db` opens an interactive `sqlite3` shell for the selected repository database. Prism initializes and migrates the database before launching the shell, then configures the shell with a five-second busy timeout, foreign keys enabled, and `synchronous=FULL`. This is direct writable SQLite access; quit Prism first if you are doing manual repairs to avoid lock contention or conflicting writes.

`prism db path` prints the selected repository database path and exits.

`prism db <query>` runs a read-only query and prints tab-separated rows for scripts. Write statements are rejected in query mode. Query mode uses Prism's built-in SQLite support and does not require the external `sqlite3` command.

Prism databases require WAL mode and `synchronous=FULL`. WAL is requested and verified before versioned migrations acquire SQLite's immediate write lock. The database location must be on a local filesystem that supports SQLite WAL shared memory; Prism fails explicitly instead of silently falling back to a rollback journal. Do not copy only `prism.db` while Prism is running because committed data may still be in the `-wal` file.

`prism debug integrity` opens the existing database read-only, prints its path, main/WAL/shared-memory file sizes, schema version, journal mode, complete `integrity_check` output, and `foreign_key_check` output. It exits nonzero on any failure and never migrates, repairs, recreates, checkpoints, or writes observability data. `prism debug info` additionally reports the result of an explicitly requested passive WAL checkpoint; normal TUI reads never request a checkpoint.

Prism keeps a locked per-process run marker for every repository database it attaches. A marker left unlocked and incomplete by a crash or `SIGKILL` causes the next process to run read-only `quick_check` and `foreign_key_check` before normal database use; live concurrent processes keep their marker locked and are not classified as unclean. Failed checks stop startup without repair or recreation. SQLite corruption-class errors independently trigger the same best-effort read-only diagnostics while preserving the original error and database.

When running outside the checkout you want to inspect, select the repository explicitly:

```sh
prism --repo /path/to/repo db
prism --repo /path/to/repo db path
```

If bare `prism db` reports that `sqlite3` is missing, install the SQLite command-line shell and make sure `sqlite3` is on your `PATH`.
