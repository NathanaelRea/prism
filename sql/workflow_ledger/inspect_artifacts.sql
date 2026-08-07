select
  id as "id!: String",
  producing_attempt_id,
  revision as "revision!: i64",
  digest as "digest!: String",
  size_bytes as "size_bytes!: i64",
  sensitivity as "sensitivity!: String",
  inline_body,
  file_path,
  artifact.created_unix_ms as "created_unix_ms!: i64",
  provenance.provider_item_id,
  provenance.observation_revision,
  provenance.trigger_occurrence_id,
  provenance.admission_decision_id
from artifact
left join artifact_provenance provenance on provenance.artifact_id=artifact.id
where artifact.run_id = ?
order by artifact.id, artifact.revision
