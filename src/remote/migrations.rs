use rusqlite::params;

pub(crate) fn migrate_pr_cache_schema(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists pr_cache (
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
        create table if not exists pr_details_cache (
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
        create table if not exists repo_policy_cache (
          repo_remote text primary key, provider text, canonical_host text, project_path text,
          target_branch text, identity_complete integer not null default 0,
          default_branch text, required_approvals integer not null default 0,
          require_conversation_resolution integer not null default 0,
          require_branch_up_to_date integer not null default 0,
          required_checks text not null default '[]', merge_queue_required integer not null default 0,
          refreshed_unix_ms integer not null, error text
        );
        create table if not exists repo_policy_cache_v2 (
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
        ",
    )
    .map_err(|error| format!("create PR cache schema: {error}"))?;

    for (table, column, definition, context) in [
        (
            "pr_cache",
            "body",
            "text not null default ''",
            "migrate pr_cache body column",
        ),
        (
            "pr_cache",
            "author",
            "text not null default ''",
            "migrate pr_cache author column",
        ),
        (
            "pr_cache",
            "comment_count",
            "integer not null default 0",
            "migrate pr_cache comment_count column",
        ),
        (
            "pr_cache",
            "merge_state_status",
            "text not null default ''",
            "migrate pr_cache merge_state_status column",
        ),
        (
            "pr_cache",
            "queue_state",
            "text not null default ''",
            "migrate pr_cache queue_state column",
        ),
        (
            "pr_cache",
            "requested_reviewers",
            "text not null default ''",
            "migrate pr_cache requested_reviewers column",
        ),
        (
            "pr_cache",
            "native_state_evidence",
            "text not null default '{}'",
            "migrate pr_cache native_state_evidence column",
        ),
        (
            "pr_details_cache",
            "ci_failures",
            "text not null default '[]'",
            "migrate pr_details_cache ci_failures column",
        ),
        (
            "pr_details_cache",
            "check_contexts",
            "text not null default '[]'",
            "migrate pr_details_cache check_contexts column",
        ),
        (
            "pr_details_cache",
            "pr_number",
            "integer",
            "migrate pr_details_cache pr_number column",
        ),
        (
            "pr_details_cache",
            "head_sha",
            "text",
            "migrate pr_details_cache head_sha column",
        ),
        (
            "pr_cache",
            "observation_error",
            "text",
            "migrate pr_cache observation_error column",
        ),
        (
            "pr_details_cache",
            "observation_error",
            "text",
            "migrate pr_details_cache observation_error column",
        ),
    ] {
        add_column_if_missing(conn, table, column, definition, context)?;
    }
    for (table, column, definition) in [
        ("pr_cache", "provider", "text"),
        ("pr_cache", "canonical_host", "text"),
        ("pr_cache", "project_path", "text"),
        ("pr_cache", "native_cr_id", "text"),
        ("pr_cache", "display_number", "integer"),
        ("pr_cache", "source_provider", "text"),
        ("pr_cache", "source_canonical_host", "text"),
        ("pr_cache", "source_project_path", "text"),
        ("pr_cache", "target_provider", "text"),
        ("pr_cache", "target_canonical_host", "text"),
        ("pr_cache", "target_project_path", "text"),
        (
            "pr_cache",
            "identity_complete",
            "integer not null default 0",
        ),
        ("pr_details_cache", "provider", "text"),
        ("pr_details_cache", "canonical_host", "text"),
        ("pr_details_cache", "project_path", "text"),
        ("pr_details_cache", "native_cr_id", "text"),
        ("pr_details_cache", "display_number", "integer"),
        ("pr_details_cache", "source_provider", "text"),
        ("pr_details_cache", "source_canonical_host", "text"),
        ("pr_details_cache", "source_project_path", "text"),
        ("pr_details_cache", "target_provider", "text"),
        ("pr_details_cache", "target_canonical_host", "text"),
        ("pr_details_cache", "target_project_path", "text"),
        (
            "pr_details_cache",
            "identity_complete",
            "integer not null default 0",
        ),
        ("repo_policy_cache", "provider", "text"),
        ("repo_policy_cache", "canonical_host", "text"),
        ("repo_policy_cache", "project_path", "text"),
        ("repo_policy_cache", "target_branch", "text"),
        (
            "repo_policy_cache",
            "identity_complete",
            "integer not null default 0",
        ),
    ] {
        add_column_if_missing(
            conn,
            table,
            column,
            definition,
            &format!("migrate {table} {column} column"),
        )?;
    }
    add_column_if_missing(
        conn,
        "repo_policy_cache_v2",
        "project_path_key",
        "text not null default ''",
        "migrate repository policy project path key",
    )?;
    conn.execute(
        "update repo_policy_cache_v2
            set project_path_key = case when provider = 'github' then lower(project_path) else project_path end
          where project_path_key = '' or project_path_key != case when provider = 'github' then lower(project_path) else project_path end",
        [],
    )
    .map_err(|error| format!("normalize repository policy project path keys: {error}"))?;
    conn.execute(
        "delete from repo_policy_cache_v2
          where rowid in (
            select rowid from (
              select rowid, row_number() over (
                partition by provider, canonical_host, project_path_key, target_branch
                order by refreshed_unix_ms desc, rowid desc
              ) as duplicate_rank from repo_policy_cache_v2
            ) where duplicate_rank > 1
          )",
        [],
    )
    .map_err(|error| format!("deduplicate repository policy project path keys: {error}"))?;
    conn.execute(
        "create unique index if not exists repo_policy_cache_v2_identity_key
             on repo_policy_cache_v2(provider, canonical_host, project_path_key, target_branch)",
        [],
    )
    .map_err(|error| format!("index repository policy project path keys: {error}"))?;
    backfill_legacy_github_url_identity(conn)?;
    Ok(())
}

fn add_column_if_missing(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    definition: &str,
    context: &str,
) -> Result<(), String> {
    if !table_has_column(conn, table, column)? {
        conn.execute(
            &format!("alter table {table} add column {column} {definition}"),
            [],
        )
        .map_err(|error| format!("{context}: {error}"))?;
    }
    Ok(())
}

fn backfill_legacy_github_url_identity(conn: &rusqlite::Connection) -> Result<(), String> {
    let rows = {
        let mut statement = conn
            .prepare("select branch, url, number from pr_cache where provider is null")
            .map_err(|error| format!("prepare legacy PR identity backfill: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|error| format!("read legacy PR identities: {error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("read legacy PR identity: {error}"))?
    };
    for (branch, url, number) in rows {
        let Some(project_path) = legacy_github_project_path_from_pr_url(&url) else {
            continue;
        };
        conn.execute(
            "update pr_cache
                set provider = 'github', canonical_host = 'github.com', project_path = ?2,
                    display_number = ?3, target_provider = 'github',
                    target_canonical_host = 'github.com', target_project_path = ?2
              where branch = ?1 and provider is null",
            params![branch, project_path, number],
        )
        .map_err(|error| format!("backfill legacy PR identity: {error}"))?;
    }
    conn.execute(
        "update pr_details_cache
            set pr_number = coalesce(pr_number, (select number from pr_cache where pr_cache.branch = pr_details_cache.branch)),
                provider = (select provider from pr_cache where pr_cache.branch = pr_details_cache.branch),
                canonical_host = (select canonical_host from pr_cache where pr_cache.branch = pr_details_cache.branch),
                project_path = (select project_path from pr_cache where pr_cache.branch = pr_details_cache.branch),
                display_number = coalesce(display_number, pr_number, (select number from pr_cache where pr_cache.branch = pr_details_cache.branch)),
                target_provider = (select target_provider from pr_cache where pr_cache.branch = pr_details_cache.branch),
                target_canonical_host = (select target_canonical_host from pr_cache where pr_cache.branch = pr_details_cache.branch),
                target_project_path = (select target_project_path from pr_cache where pr_cache.branch = pr_details_cache.branch)
          where provider is null
            and exists (select 1 from pr_cache where pr_cache.branch = pr_details_cache.branch and pr_cache.provider is not null)",
        [],
    )
    .map_err(|error| format!("backfill legacy PR details identity: {error}"))?;
    conn.execute(
        "update repo_policy_cache
            set provider = 'github', canonical_host = 'github.com', project_path = repo_remote,
                target_branch = default_branch,
                identity_complete = case when default_branch is not null and default_branch != '' then 1 else 0 end
          where provider is null and instr(repo_remote, '/') > 1",
        [],
    )
    .map_err(|error| format!("backfill legacy repository policy identity: {error}"))?;
    Ok(())
}

fn legacy_github_project_path_from_pr_url(url: &str) -> Option<String> {
    let remainder = url.strip_prefix("https://github.com/")?;
    let (project_path, number) = remainder.rsplit_once("/pull/")?;
    if project_path.split('/').count() != 2 || number.parse::<u64>().is_err() {
        return None;
    }
    crate::remote::RemoteRepositoryId::new(
        crate::remote::ProviderKind::GitHub,
        crate::remote::HostIdentity::new("github.com", None).ok()?,
        project_path,
    )
    .ok()
    .map(|repository| repository.project_path().to_string())
}

pub(crate) fn table_has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> Result<bool, String> {
    let mut statement = conn
        .prepare(&format!("pragma table_info({table})"))
        .map_err(|error| format!("prepare table info: {error}"))?;
    let mut rows = statement
        .query([])
        .map_err(|error| format!("read table info: {error}"))?;
    while let Some(row) = rows
        .next()
        .map_err(|error| format!("read column info: {error}"))?
    {
        let name = row
            .get::<_, String>(1)
            .map_err(|error| format!("read column name: {error}"))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}
