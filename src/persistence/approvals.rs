use super::error::DatabaseError;
use super::pools::WorkflowDatabase;

#[derive(Clone)]
pub(crate) struct ApprovalStore {
    database: WorkflowDatabase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Approve,
    Reject,
}

pub(crate) struct EvidenceRequest<'a> {
    pub id: &'a str,
    pub run_id: &'a str,
    pub step_id: &'a str,
    pub subject_json: &'a str,
    pub evidence_json: &'a str,
    pub policy_json: &'a str,
    pub now_unix_ms: i64,
}

impl ApprovalStore {
    pub(crate) fn new(database: WorkflowDatabase) -> Self {
        Self { database }
    }

    pub(crate) async fn request(
        &self,
        id: &str,
        run_id: &str,
        step_id: &str,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        self.request_evidence(EvidenceRequest {
            id,
            run_id,
            step_id,
            subject_json: "{}",
            evidence_json: "{}",
            policy_json: "{}",
            now_unix_ms,
        })
        .await
    }

    pub(crate) async fn request_evidence(
        &self,
        request: EvidenceRequest<'_>,
    ) -> Result<(), DatabaseError> {
        let id = request.id.to_string();
        let run_id = request.run_id.to_string();
        let step_id = request.step_id.to_string();
        let subject_json = request.subject_json.to_string();
        let evidence_json = request.evidence_json.to_string();
        let policy_json = request.policy_json.to_string();
        let now_unix_ms = request.now_unix_ms;
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let changed = sqlx::query(
                        "update workflow_step set status = 'waiting', runtime_status = 'waiting_approval' where id = ? and run_id = ? and status = 'runnable'",
                    )
                    .bind(&step_id)
                    .bind(&run_id)
                    .execute(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?
                    .rows_affected();
                    if changed != 1 {
                        return Err(DatabaseError::Conflict {
                            operation: "request approval",
                        });
                    }
                    sqlx::query("update workflow_run set status = 'waiting', runtime_status = 'waiting', updated_unix_ms = ? where id = ? and status = 'runnable' and not exists (select 1 from workflow_step where run_id = ? and status in ('runnable','claimed'))")
                        .bind(now_unix_ms)
                        .bind(&run_id)
                        .bind(&run_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("insert into approval_request (id, run_id, step_id, status, requested_unix_ms) values (?, ?, ?, 'pending', ?)")
                        .bind(&id)
                        .bind(&run_id)
                        .bind(&step_id)
                        .bind(now_unix_ms)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?;
                    sqlx::query("insert into approval_evidence (approval_id, subject_json, evidence_json, policy_json) values (?, ?, ?, ?)")
                        .bind(&id).bind(subject_json).bind(evidence_json).bind(policy_json)
                        .execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    append_run_event(
                        connection,
                        &run_id,
                        "approval_requested",
                        &serde_json::json!({"approval_id": id, "step_id": step_id}).to_string(),
                        now_unix_ms,
                    )
                    .await
                })
            })
            .await
    }

    pub(crate) async fn decide(
        &self,
        id: &str,
        decision: ApprovalDecision,
        decided_by: &str,
        note: Option<&str>,
        now_unix_ms: i64,
    ) -> Result<(), DatabaseError> {
        let id = id.to_string();
        let decided_by = decided_by.to_string();
        let note = note.map(str::to_string);
        self.database
            .write_immediate(|connection| {
                Box::pin(async move {
                    let status = match decision {
                        ApprovalDecision::Approve => "approved",
                        ApprovalDecision::Reject => "rejected",
                    };
                    let row: Option<(String, Option<String>)> = sqlx::query_as(
                        "update approval_request set status = ?, decided_unix_ms = ?, decided_by = ?, decision_note = ? where id = ? and status = 'pending' returning run_id, step_id",
                    )
                    .bind(status)
                    .bind(now_unix_ms)
                    .bind(&decided_by)
                    .bind(&note)
                    .bind(&id)
                    .fetch_optional(&mut *connection)
                    .await
                    .map_err(DatabaseError::Query)?;
                    let Some((run_id, step_id)) = row else {
                        return Err(DatabaseError::Conflict {
                            operation: "decide approval",
                        });
                    };
                    if let Some(step_id) = step_id {
                        let class: String = sqlx::query_scalar("select class from workflow_step where id = ?")
                            .bind(&step_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                        let next = match decision {
                            ApprovalDecision::Approve
                                if matches!(class.as_str(), "approval" | "workflow_call") =>
                            {
                                "succeeded"
                            }
                            ApprovalDecision::Approve => "runnable",
                            ApprovalDecision::Reject => "failed",
                        };
                        let changed = sqlx::query(
                            "update workflow_step set status = ? where id = ? and run_id = ? and status = 'waiting'",
                        )
                        .bind(next)
                        .bind(&step_id)
                        .bind(&run_id)
                        .execute(&mut *connection)
                        .await
                        .map_err(DatabaseError::Query)?
                        .rows_affected();
                        if changed != 1 {
                            return Err(DatabaseError::Conflict {
                                operation: "apply approval decision",
                            });
                        }
                        sqlx::query("update workflow_step set runtime_status = ? where id = ?")
                            .bind(next).bind(&step_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                    }
                    match decision {
                        ApprovalDecision::Approve => {
                            sqlx::query("update workflow_run set status = 'runnable', updated_unix_ms = ? where id = ? and status = 'waiting'")
                                .bind(now_unix_ms)
                                .bind(&run_id)
                                .execute(&mut *connection)
                                .await
                                .map_err(DatabaseError::Query)?;
                            let unfinished: i64 = sqlx::query_scalar("select count(*) from workflow_step where run_id = ? and status <> 'succeeded'")
                                .bind(&run_id).fetch_one(&mut *connection).await.map_err(DatabaseError::Query)?;
                            if unfinished == 0 {
                                sqlx::query("update workflow_run set status = 'succeeded', runtime_status = 'succeeded', completed_unix_ms = ?, updated_unix_ms = ? where id = ?")
                                    .bind(now_unix_ms).bind(now_unix_ms).bind(&run_id).execute(&mut *connection).await.map_err(DatabaseError::Query)?;
                            }
                        }
                        ApprovalDecision::Reject => {
                            sqlx::query("update workflow_run set status = 'failed', updated_unix_ms = ?, completed_unix_ms = ? where id = ? and status in ('waiting','runnable','paused')")
                                .bind(now_unix_ms)
                                .bind(now_unix_ms)
                                .bind(&run_id)
                                .execute(&mut *connection)
                                .await
                                .map_err(DatabaseError::Query)?;
                        }
                    }
                    append_run_event(
                        connection,
                        &run_id,
                        match decision {
                            ApprovalDecision::Approve => "approval_approved",
                            ApprovalDecision::Reject => "approval_rejected",
                        },
                        &serde_json::json!({"approval_id": id, "decided_by": decided_by, "note": note}).to_string(),
                        now_unix_ms,
                    )
                    .await
                })
            })
            .await
    }
}

async fn append_run_event(
    connection: &mut sqlx::SqliteConnection,
    run_id: &str,
    kind: &str,
    data_json: &str,
    now_unix_ms: i64,
) -> Result<(), DatabaseError> {
    sqlx::query("insert into audit_event (run_id, sequence, kind, time_unix_ms, data_json) select ?, coalesce(max(sequence), 0) + 1, ?, ?, ? from audit_event where run_id = ?")
        .bind(run_id)
        .bind(kind)
        .bind(now_unix_ms)
        .bind(data_json)
        .bind(run_id)
        .execute(connection)
        .await
        .map_err(DatabaseError::Query)?;
    Ok(())
}
