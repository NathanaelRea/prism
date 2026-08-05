use rusqlite::{Connection, OptionalExtension, params};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MergeIntentState {
    Armed,
    Withdrawn,
    Superseded,
    Merged,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationPlacement {
    Pending,
    NotReady,
    Ready,
    Backlogged,
    Reserved,
    Updating,
    Submitting,
    Submitted,
    Withdrawn,
    Merged,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CandidateGeneration {
    pub(crate) change_request_identity: crate::remote::CanonicalChangeRequestIdentity,
    pub(crate) target_branch: String,
    pub(crate) pr_number: u64,
    pub(crate) head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MergeIntent {
    pub(crate) id: i64,
    pub(crate) run_id: String,
    pub(crate) generation: u64,
    pub(crate) state: MergeIntentState,
    pub(crate) placement: IntegrationPlacement,
    pub(crate) lane_key: Option<String>,
    pub(crate) head_sha: Option<String>,
    pub(crate) ready_sequence: Option<u64>,
}

pub(crate) fn migrate_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
        create table if not exists merge_intent (
          id integer primary key autoincrement,
          run_id text not null references auto_run(id) on delete cascade,
          generation integer not null,
          state text not null,
          placement text not null,
          change_request_identity_json text,
          lane_key text,
          target_branch text,
          pr_number integer,
          head_sha text,
          ready_sequence integer,
          created_unix_ms integer not null,
          updated_unix_ms integer not null,
          unique(run_id, generation)
        );

        create unique index if not exists merge_intent_active_run_idx
          on merge_intent(run_id) where state = 'armed';
        create index if not exists merge_intent_lane_ready_idx
          on merge_intent(lane_key, state, ready_sequence);

        create table if not exists integration_lane (
          lane_key text primary key,
          next_ready_sequence integer not null default 1,
          reserved_intent_id integer references merge_intent(id) on delete set null,
          updated_unix_ms integer not null
        );
        ",
    )
    .map_err(|error| format!("create integration schema: {error}"))
}

pub(crate) fn toggle_merge_intent(
    conn: &Connection,
    run_id: &str,
    default_armed: bool,
) -> Result<MergeIntent, String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("begin merge intent toggle: {error}"))?;
    let current = active_merge_intent(&tx, run_id)?;
    let outcome = if let Some(current) = current {
        if matches!(
            current.placement,
            IntegrationPlacement::Updating
                | IntegrationPlacement::Submitting
                | IntegrationPlacement::Submitted
        ) {
            return Err(
                "merge intent cannot be withdrawn after an integration mutation started"
                    .to_string(),
            );
        }
        let now = crate::auto_flow::unix_ms();
        if let Some(lane_key) = current.lane_key.as_deref() {
            let released = tx
                .execute(
                    "update integration_lane
                 set reserved_intent_id = null, updated_unix_ms = ?1
                 where lane_key = ?2 and reserved_intent_id = ?3",
                    params![u64_to_i64(now), lane_key, current.id],
                )
                .map_err(|error| format!("release withdrawn integration reservation: {error}"))?;
            if current.placement == IntegrationPlacement::Reserved && released != 1 {
                return Err(
                    "withdrawn intent no longer owned its integration reservation".to_string(),
                );
            }
            if released == 1 {
                reserve_and_wake_next(&tx, lane_key)?;
            }
        }
        let withdrawn = tx
            .execute(
                "update merge_intent
             set state = 'withdrawn', placement = 'withdrawn', updated_unix_ms = ?1
             where id = ?2 and state = 'armed'",
                params![u64_to_i64(now), current.id],
            )
            .map_err(|error| format!("withdraw merge intent: {error}"))?;
        if withdrawn != 1 {
            return Err("merge intent changed while it was being withdrawn".to_string());
        }
        MergeIntent {
            state: MergeIntentState::Withdrawn,
            placement: IntegrationPlacement::Withdrawn,
            ..current
        }
    } else {
        let latest = latest_merge_intent(&tx, run_id)?;
        let should_arm = latest.is_some() || !default_armed;
        let state = if should_arm {
            MergeIntentState::Armed
        } else {
            MergeIntentState::Withdrawn
        };
        let intent = insert_unbound_intent(&tx, run_id, next_generation(latest.as_ref()), state)?;
        if state == MergeIntentState::Armed {
            wake_toggled_auto_run(&tx, run_id)?;
        }
        intent
    };
    tx.commit()
        .map_err(|error| format!("commit merge intent toggle: {error}"))?;
    Ok(outcome)
}

#[cfg(test)]
pub(crate) fn withdraw_merge_intent(conn: &Connection, run_id: &str) -> Result<bool, String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("begin merge intent withdrawal: {error}"))?;
    let withdrawn = withdraw_merge_intent_in_transaction(&tx, run_id)?;
    tx.commit()
        .map_err(|error| format!("commit merge intent withdrawal: {error}"))?;
    Ok(withdrawn)
}

pub(crate) fn withdraw_merge_intent_in_transaction(
    conn: &Connection,
    run_id: &str,
) -> Result<bool, String> {
    let Some(current) = active_merge_intent(conn, run_id)? else {
        return Ok(false);
    };
    if matches!(
        current.placement,
        IntegrationPlacement::Updating
            | IntegrationPlacement::Submitting
            | IntegrationPlacement::Submitted
    ) {
        return Err(
            "merge intent cannot be withdrawn after an integration mutation started".to_string(),
        );
    }
    let now = crate::auto_flow::unix_ms();
    if let Some(lane_key) = current.lane_key.as_deref() {
        let released = conn
            .execute(
                "update integration_lane
                 set reserved_intent_id = null, updated_unix_ms = ?1
                 where lane_key = ?2 and reserved_intent_id = ?3",
                params![u64_to_i64(now), lane_key, current.id],
            )
            .map_err(|error| format!("release withdrawn integration reservation: {error}"))?;
        if current.placement == IntegrationPlacement::Reserved && released != 1 {
            return Err("withdrawn intent no longer owned its integration reservation".to_string());
        }
        if released == 1 {
            reserve_and_wake_next(conn, lane_key)?;
        }
    }
    let withdrawn = conn
        .execute(
            "update merge_intent
         set state = 'withdrawn', placement = 'withdrawn', updated_unix_ms = ?1
         where id = ?2 and state = 'armed'",
            params![u64_to_i64(now), current.id],
        )
        .map_err(|error| format!("withdraw merge intent: {error}"))?;
    if withdrawn != 1 {
        return Err("merge intent changed while it was being withdrawn".to_string());
    }
    Ok(true)
}

pub(crate) fn ensure_default_merge_intent(
    conn: &Connection,
    run_id: &str,
    default_armed: bool,
) -> Result<bool, String> {
    if let Some(intent) = latest_merge_intent(conn, run_id)? {
        return Ok(intent.state == MergeIntentState::Armed);
    }
    if !default_armed {
        return Ok(false);
    }
    insert_unbound_intent(conn, run_id, 1, MergeIntentState::Armed)?;
    Ok(true)
}

pub(crate) fn arm_merge_intent(conn: &Connection, run_id: &str) -> Result<MergeIntent, String> {
    if let Some(intent) = active_merge_intent(conn, run_id)? {
        return Ok(intent);
    }
    let latest = latest_merge_intent(conn, run_id)?;
    insert_unbound_intent(
        conn,
        run_id,
        next_generation(latest.as_ref()),
        MergeIntentState::Armed,
    )
}

pub(crate) fn merge_intent_enabled(
    conn: &Connection,
    run_id: &str,
    default_armed: bool,
) -> Result<bool, String> {
    Ok(latest_merge_intent(conn, run_id)?
        .map(|intent| intent.state == MergeIntentState::Armed)
        .unwrap_or(default_armed))
}

pub(crate) fn active_merge_intent(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<MergeIntent>, String> {
    load_merge_intent(
        conn,
        "select id, run_id, generation, state, placement, lane_key, head_sha, ready_sequence
         from merge_intent where run_id = ?1 and state = 'armed'",
        run_id,
    )
}

pub(crate) fn synchronize_generation(
    conn: &Connection,
    run_id: &str,
    candidate: &CandidateGeneration,
) -> Result<Option<MergeIntent>, String> {
    synchronize_generation_with_policy(conn, run_id, candidate, false)
}

pub(crate) fn synchronize_managed_generation(
    conn: &Connection,
    run_id: &str,
    candidate: &CandidateGeneration,
) -> Result<Option<MergeIntent>, String> {
    synchronize_generation_with_policy(conn, run_id, candidate, true)
}

fn synchronize_generation_with_policy(
    conn: &Connection,
    run_id: &str,
    candidate: &CandidateGeneration,
    preserve_reserved_position: bool,
) -> Result<Option<MergeIntent>, String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("begin merge generation synchronization: {error}"))?;
    let Some(current) = active_merge_intent(&tx, run_id)? else {
        tx.commit()
            .map_err(|error| format!("commit empty merge generation synchronization: {error}"))?;
        return Ok(None);
    };
    let identity_json = serde_json::to_string(&candidate.change_request_identity)
        .map_err(|error| format!("serialize merge intent identity: {error}"))?;
    let lane_key = lane_key(candidate)?;
    ensure_lane(&tx, &lane_key)?;
    let stored_identity = tx
        .query_row(
            "select change_request_identity_json from merge_intent where id = ?1",
            [current.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("read merge intent identity: {error}"))?;
    let unchanged = current.head_sha.as_deref() == Some(candidate.head_sha.as_str())
        && current.lane_key.as_deref() == Some(lane_key.as_str())
        && stored_identity.as_deref() == Some(identity_json.as_str());
    let synchronized = if unchanged {
        current
    } else if current.head_sha.is_none() && stored_identity.is_none() {
        update_intent_generation(&tx, current.id, candidate, &identity_json, &lane_key)?;
        active_merge_intent(&tx, run_id)?
            .ok_or_else(|| "merge intent disappeared while binding its generation".to_string())?
    } else {
        if matches!(
            current.placement,
            IntegrationPlacement::Submitting | IntegrationPlacement::Submitted
        ) {
            return Err("pull request head changed after provider submission".to_string());
        }
        let owned_reservation = matches!(
            current.placement,
            IntegrationPlacement::Reserved | IntegrationPlacement::Updating
        );
        let same_lane = current.lane_key.as_deref() == Some(lane_key.as_str());
        let preserve_reservation = owned_reservation
            && same_lane
            && (current.placement == IntegrationPlacement::Updating || preserve_reserved_position);
        if owned_reservation
            && !preserve_reservation
            && let Some(old_lane_key) = current.lane_key.as_deref()
        {
            let released = tx
                .execute(
                    "update integration_lane
                     set reserved_intent_id = null, updated_unix_ms = ?1
                     where lane_key = ?2 and reserved_intent_id = ?3",
                    params![
                        u64_to_i64(crate::auto_flow::unix_ms()),
                        old_lane_key,
                        current.id
                    ],
                )
                .map_err(|error| format!("release superseded integration reservation: {error}"))?;
            if released != 1 {
                return Err(
                    "superseded intent no longer owned its integration reservation".to_string(),
                );
            }
            reserve_and_wake_next(&tx, old_lane_key)?;
        }
        let superseded = tx
            .execute(
                "update merge_intent set state = 'superseded', updated_unix_ms = ?1
             where id = ?2 and state = 'armed'",
                params![u64_to_i64(crate::auto_flow::unix_ms()), current.id],
            )
            .map_err(|error| format!("supersede merge intent generation: {error}"))?;
        if superseded != 1 {
            return Err("merge intent changed while superseding its generation".to_string());
        }
        let next = insert_bound_intent(
            &tx,
            run_id,
            current.generation.saturating_add(1),
            candidate,
            &identity_json,
            &lane_key,
            preserve_reservation
                .then_some(current.ready_sequence)
                .flatten(),
            if preserve_reservation {
                IntegrationPlacement::Reserved
            } else {
                IntegrationPlacement::NotReady
            },
        )?;
        if preserve_reservation {
            let changed = tx
                .execute(
                    "update integration_lane set reserved_intent_id = ?1, updated_unix_ms = ?2
                 where lane_key = ?3 and reserved_intent_id = ?4",
                    params![
                        next.id,
                        u64_to_i64(crate::auto_flow::unix_ms()),
                        lane_key,
                        current.id
                    ],
                )
                .map_err(|error| format!("advance integration reservation generation: {error}"))?;
            if changed != 1 {
                return Err(
                    "integration reservation changed while advancing its generation".to_string(),
                );
            }
        }
        next
    };
    tx.commit()
        .map_err(|error| format!("commit merge generation synchronization: {error}"))?;
    Ok(Some(synchronized))
}

pub(crate) fn publish_ready(
    conn: &Connection,
    run_id: &str,
    expected_head_sha: &str,
) -> Result<IntegrationPlacement, String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("begin integration readiness publication: {error}"))?;
    let mut intent =
        active_merge_intent(&tx, run_id)?.ok_or_else(|| "merge intent is not armed".to_string())?;
    if intent.head_sha.as_deref() != Some(expected_head_sha) {
        return Err("merge intent head changed before readiness publication".to_string());
    }
    let lane_key = intent
        .lane_key
        .clone()
        .ok_or_else(|| "merge intent has no integration lane".to_string())?;
    if intent.ready_sequence.is_none() {
        let sequence = tx
            .query_row(
                "select next_ready_sequence from integration_lane where lane_key = ?1",
                [&lane_key],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("read integration ready sequence: {error}"))?;
        let published = tx
            .execute(
                "update merge_intent
             set ready_sequence = ?1, placement = 'ready', updated_unix_ms = ?2
             where id = ?3 and state = 'armed' and ready_sequence is null and head_sha = ?4",
                params![
                    sequence,
                    u64_to_i64(crate::auto_flow::unix_ms()),
                    intent.id,
                    expected_head_sha
                ],
            )
            .map_err(|error| format!("publish integration readiness: {error}"))?;
        if published != 1 {
            return Err("merge intent changed while publishing integration readiness".to_string());
        }
        tx.execute(
            "update integration_lane
             set next_ready_sequence = ?1, updated_unix_ms = ?2 where lane_key = ?3",
            params![
                sequence.saturating_add(1),
                u64_to_i64(crate::auto_flow::unix_ms()),
                lane_key
            ],
        )
        .map_err(|error| format!("advance integration ready sequence: {error}"))?;
        intent.ready_sequence = Some(i64_to_u64(sequence));
    }
    let reserved = reserved_intent_id(&tx, &lane_key)?;
    let selected = match reserved {
        Some(id) => id,
        None => reserve_next(&tx, &lane_key)?
            .ok_or_else(|| "ready integration lane had no selectable candidate".to_string())?,
    };
    let placement = if selected == intent.id {
        IntegrationPlacement::Reserved
    } else if intent.placement == IntegrationPlacement::Backlogged {
        IntegrationPlacement::Backlogged
    } else {
        let backlogged = tx
            .execute(
                "update merge_intent set placement = 'backlogged', updated_unix_ms = ?1
             where id = ?2 and state = 'armed' and placement = 'ready'",
                params![u64_to_i64(crate::auto_flow::unix_ms()), intent.id],
            )
            .map_err(|error| format!("backlog ready merge intent: {error}"))?;
        if backlogged != 1 {
            return Err("merge intent changed while entering the integration backlog".to_string());
        }
        IntegrationPlacement::Backlogged
    };
    tx.commit()
        .map_err(|error| format!("commit integration readiness publication: {error}"))?;
    Ok(placement)
}

pub(crate) fn mark_submitting(conn: &Connection, run_id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "update merge_intent set placement = 'submitting', updated_unix_ms = ?1
             where run_id = ?2 and state = 'armed' and placement = 'reserved'
               and exists (
                 select 1 from integration_lane lane
                 where lane.lane_key = merge_intent.lane_key
                   and lane.reserved_intent_id = merge_intent.id
               )",
            params![u64_to_i64(crate::auto_flow::unix_ms()), run_id],
        )
        .map_err(|error| format!("mark integration submission started: {error}"))?;
    if changed != 1 {
        return Err("merge intent no longer owns the integration reservation".to_string());
    }
    Ok(())
}

pub(crate) fn mark_submitted(conn: &Connection, run_id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "update merge_intent set placement = 'submitted', updated_unix_ms = ?1
             where run_id = ?2 and state = 'armed' and placement = 'submitting'
               and exists (
                 select 1 from integration_lane lane
                 where lane.lane_key = merge_intent.lane_key
                   and lane.reserved_intent_id = merge_intent.id
               )",
            params![u64_to_i64(crate::auto_flow::unix_ms()), run_id],
        )
        .map_err(|error| format!("mark integration submitted: {error}"))?;
    if changed != 1 {
        return Err(
            "merge intent no longer owns the submitted integration reservation".to_string(),
        );
    }
    Ok(())
}

pub(crate) fn retry_unobserved_submission(conn: &Connection, run_id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "update merge_intent set placement = 'reserved', updated_unix_ms = ?1
             where run_id = ?2 and state = 'armed' and placement = 'submitting'
               and exists (
                 select 1 from integration_lane lane
                 where lane.lane_key = merge_intent.lane_key
                   and lane.reserved_intent_id = merge_intent.id
               )",
            params![u64_to_i64(crate::auto_flow::unix_ms()), run_id],
        )
        .map_err(|error| format!("rearm unobserved integration submission: {error}"))?;
    if changed != 1 {
        return Err("integration submission changed before it could be retried".to_string());
    }
    Ok(())
}

pub(crate) fn mark_updating(conn: &Connection, run_id: &str) -> Result<(), String> {
    let changed = conn
        .execute(
            "update merge_intent set placement = 'updating', updated_unix_ms = ?1
             where run_id = ?2 and state = 'armed' and placement = 'reserved'
               and exists (
                 select 1 from integration_lane lane
                 where lane.lane_key = merge_intent.lane_key
                   and lane.reserved_intent_id = merge_intent.id
               )",
            params![u64_to_i64(crate::auto_flow::unix_ms()), run_id],
        )
        .map_err(|error| format!("mark reserved base update: {error}"))?;
    if changed != 1 {
        return Err("merge intent no longer owns the base-update reservation".to_string());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn complete_merge(conn: &Connection, run_id: &str) -> Result<Option<String>, String> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("begin integration completion: {error}"))?;
    let next = complete_merge_in_transaction(&tx, run_id)?;
    tx.commit()
        .map_err(|error| format!("commit integration completion: {error}"))?;
    Ok(next)
}

pub(crate) fn complete_merge_in_transaction(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<String>, String> {
    let intent = active_merge_intent(conn, run_id)?
        .ok_or_else(|| "completed merge has no armed intent".to_string())?;
    let lane_key = intent
        .lane_key
        .clone()
        .ok_or_else(|| "completed merge intent has no lane".to_string())?;
    let now = crate::auto_flow::unix_ms();
    let released = conn
        .execute(
            "update integration_lane set reserved_intent_id = null, updated_unix_ms = ?1
         where lane_key = ?2 and reserved_intent_id = ?3",
            params![u64_to_i64(now), lane_key, intent.id],
        )
        .map_err(|error| format!("release completed integration reservation: {error}"))?;
    if released != 1 {
        return Err("completed intent no longer owned its integration reservation".to_string());
    }
    let next = reserve_and_wake_next(conn, &lane_key)?;
    let completed = conn
        .execute(
            "update merge_intent set state = 'merged', placement = 'merged', updated_unix_ms = ?1
         where id = ?2 and state = 'armed'
           and placement in ('reserved', 'submitting', 'submitted')",
            params![u64_to_i64(now), intent.id],
        )
        .map_err(|error| format!("complete merge intent: {error}"))?;
    if completed != 1 {
        return Err("merge intent changed before integration completion".to_string());
    }
    Ok(next)
}

pub(crate) fn release_failed_reservation_in_transaction(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<String>, String> {
    let Some(intent) = active_merge_intent(conn, run_id)? else {
        return Ok(None);
    };
    if !matches!(
        intent.placement,
        IntegrationPlacement::Reserved | IntegrationPlacement::Updating
    ) {
        return Ok(None);
    }
    release_intent_as_failed(conn, intent)
}

pub(crate) fn release_submitted_reservation_in_transaction(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<String>, String> {
    let Some(intent) = active_merge_intent(conn, run_id)? else {
        return Ok(None);
    };
    if !matches!(
        intent.placement,
        IntegrationPlacement::Submitting | IntegrationPlacement::Submitted
    ) {
        return Ok(None);
    }
    release_intent_as_failed(conn, intent)
}

fn release_intent_as_failed(
    conn: &Connection,
    intent: MergeIntent,
) -> Result<Option<String>, String> {
    let lane_key = intent
        .lane_key
        .clone()
        .ok_or_else(|| "failed integration intent has no lane".to_string())?;
    let now = crate::auto_flow::unix_ms();
    let released = conn
        .execute(
            "update integration_lane set reserved_intent_id = null, updated_unix_ms = ?1
         where lane_key = ?2 and reserved_intent_id = ?3",
            params![u64_to_i64(now), lane_key, intent.id],
        )
        .map_err(|error| format!("release failed integration reservation: {error}"))?;
    if released != 1 {
        return Err("failed intent no longer owned its integration reservation".to_string());
    }
    let next = reserve_and_wake_next(conn, &lane_key)?;
    let failed = conn
        .execute(
            "update merge_intent set state = 'failed', placement = 'failed', updated_unix_ms = ?1
         where id = ?2 and state = 'armed' and placement = ?3",
            params![u64_to_i64(now), intent.id, intent.placement.as_str()],
        )
        .map_err(|error| format!("fail integration intent: {error}"))?;
    if failed != 1 {
        return Err("merge intent changed before failed integration release".to_string());
    }
    Ok(next)
}

fn latest_merge_intent(conn: &Connection, run_id: &str) -> Result<Option<MergeIntent>, String> {
    load_merge_intent(
        conn,
        "select id, run_id, generation, state, placement, lane_key, head_sha, ready_sequence
         from merge_intent where run_id = ?1 order by generation desc limit 1",
        run_id,
    )
}

fn load_merge_intent(
    conn: &Connection,
    sql: &str,
    run_id: &str,
) -> Result<Option<MergeIntent>, String> {
    conn.query_row(sql, [run_id], |row| {
        let state: String = row.get(3)?;
        let placement: String = row.get(4)?;
        Ok(MergeIntent {
            id: row.get(0)?,
            run_id: row.get(1)?,
            generation: i64_to_u64(row.get(2)?),
            state: MergeIntentState::parse(&state).map_err(sql_string_error)?,
            placement: IntegrationPlacement::parse(&placement).map_err(sql_string_error)?,
            lane_key: row.get(5)?,
            head_sha: row.get(6)?,
            ready_sequence: row.get::<_, Option<i64>>(7)?.map(i64_to_u64),
        })
    })
    .optional()
    .map_err(|error| format!("load merge intent: {error}"))
}

fn insert_unbound_intent(
    conn: &Connection,
    run_id: &str,
    generation: u64,
    state: MergeIntentState,
) -> Result<MergeIntent, String> {
    let now = crate::auto_flow::unix_ms();
    let placement = if state == MergeIntentState::Armed {
        IntegrationPlacement::Pending
    } else {
        IntegrationPlacement::Withdrawn
    };
    conn.execute(
        "insert into merge_intent (
           run_id, generation, state, placement, created_unix_ms, updated_unix_ms
         ) values (?1, ?2, ?3, ?4, ?5, ?5)",
        params![
            run_id,
            u64_to_i64(generation),
            state.as_str(),
            placement.as_str(),
            u64_to_i64(now)
        ],
    )
    .map_err(|error| format!("insert merge intent: {error}"))?;
    Ok(MergeIntent {
        id: conn.last_insert_rowid(),
        run_id: run_id.to_string(),
        generation,
        state,
        placement,
        lane_key: None,
        head_sha: None,
        ready_sequence: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn insert_bound_intent(
    conn: &Connection,
    run_id: &str,
    generation: u64,
    candidate: &CandidateGeneration,
    identity_json: &str,
    lane_key: &str,
    ready_sequence: Option<u64>,
    placement: IntegrationPlacement,
) -> Result<MergeIntent, String> {
    let now = crate::auto_flow::unix_ms();
    conn.execute(
        "insert into merge_intent (
           run_id, generation, state, placement, change_request_identity_json,
           lane_key, target_branch, pr_number, head_sha, ready_sequence,
           created_unix_ms, updated_unix_ms
         ) values (?1, ?2, 'armed', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
        params![
            run_id,
            u64_to_i64(generation),
            placement.as_str(),
            identity_json,
            lane_key,
            candidate.target_branch,
            u64_to_i64(candidate.pr_number),
            candidate.head_sha,
            ready_sequence.map(u64_to_i64),
            u64_to_i64(now)
        ],
    )
    .map_err(|error| format!("insert bound merge intent: {error}"))?;
    Ok(MergeIntent {
        id: conn.last_insert_rowid(),
        run_id: run_id.to_string(),
        generation,
        state: MergeIntentState::Armed,
        placement,
        lane_key: Some(lane_key.to_string()),
        head_sha: Some(candidate.head_sha.clone()),
        ready_sequence,
    })
}

fn update_intent_generation(
    conn: &Connection,
    intent_id: i64,
    candidate: &CandidateGeneration,
    identity_json: &str,
    lane_key: &str,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "update merge_intent
         set placement = 'not_ready', change_request_identity_json = ?1,
             lane_key = ?2, target_branch = ?3, pr_number = ?4, head_sha = ?5,
             ready_sequence = null, updated_unix_ms = ?6
         where id = ?7 and state = 'armed'",
            params![
                identity_json,
                lane_key,
                candidate.target_branch,
                u64_to_i64(candidate.pr_number),
                candidate.head_sha,
                u64_to_i64(crate::auto_flow::unix_ms()),
                intent_id
            ],
        )
        .map_err(|error| format!("bind merge intent generation: {error}"))?;
    if changed != 1 {
        return Err("merge intent changed while binding its generation".to_string());
    }
    Ok(())
}

fn ensure_lane(conn: &Connection, lane_key: &str) -> Result<(), String> {
    conn.execute(
        "insert into integration_lane (lane_key, next_ready_sequence, updated_unix_ms)
         values (?1, 1, ?2) on conflict(lane_key) do nothing",
        params![lane_key, u64_to_i64(crate::auto_flow::unix_ms())],
    )
    .map_err(|error| format!("ensure integration lane: {error}"))?;
    Ok(())
}

fn reserved_intent_id(conn: &Connection, lane_key: &str) -> Result<Option<i64>, String> {
    conn.query_row(
        "select reserved_intent_id from integration_lane where lane_key = ?1",
        [lane_key],
        |row| row.get(0),
    )
    .map_err(|error| format!("read integration reservation: {error}"))
}

fn reserve_next(conn: &Connection, lane_key: &str) -> Result<Option<i64>, String> {
    let next = conn
        .query_row(
            "select id from merge_intent
             where lane_key = ?1 and state = 'armed'
               and placement in ('ready', 'backlogged') and ready_sequence is not null
             order by ready_sequence, id limit 1",
            [lane_key],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| format!("select next integration candidate: {error}"))?;
    let Some(next) = next else {
        return Ok(None);
    };
    let now = crate::auto_flow::unix_ms();
    let reserved = conn
        .execute(
            "update integration_lane set reserved_intent_id = ?1, updated_unix_ms = ?2
         where lane_key = ?3 and reserved_intent_id is null",
            params![next, u64_to_i64(now), lane_key],
        )
        .map_err(|error| format!("reserve integration candidate: {error}"))?;
    if reserved == 0 {
        return reserved_intent_id(conn, lane_key);
    }
    let placed = conn
        .execute(
            "update merge_intent set placement = 'reserved', updated_unix_ms = ?1
         where id = ?2 and state = 'armed'
           and placement in ('ready', 'backlogged')",
            params![u64_to_i64(now), next],
        )
        .map_err(|error| format!("mark integration candidate reserved: {error}"))?;
    if placed != 1 {
        return Err("selected integration candidate disappeared before reservation".to_string());
    }
    Ok(Some(next))
}

fn reserve_and_wake_next(conn: &Connection, lane_key: &str) -> Result<Option<String>, String> {
    let Some(intent_id) = reserve_next(conn, lane_key)? else {
        return Ok(None);
    };
    let run_id = conn
        .query_row(
            "select run_id from merge_intent where id = ?1",
            [intent_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("load reserved integration run: {error}"))?;
    enqueue_auto_run(conn, &run_id)?;
    Ok(Some(run_id))
}

fn wake_toggled_auto_run(conn: &Connection, run_id: &str) -> Result<(), String> {
    let (status, pause_requested) = conn
        .query_row(
            "select status, pause_requested from auto_run where id = ?1",
            [run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
        )
        .optional()
        .map_err(|error| format!("inspect merge-intent Auto Flow: {error}"))?
        .ok_or_else(|| "merge intent cannot wake a missing Auto Flow run".to_string())?;
    if status == "aborted" {
        return Err("merge intent cannot wake an aborted Auto Flow run".to_string());
    }
    if pause_requested {
        return Ok(());
    }
    let changed = conn
        .execute(
            "update auto_run
             set status = case when status = 'running' then 'running' else 'queued' end,
                 updated_unix_ms = ?1
             where id = ?2 and status != 'aborted' and pause_requested = 0",
            params![u64_to_i64(crate::auto_flow::unix_ms()), run_id],
        )
        .map_err(|error| format!("wake merge-intent Auto Flow: {error}"))?;
    if changed != 1 {
        return Err("merge intent cannot wake the Auto Flow run".to_string());
    }
    enqueue_auto_run(conn, run_id)
}

fn enqueue_auto_run(conn: &Connection, run_id: &str) -> Result<(), String> {
    crate::execution::enqueue(
        conn,
        &crate::execution::WorkflowIdentity::new(crate::execution::WorkflowKind::Auto, run_id),
    )
}

fn lane_key(candidate: &CandidateGeneration) -> Result<String, String> {
    let target = candidate
        .change_request_identity
        .target_repository()
        .map_err(|error| error.to_string())?;
    let project = if target.provider() == crate::remote::ProviderKind::GitHub {
        target.project_path().to_ascii_lowercase()
    } else {
        target.project_path().to_string()
    };
    Ok(format!(
        "{}|{}|{}|{}",
        target.provider(),
        target.host().to_string().to_ascii_lowercase(),
        project,
        candidate.target_branch
    ))
}

fn next_generation(intent: Option<&MergeIntent>) -> u64 {
    intent.map_or(1, |intent| intent.generation.saturating_add(1))
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn sql_string_error(error: String) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.into())
}

impl MergeIntentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Withdrawn => "withdrawn",
            Self::Superseded => "superseded",
            Self::Merged => "merged",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "armed" => Ok(Self::Armed),
            "withdrawn" => Ok(Self::Withdrawn),
            "superseded" => Ok(Self::Superseded),
            "merged" => Ok(Self::Merged),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("unknown merge intent state: {value}")),
        }
    }
}

impl IntegrationPlacement {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::NotReady => "not_ready",
            Self::Ready => "ready",
            Self::Backlogged => "backlogged",
            Self::Reserved => "reserved",
            Self::Updating => "updating",
            Self::Submitting => "submitting",
            Self::Submitted => "submitted",
            Self::Withdrawn => "withdrawn",
            Self::Merged => "merged",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "not_ready" => Ok(Self::NotReady),
            "ready" => Ok(Self::Ready),
            "backlogged" => Ok(Self::Backlogged),
            "reserved" => Ok(Self::Reserved),
            "updating" => Ok(Self::Updating),
            "submitting" => Ok(Self::Submitting),
            "submitted" => Ok(Self::Submitted),
            "withdrawn" => Ok(Self::Withdrawn),
            "merged" => Ok(Self::Merged),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("unknown integration placement: {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rusqlite::Connection;

    use super::*;

    fn saved_run(conn: &Connection, branch: &str) -> crate::auto_flow::PersistedAutoRun {
        let mut run = crate::auto_flow::AutoLaunch::new(
            Path::new("/repo"),
            &Path::new("/repo").join(branch),
            branch,
            "stabilize pull request",
        )
        .unwrap()
        .create_run();
        crate::auto_flow::save_auto_run(conn, &mut run).unwrap();
        run
    }

    fn generation(head_sha: &str) -> CandidateGeneration {
        CandidateGeneration {
            change_request_identity: crate::remote::test_change_request_identity(),
            target_branch: "main".to_string(),
            pr_number: 42,
            head_sha: head_sha.to_string(),
        }
    }

    #[test]
    fn merge_intent_toggle_is_durable() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let run = saved_run(&conn, "feature");

        let armed = toggle_merge_intent(&conn, &run.run.id, false).unwrap();
        let persisted = active_merge_intent(&conn, &run.run.id).unwrap().unwrap();
        let withdrawn = toggle_merge_intent(&conn, &run.run.id, false).unwrap();

        assert_eq!(armed.state, MergeIntentState::Armed);
        assert_eq!(persisted.state, MergeIntentState::Armed);
        assert_eq!(persisted.placement, IntegrationPlacement::Pending);
        assert_eq!(withdrawn.state, MergeIntentState::Withdrawn);
        assert!(active_merge_intent(&conn, &run.run.id).unwrap().is_none());
    }

    #[test]
    fn arming_merge_intent_preserves_explicit_auto_run_pause() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let mut run = saved_run(&conn, "feature");
        run.run.status = crate::auto_flow::AutoRunStatus::Paused;
        run.run.pause_requested = true;
        crate::auto_flow::save_auto_run(&conn, &mut run).unwrap();

        let intent = toggle_merge_intent(&conn, &run.run.id, false).unwrap();

        let loaded = crate::auto_flow::load_auto_run(&conn, &run.run.id)
            .unwrap()
            .unwrap();
        assert_eq!(intent.state, MergeIntentState::Armed);
        assert_eq!(loaded.run.status, crate::auto_flow::AutoRunStatus::Paused);
        assert!(loaded.run.pause_requested);
        assert_eq!(
            crate::execution::dispatch_state(
                &conn,
                &crate::execution::WorkflowIdentity::new(
                    crate::execution::WorkflowKind::Auto,
                    &run.run.id,
                ),
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn ready_generations_are_reserved_in_fifo_order() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let first = saved_run(&conn, "first");
        let second = saved_run(&conn, "second");
        toggle_merge_intent(&conn, &first.run.id, false).unwrap();
        toggle_merge_intent(&conn, &second.run.id, false).unwrap();
        synchronize_generation(&conn, &first.run.id, &generation("first-head")).unwrap();
        synchronize_generation(&conn, &second.run.id, &generation("second-head")).unwrap();

        let first_ready = publish_ready(&conn, &first.run.id, "first-head").unwrap();
        let second_ready = publish_ready(&conn, &second.run.id, "second-head").unwrap();

        assert_eq!(first_ready, IntegrationPlacement::Reserved);
        assert_eq!(second_ready, IntegrationPlacement::Backlogged);
        assert_eq!(
            active_merge_intent(&conn, &first.run.id)
                .unwrap()
                .unwrap()
                .ready_sequence,
            Some(1)
        );
        assert_eq!(
            active_merge_intent(&conn, &second.run.id)
                .unwrap()
                .unwrap()
                .ready_sequence,
            Some(2)
        );
        assert_eq!(
            publish_ready(&conn, &second.run.id, "second-head").unwrap(),
            IntegrationPlacement::Backlogged
        );
    }

    #[test]
    fn completing_reserved_generation_selects_next_candidate() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let first = saved_run(&conn, "first");
        let second = saved_run(&conn, "second");
        for (run, head) in [(&first, "first-head"), (&second, "second-head")] {
            toggle_merge_intent(&conn, &run.run.id, false).unwrap();
            synchronize_generation(&conn, &run.run.id, &generation(head)).unwrap();
            publish_ready(&conn, &run.run.id, head).unwrap();
        }

        let next = complete_merge(&conn, &first.run.id).unwrap();

        assert_eq!(next.as_deref(), Some(second.run.id.as_str()));
        assert_eq!(
            active_merge_intent(&conn, &second.run.id)
                .unwrap()
                .unwrap()
                .placement,
            IntegrationPlacement::Reserved
        );
        assert_eq!(
            crate::execution::dispatch_state(
                &conn,
                &crate::execution::WorkflowIdentity::new(
                    crate::execution::WorkflowKind::Auto,
                    &second.run.id,
                ),
            )
            .unwrap(),
            Some(crate::execution::DispatchState::Queued)
        );
    }

    #[test]
    fn external_head_change_releases_the_lane_to_the_next_candidate() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let first = saved_run(&conn, "first");
        let second = saved_run(&conn, "second");
        for (run, head) in [(&first, "first-head"), (&second, "second-head")] {
            toggle_merge_intent(&conn, &run.run.id, false).unwrap();
            synchronize_generation(&conn, &run.run.id, &generation(head)).unwrap();
            publish_ready(&conn, &run.run.id, head).unwrap();
        }

        synchronize_generation(&conn, &first.run.id, &generation("external-head")).unwrap();

        assert_eq!(
            active_merge_intent(&conn, &first.run.id)
                .unwrap()
                .unwrap()
                .placement,
            IntegrationPlacement::NotReady
        );
        assert_eq!(
            active_merge_intent(&conn, &second.run.id)
                .unwrap()
                .unwrap()
                .placement,
            IntegrationPlacement::Reserved
        );
    }

    #[test]
    fn managed_head_change_preserves_the_owned_lane_position() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let first = saved_run(&conn, "first");
        let second = saved_run(&conn, "second");
        for (run, head) in [(&first, "first-head"), (&second, "second-head")] {
            toggle_merge_intent(&conn, &run.run.id, false).unwrap();
            synchronize_generation(&conn, &run.run.id, &generation(head)).unwrap();
            publish_ready(&conn, &run.run.id, head).unwrap();
        }

        synchronize_managed_generation(&conn, &first.run.id, &generation("managed-head")).unwrap();

        let first_intent = active_merge_intent(&conn, &first.run.id).unwrap().unwrap();
        assert_eq!(first_intent.placement, IntegrationPlacement::Reserved);
        assert_eq!(first_intent.ready_sequence, Some(1));
        assert_eq!(
            active_merge_intent(&conn, &second.run.id)
                .unwrap()
                .unwrap()
                .placement,
            IntegrationPlacement::Backlogged
        );
    }

    #[test]
    fn withdrawing_a_reservation_selects_the_next_candidate() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let first = saved_run(&conn, "first");
        let second = saved_run(&conn, "second");
        for (run, head) in [(&first, "first-head"), (&second, "second-head")] {
            toggle_merge_intent(&conn, &run.run.id, false).unwrap();
            synchronize_generation(&conn, &run.run.id, &generation(head)).unwrap();
            publish_ready(&conn, &run.run.id, head).unwrap();
        }

        assert!(withdraw_merge_intent(&conn, &first.run.id).unwrap());

        assert!(active_merge_intent(&conn, &first.run.id).unwrap().is_none());
        assert_eq!(
            active_merge_intent(&conn, &second.run.id)
                .unwrap()
                .unwrap()
                .placement,
            IntegrationPlacement::Reserved
        );
    }

    #[test]
    fn submission_requires_current_lane_ownership() {
        let conn = Connection::open_in_memory().unwrap();
        crate::auto_flow::migrate_schema(&conn).unwrap();
        crate::plan_run::migrate_schema(&conn).unwrap();
        crate::execution::migrate_schema(&conn).unwrap();
        let run = saved_run(&conn, "feature");
        toggle_merge_intent(&conn, &run.run.id, false).unwrap();
        synchronize_generation(&conn, &run.run.id, &generation("head")).unwrap();
        publish_ready(&conn, &run.run.id, "head").unwrap();
        conn.execute("update integration_lane set reserved_intent_id = null", [])
            .unwrap();

        assert_eq!(
            mark_submitting(&conn, &run.run.id).unwrap_err(),
            "merge intent no longer owns the integration reservation"
        );
    }
}
