## Review

- **Correct:** Stages `:2` and `:3`, combined diffs, surrounding code, and branch commits `0e7c71b`, `5b7427d`, and `b880638` were inspected. Every listed conflict requires a **manual** merge; no hunk should be accepted wholesale as ours or theirs.
- **Note:** Requested `plan.md` and `progress.md` do not exist at the supplied paths.

### `src/persistence/pools.rs`

- **Blocker — lines 14–22, `initialize_repository_database`: manual.** Preserve both operations in this exact order:
  1. `prepare_parent(path)?`
  2. `secure_existing_database(path)?`
  3. `adopt_historical_repository_database(...).await?`
  4. `migrate(...).await?`
  5. `set_owner_only(path)`

  Security must precede adoption because adoption opens the existing database. Taking ours loses the main schema-adoption cutover; taking theirs opens existing paths before Windows ACL/reparse validation.

- **Blocker — lines 161–315, tests: manual.** Retain the single `#[cfg(test)] mod tests` from theirs, including both adoption tests, then add ours:
  ```rust
  #[cfg(windows)]
  use std::os::windows::fs::symlink_file;

  #[cfg(windows)]
  #[test]
  fn windows_database_reparse_target_is_rejected_before_migration() { ... }
  ```
  Keep the cross-platform `database_path`/`open_connection` helpers. Do not retain ours’ module-level `#[cfg(all(test, windows))]`, which would suppress adoption coverage on Unix.

- **Blocker — dependency outside the hunk:** `src/persistence/adoption.rs:139-193` uses `std::fs::rename(&temporary, &backup)` while `pools.rs:272-274` deliberately pre-creates a stale destination. Windows rename does not replace an existing destination. Integrating adoption unchanged therefore breaks `adopts_the_released_v2_schema_and_promotes_policy_cache` on Windows. Use the project’s Windows-aware atomic replacement mechanism, not remove-then-rename, so an interruption cannot discard the prior backup.

### `src/repository/lifecycle.rs`

- **Blocker — lines 18–111, `list_worktrees` and rebase helpers: manual.** Keep ours’ async ProcessKit API and port theirs’ rebase recovery:
  - `list_worktrees` remains `async`.
  - Initial `run_capture(...).await?`.
  - For detached entries, call `rebase_branch(...).await`.
  - Make `rebase_branch`, `git_succeeds`, and `git_exit_code` async.
  - Add `.await` to every `run_capture` and `run_output_allow_failure`.
  - Preserve theirs’ validation of `head-name`, `check-ref-format`, `show-ref`, detached `HEAD`, and the second file read.

  Do **not** take theirs wholesale: current `crate::process::{run_capture, run_output_allow_failure}` are async (`src/system/process/execution.rs:398-478`), so their synchronous calls cannot compile.

- **Blocker — lines 417–499, test imports/new tests: manual.**
  - Retain ours’ `#[cfg(unix)]` guards on shell-dependent imports.
  - Add `#[cfg(unix)] use std::process::Command;`.
  - Add both main tests, each as:
    ```rust
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn ...()
    ```
  - Change calls to `list_worktrees(...).await.unwrap()` and `rebase_branch(...).await`.
  - Keep ours’ async `create_worktree_session_clears_stale_hidden_marker`.

  Main’s tests use POSIX shell shims and Unix permissions and therefore must not become Windows tests.

- **Blocker — lines 1656–1669, `run_git`: manual.** Add theirs’ `run_git` helper with `#[cfg(unix)]`, then retain ours’ `#[cfg(unix)]` on `count_rows`. Taking theirs removes the Windows-safe test gating.

- **Correct:** Preserve the already auto-merged `git branch -D -- <branch>` behavior and corresponding assertions (`lifecycle.rs:322`, test regions around 974–1525). The `--` hardening is independent of async conversion.

### `src/repository/session.rs`

- **Blocker — lines 2334–2388, refresh tests: manual.**
  - Add theirs’ `rebase_detachment_refresh_preserves_branch_session`, but adapt it to:
    ```rust
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn rebase_detachment_refresh_preserves_branch_session()
    ```
    and retain `.await` on `refresh_worktree_sessions`.
  - Immediately afterward retain ours’ existing Unix async `detached_session_discovery_refresh_preserves_matching_session`.

  Taking theirs wholesale removes the async call required by `refresh_worktree_sessions` (`session.rs:693`) and exposes POSIX shell shims on Windows. This test depends on the async rebase recovery in `lifecycle::list_worktrees`.

### Validation after resolution

Run:

1. Focused rebase, detached-session, adoption, and Windows reparse tests.
2. `scripts/check.sh`.
3. Native `scripts/windows-check.ps1`, especially the adoption test with an existing backup.

The reported cargo delimiter failure is expected while conflict markers remain; no code was edited under the read-only instruction.