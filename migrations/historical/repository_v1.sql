CREATE TABLE metadata (key text primary key, value text not null);
CREATE TABLE event (
          id integer primary key autoincrement, time_unix_ms integer not null,
          level text not null, target text not null, action text not null,
          operation_id text, parent_operation_id text, repo text, branch text,
          session text, message text not null, data_json text
        );
CREATE INDEX event_time_idx on event(time_unix_ms);
CREATE INDEX event_target_idx on event(target);
CREATE INDEX event_action_idx on event(action);
CREATE INDEX event_branch_idx on event(branch);
CREATE INDEX event_operation_idx on event(operation_id);
CREATE TABLE startup_run (
          id text primary key, time_started_unix_ms integer not null,
          time_finished_unix_ms integer, status text not null, repo text,
          version text not null, error text
        );
CREATE TABLE startup_phase (
          id integer primary key autoincrement,
          run_id text not null references startup_run(id) on delete cascade,
          phase text not null, time_started_unix_ms integer not null,
          time_finished_unix_ms integer, status text not null, error text
        );
CREATE TABLE task_metadata (
          branch text primary key,
          prompt_summary text not null,
          initial_prompt text not null,
          worktree text not null,
          classification text not null default 'work',
          visibility integer not null default 0,
          updated_unix_ms integer not null
        );
CREATE TABLE hidden_session (
          branch text primary key,
          hidden_unix_ms integer not null
        );
CREATE TABLE archived_worktree (
          branch text primary key,
          repo_root text not null,
          worktree_path text not null,
          archived_unix_ms integer not null,
          classification text not null default 'work'
        );
CREATE TABLE agent_state (
          branch text primary key,
          state text not null,
          updated_unix_ms integer not null
        );
CREATE TABLE worktree_harness (
          branch text primary key,
          worktree_path text not null,
          worktree_incarnation text not null,
          harness_id text not null,
          migration_policy text not null default 'ask',
          updated_unix_ms integer not null
        );
CREATE TABLE pending_worktree_deletion (
          branch text primary key,
          worktree_path text not null,
          worktree_incarnation text not null,
          branch_oid text,
          worktree_removed integer not null default 0,
          branch_deleted integer not null default 0,
          updated_unix_ms integer not null
        );
CREATE TABLE opencode_runtime (
          repo_root text not null,
          harness_id text not null default 'opencode',
          branch text not null,
          worktree_path text not null,
          server_port integer not null,
          server_url text not null,
          server_pid integer,
          opencode_session_id text,
          generation integer not null,
          updated_unix_ms integer not null,
          server_start_time_ticks integer,
          primary key (repo_root, harness_id, branch, worktree_path)
        );
CREATE INDEX opencode_runtime_branch_idx
          on opencode_runtime(repo_root, harness_id, branch);
CREATE TABLE plan_run (
          id text primary key,
          harness_id text not null default 'opencode',
          adapter_id text not null default 'opencode',
          repo_root text not null,
          scope_path text not null,
          plan_path text not null,
          plan_display text not null,
          step_name text not null,
          start_step integer not null,
          total_steps integer not null,
          mode text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step integer not null,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer
        );
CREATE TABLE plan_step_run (
          run_id text not null references plan_run(id) on delete cascade,
          step integer not null,
          prompt text not null,
          status text not null,
          opencode_state text,
          opencode_server_url text,
          opencode_session_id text,
          execution_state text,
          execution_process_id integer,
          execution_process_start_time_ticks integer,
          session_endpoint text,
          session_id text,
          session_adapter_id text,
          agent_variant text,
          process_id integer,
          started_unix_ms integer,
          finished_unix_ms integer,
          exit_code integer,
          latest_message text,
          active_tool text,
          todos_json text not null default '[]',
          summary text,
          error text,
          primary key (run_id, step)
        );
CREATE TABLE plan_output_line (
          run_id text not null,
          step integer not null,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (run_id, step, line_number),
          foreign key (run_id, step) references plan_step_run(run_id, step) on delete cascade
        );
CREATE INDEX plan_run_repo_idx
          on plan_run(repo_root, updated_unix_ms);
CREATE INDEX plan_run_scope_idx
          on plan_run(scope_path, updated_unix_ms);
CREATE INDEX plan_run_status_idx
          on plan_run(status, updated_unix_ms);
CREATE INDEX plan_output_line_step_idx
          on plan_output_line(run_id, step, line_number);
CREATE TABLE auto_run (
          id text primary key,
          harness_id text not null default 'opencode',
          adapter_id text not null default 'opencode',
          repo_root text not null,
          worktree_path text not null,
          worktree_incarnation text,
          branch text not null,
          mode text not null,
          implementation_source text not null default 'prompt',
          plan_path text,
          plan_run_mode text not null default 'sequential',
          variant text not null,
          agent_profile text,
          prompt_summary text not null,
          initial_prompt text not null,
          status text not null,
          pause_requested integer not null default 0,
          selected_step_run_id integer,
          change_request_identity_json text,
          pr_number integer,
          pr_url text,
          current_head_sha text,
          review_baseline_json text,
          stabilization_status text,
          stabilization_blocker text,
          stabilization_next_work text,
          pending_push_json text,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          archived_unix_ms integer,
          foreign key (selected_step_run_id) references auto_step_run(id) on delete set null
        );
CREATE TABLE auto_step_run (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          sequence integer not null,
          step_key text not null,
          reason text,
          status text not null,
          attempt integer not null,
          started_unix_ms integer,
          finished_unix_ms integer,
          opencode_server_url text,
          opencode_session_id text,
          process_id integer,
          execution_state text,
          execution_process_id integer,
          execution_process_start_time_ticks integer,
          session_endpoint text,
          session_id text,
          session_adapter_id text,
          plan_run_id text,
          commit_sha text,
          head_sha text,
          work_guard_json text,
          blocker text,
          summary text,
          error text,
          unique(run_id, sequence)
        );
CREATE TABLE auto_output_line (
          step_run_id integer not null references auto_step_run(id) on delete cascade,
          line_number integer not null,
          time_unix_ms integer not null,
          kind text not null,
          text text not null,
          block_id text,
          primary key (step_run_id, line_number)
        );
CREATE TABLE auto_event (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          step_run_id integer references auto_step_run(id) on delete set null,
          time_unix_ms integer not null,
          kind text not null,
          data_json text not null
        );
CREATE INDEX auto_run_repo_idx
          on auto_run(repo_root, updated_unix_ms);
CREATE INDEX auto_run_worktree_idx
          on auto_run(worktree_path, updated_unix_ms);
CREATE INDEX auto_run_status_idx
          on auto_run(status, updated_unix_ms);
CREATE INDEX auto_step_run_run_idx
          on auto_step_run(run_id, sequence);
CREATE INDEX auto_step_run_key_idx
          on auto_step_run(run_id, step_key, attempt);
CREATE INDEX auto_output_line_step_idx
          on auto_output_line(step_run_id, line_number);
CREATE INDEX auto_event_run_idx
          on auto_event(run_id, time_unix_ms);
CREATE TABLE auto_schema_version (
          id integer primary key check (id = 1),
          version integer not null
        );
CREATE TABLE workflow_execution (
          workflow_kind text not null,
          run_id text not null,
          dispatch_state text not null,
          worker_id text,
          daemon_instance_id text,
          lease_expires_unix_ms integer,
          heartbeat_unix_ms integer,
          fencing_token integer not null default 0,
          executor_pid integer,
          executor_process_identity text,
          requeue_requested integer not null default 0,
          interruption_generation integer not null default 0,
          recovery_decided_unix_ms integer,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          primary key (workflow_kind, run_id),
          check (workflow_kind in ('auto', 'plan')),
          check (dispatch_state in ('queued', 'claimed', 'recovery_pending', 'paused', 'terminal'))
        );
CREATE INDEX workflow_execution_dispatch_idx
          on workflow_execution(dispatch_state, updated_unix_ms);
CREATE INDEX workflow_execution_lease_idx
          on workflow_execution(dispatch_state, lease_expires_unix_ms);
CREATE INDEX workflow_execution_daemon_idx
          on workflow_execution(daemon_instance_id, dispatch_state);
CREATE TABLE pr_cache (
          branch text primary key, number integer not null, provider text,
          canonical_host text, project_path text, native_cr_id text, display_number integer,
          source_provider text, source_canonical_host text, source_project_path text,
          target_provider text, target_canonical_host text, target_project_path text,
          identity_complete integer not null default 0, title text not null,
          author text not null default '', body text not null default '', url text not null,
          state text not null, review_decision text not null,
          requested_reviewers text not null default '', head_ref text not null,
          base_ref text not null, head_sha text not null, updated_at text not null,
          check_status text not null, merge_state_status text not null default '',
          queue_state text not null default '', comment_count integer not null default 0,
          merged integer not null, draft integer not null, last_refreshed text not null,
          refreshed_unix_ms integer not null, observation_error text,
          native_state_evidence text not null default '{}'
        );
CREATE TABLE pr_details_cache (
          branch text primary key, pr_number integer, head_sha text, provider text,
          canonical_host text, project_path text, native_cr_id text, display_number integer,
          source_provider text, source_canonical_host text, source_project_path text,
          target_provider text, target_canonical_host text, target_project_path text,
          identity_complete integer not null default 0, comments text not null,
          reviews text not null, review_comments text not null, files text not null,
          failing_checks text not null, check_contexts text not null default '[]',
          ci_failures text not null default '[]', refreshed_unix_ms integer not null,
          observation_error text
        );
CREATE TABLE repo_policy_cache (
          repo_remote text primary key, provider text, canonical_host text, project_path text,
          target_branch text, identity_complete integer not null default 0,
          default_branch text, required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]', merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null, error text
        );
CREATE TABLE repo_policy_cache_v2 (
          provider text not null, canonical_host text not null, project_path text not null,
          project_path_key text not null default '', target_branch text not null,
          repo_remote text not null, default_branch text,
          required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]', merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null, error text,
          primary key (provider, canonical_host, project_path, target_branch)
        );
CREATE UNIQUE INDEX repo_policy_cache_v2_identity_key
             on repo_policy_cache_v2(provider, canonical_host, project_path_key, target_branch);
CREATE TABLE notification_session (
          worktree_path text not null,
          branch text not null,
          incarnation text not null,
          state text not null,
          transition_sequence integer not null,
          observed_unix_ms integer not null,
          primary key (worktree_path, branch, incarnation)
        );
CREATE TABLE notification_outbox (
          id integer primary key autoincrement,
          worktree_path text not null,
          branch text not null,
          incarnation text not null,
          transition_sequence integer not null,
          kind text not null,
          title text not null,
          body text not null,
          observed_unix_ms integer not null,
          expires_unix_ms integer not null,
          delivery_state text not null,
          attempt_count integer not null default 0,
          available_unix_ms integer not null,
          attempted_unix_ms integer,
          backend_accepted_unix_ms integer,
          superseded_unix_ms integer,
          last_failure_category text,
          unique (worktree_path, branch, incarnation, transition_sequence)
        );
CREATE INDEX notification_outbox_delivery_idx
          on notification_outbox(delivery_state, expires_unix_ms, id);

pragma user_version = 1;
