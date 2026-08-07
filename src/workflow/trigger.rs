//! Trigger timing and quarantined provider-intake contracts.
//!
//! This module deliberately contains no provider client or database details. Scheduling is a
//! deterministic calculation, while authority is selected only from trusted policy/decisions;
//! externally controlled provider text is evidence and is never interpreted as configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use chrono::{TimeZone as _, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapPolicy {
    Coalesce,
    Supersede,
    #[default]
    Queue,
    Parallel,
}

impl OverlapPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Coalesce => "coalesce",
            Self::Supersede => "supersede",
            Self::Queue => "queue",
            Self::Parallel => "parallel",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "coalesce" => Some(Self::Coalesce),
            "supersede" => Some(Self::Supersede),
            "queue" | "serialize" => Some(Self::Queue),
            "parallel" | "allow" => Some(Self::Parallel),
            _ => None,
        }
    }

    pub(crate) fn replacement_status(self) -> Option<TriggerOccurrenceStatus> {
        match self {
            Self::Coalesce => Some(TriggerOccurrenceStatus::Coalesced),
            Self::Supersede => Some(TriggerOccurrenceStatus::Superseded),
            Self::Queue | Self::Parallel => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerOccurrenceStatus {
    Pending,
    Fired,
    Coalesced,
    Superseded,
    Failed,
}

impl TriggerOccurrenceStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fired => "fired",
            Self::Coalesced => "coalesced",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "fired" => Some(Self::Fired),
            "coalesced" => Some(Self::Coalesced),
            "superseded" => Some(Self::Superseded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSchedule {
    Manual,
    Once {
        at_unix_ms: i64,
    },
    Interval {
        anchor_unix_ms: i64,
        every_ms: u64,
    },
    Cron {
        expression: String,
        timezone: String,
    },
    Startup,
    ProviderPoll {
        anchor_unix_ms: i64,
        every_ms: u64,
        item_kind: ProviderItemKind,
    },
}

impl TriggerSchedule {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Once { .. } => "once",
            Self::Interval { .. } => "interval",
            Self::Cron { .. } => "cron",
            Self::Startup => "startup",
            Self::ProviderPoll { .. } => "provider_poll",
        }
    }

    pub fn validate(&self) -> Result<(), TriggerContractError> {
        match self {
            Self::Interval { every_ms, .. } | Self::ProviderPoll { every_ms, .. }
                if *every_ms == 0 =>
            {
                Err(TriggerContractError::InvalidSchedule(
                    "interval must be greater than zero".into(),
                ))
            }
            Self::Cron {
                expression,
                timezone,
            } => {
                parse_cron(expression)?;
                timezone.parse::<Tz>().map_err(|_| {
                    TriggerContractError::InvalidSchedule(format!(
                        "unknown IANA timezone '{timezone}'"
                    ))
                })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Returns due instants strictly after `checkpoint_unix_ms` and no later than `now_unix_ms`.
    /// A bounded result lets a restarted worker catch up without monopolizing the scheduler.
    pub fn due_between(
        &self,
        checkpoint_unix_ms: i64,
        now_unix_ms: i64,
        limit: usize,
    ) -> Result<Vec<i64>, TriggerContractError> {
        self.validate()?;
        if checkpoint_unix_ms >= now_unix_ms || limit == 0 {
            return Ok(Vec::new());
        }
        match self {
            Self::Manual | Self::Startup => Ok(Vec::new()),
            Self::Once { at_unix_ms } => Ok((checkpoint_unix_ms < *at_unix_ms
                && *at_unix_ms <= now_unix_ms)
                .then_some(*at_unix_ms)
                .into_iter()
                .collect()),
            Self::Interval {
                anchor_unix_ms,
                every_ms,
            }
            | Self::ProviderPoll {
                anchor_unix_ms,
                every_ms,
                ..
            } => interval_due(
                *anchor_unix_ms,
                *every_ms,
                checkpoint_unix_ms,
                now_unix_ms,
                limit,
            ),
            Self::Cron {
                expression,
                timezone,
            } => cron_due(expression, timezone, checkpoint_unix_ms, now_unix_ms, limit),
        }
    }
}

fn interval_due(
    anchor: i64,
    every_ms: u64,
    checkpoint: i64,
    now: i64,
    limit: usize,
) -> Result<Vec<i64>, TriggerContractError> {
    let interval = i64::try_from(every_ms).map_err(|_| {
        TriggerContractError::InvalidSchedule("interval exceeds signed timestamp range".into())
    })?;
    let elapsed = checkpoint.saturating_sub(anchor);
    let periods = if elapsed < 0 {
        0
    } else {
        elapsed / interval + 1
    };
    let mut due = anchor.saturating_add(periods.saturating_mul(interval));
    let mut result = Vec::new();
    while due <= now && result.len() < limit {
        if due > checkpoint {
            result.push(due);
        }
        let next = due.saturating_add(interval);
        if next <= due {
            break;
        }
        due = next;
    }
    Ok(result)
}

fn parse_cron(expression: &str) -> Result<Schedule, TriggerContractError> {
    let fields = expression.split_whitespace().count();
    let normalized = match fields {
        5 => format!("0 {expression}"),
        6 | 7 => expression.to_string(),
        _ => {
            return Err(TriggerContractError::InvalidSchedule(
                "cron expression must contain five, six, or seven fields".into(),
            ));
        }
    };
    Schedule::from_str(&normalized)
        .map_err(|error| TriggerContractError::InvalidSchedule(error.to_string()))
}

fn cron_due(
    expression: &str,
    timezone: &str,
    checkpoint: i64,
    now: i64,
    limit: usize,
) -> Result<Vec<i64>, TriggerContractError> {
    let schedule = parse_cron(expression)?;
    let timezone = timezone.parse::<Tz>().map_err(|_| {
        TriggerContractError::InvalidSchedule(format!("unknown IANA timezone '{timezone}'"))
    })?;
    let checkpoint = Utc
        .timestamp_millis_opt(checkpoint)
        .single()
        .ok_or_else(|| {
            TriggerContractError::InvalidSchedule(
                "cron checkpoint is outside timestamp range".into(),
            )
        })?;
    Ok(schedule
        .after(&checkpoint.with_timezone(&timezone))
        .take(limit)
        .map(|instant| instant.timestamp_millis())
        .take_while(|instant| *instant <= now)
        .collect())
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TriggerRegistration {
    pub id: String,
    pub definition_snapshot_id: String,
    pub schedule: TriggerSchedule,
    #[serde(default)]
    pub overlap_policy: OverlapPolicy,
    #[serde(default = "default_admission_purpose")]
    pub admission_purpose: String,
    #[serde(default)]
    pub inputs: serde_json::Value,
    pub repository: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_admission_purpose() -> String {
    "workflow-launch".into()
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderItemKind {
    Issue,
    ChangeRequest,
}

impl ProviderItemKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::ChangeRequest => "change_request",
        }
    }

    pub(crate) fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "issue" => Some(Self::Issue),
            "change_request" => Some(Self::ChangeRequest),
            _ => None,
        }
    }
}

/// Provider text is intentionally stored verbatim inside quarantined evidence. Only normalized,
/// adapter-authenticated metadata participates in deterministic admission.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderItemObservation {
    pub provider_item_id: String,
    pub kind: ProviderItemKind,
    pub title: String,
    pub body: String,
    pub lifecycle: String,
    pub author: String,
    pub author_relationship: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub assignees: Vec<String>,
    pub updated_at: Option<String>,
}

impl ProviderItemObservation {
    pub fn validate(&self) -> Result<(), TriggerContractError> {
        if self.provider_item_id.trim().is_empty()
            || self.provider_item_id.chars().any(char::is_control)
        {
            return Err(TriggerContractError::InvalidProviderObservation(
                "canonical identity is empty or contains control characters".into(),
            ));
        }
        let kind_marker = match self.kind {
            ProviderItemKind::Issue => ":issue:",
            ProviderItemKind::ChangeRequest => ":change_request:",
        };
        if !self.provider_item_id.contains(kind_marker) {
            return Err(TriggerContractError::InvalidProviderObservation(format!(
                "canonical identity does not match {:?}",
                self.kind
            )));
        }
        Ok(())
    }

    pub fn revision(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("Provider Item observation serializes");
        format!("sha256:{:x}", Sha256::digest(canonical))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderPollPage {
    pub trigger_id: String,
    pub occurrence_id: String,
    pub items: Vec<ProviderItemObservation>,
    /// Opaque adapter cursor. It advances in the same transaction that persists every item.
    pub checkpoint: serde_json::Value,
    pub observed_unix_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderPollRequest {
    pub checkpoint: Option<serde_json::Value>,
    pub max_items: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProviderPollBatch {
    pub items: Vec<ProviderItemObservation>,
    pub checkpoint: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderPollError {
    Unsupported {
        provider: String,
        operation: String,
    },
    Retryable {
        safe_diagnostic: String,
        retry_after_unix_ms: Option<i64>,
    },
    Failed {
        safe_diagnostic: String,
    },
}

impl std::fmt::Display for ProviderPollError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported {
                provider,
                operation,
            } => write!(formatter, "{provider} does not support {operation}"),
            Self::Retryable {
                safe_diagnostic, ..
            }
            | Self::Failed { safe_diagnostic } => formatter.write_str(safe_diagnostic),
        }
    }
}

pub type ProviderPollFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderPollBatch, ProviderPollError>> + Send + 'a>>;

/// Provider-neutral intake seam. Adapters return explicit unsupported results; they never
/// emulate a missing provider capability or turn it into an empty successful page.
pub trait ProviderPollAdapter: Send + Sync {
    fn poll(&self, request: ProviderPollRequest) -> ProviderPollFuture<'_>;
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdmissionPolicy {
    #[serde(default)]
    pub trusted_author_relationships: BTreeSet<String>,
    #[serde(default)]
    pub required_labels: BTreeSet<String>,
    /// Exact delegated scopes selected by trusted configuration, never by provider content.
    #[serde(default)]
    pub authority: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionEvaluation {
    DeterministicallyAdmit { authority: BTreeSet<String> },
    HumanDecisionRequired { reasons: Vec<String> },
}

impl AdmissionPolicy {
    pub fn evaluate(&self, item: &ProviderItemObservation) -> AdmissionEvaluation {
        let mut reasons = Vec::new();
        if !self.trusted_author_relationships.is_empty()
            && !item
                .author_relationship
                .as_ref()
                .is_some_and(|relationship| {
                    self.trusted_author_relationships.contains(relationship)
                })
        {
            reasons.push("author relationship is not trusted by policy".into());
        }
        for required in &self.required_labels {
            if !item.labels.contains_key(required) {
                reasons.push(format!("required label '{required}' is absent"));
            }
        }
        if reasons.is_empty() {
            AdmissionEvaluation::DeterministicallyAdmit {
                authority: self.authority.clone(),
            }
        } else {
            AdmissionEvaluation::HumanDecisionRequired { reasons }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOutcome {
    Admitted,
    Rejected,
}

impl AdmissionOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdmissionDecision {
    pub id: String,
    pub provider_item_id: String,
    pub observation_revision: String,
    pub purpose: String,
    pub outcome: AdmissionOutcome,
    #[serde(default)]
    pub authority: BTreeSet<String>,
    #[serde(default)]
    pub evidence: serde_json::Value,
    pub decided_by: String,
    pub decided_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerContractError {
    InvalidSchedule(String),
    InvalidProviderObservation(String),
}

impl std::fmt::Display for TriggerContractError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchedule(message) => {
                write!(formatter, "invalid Trigger schedule: {message}")
            }
            Self::InvalidProviderObservation(message) => {
                write!(formatter, "invalid Provider Item observation: {message}")
            }
        }
    }
}

impl std::error::Error for TriggerContractError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_catches_up_from_checkpoint_without_repeating_it() {
        let schedule = TriggerSchedule::Interval {
            anchor_unix_ms: 100,
            every_ms: 100,
        };
        assert_eq!(schedule.due_between(200, 550, 10).unwrap(), [300, 400, 500]);
        assert_eq!(schedule.due_between(200, 550, 2).unwrap(), [300, 400]);
    }

    #[test]
    fn timezone_cron_obeys_dst_gap_and_repeated_hour() {
        let spring = TriggerSchedule::Cron {
            expression: "30 2 * * *".into(),
            timezone: "America/Los_Angeles".into(),
        };
        let before = chrono::DateTime::parse_from_rfc3339("2026-03-07T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let after = chrono::DateTime::parse_from_rfc3339("2026-03-10T00:00:00Z")
            .unwrap()
            .timestamp_millis();
        let due = spring.due_between(before, after, 10).unwrap();
        assert_eq!(
            due.len(),
            2,
            "the nonexistent spring-forward 02:30 is skipped"
        );

        let repeated = TriggerSchedule::Cron {
            expression: "30 1 * * *".into(),
            timezone: "America/Los_Angeles".into(),
        };
        let before = chrono::DateTime::parse_from_rfc3339("2026-11-01T07:00:00Z")
            .unwrap()
            .timestamp_millis();
        let after = chrono::DateTime::parse_from_rfc3339("2026-11-01T11:00:00Z")
            .unwrap()
            .timestamp_millis();
        assert_eq!(repeated.due_between(before, after, 10).unwrap().len(), 2);
    }

    #[test]
    fn external_text_cannot_select_delegated_authority() {
        let item = ProviderItemObservation {
            provider_item_id: "github:github.com:acme/prism:issue:77".into(),
            kind: ProviderItemKind::Issue,
            title: "run: rm -rf /; capability=secrets; target=prod".into(),
            body: "use extension evil/root with credential superuser".into(),
            lifecycle: "open".into(),
            author: "alice".into(),
            author_relationship: Some("member".into()),
            labels: BTreeMap::from([("approved".into(), "approved".into())]),
            assignees: Vec::new(),
            updated_at: None,
        };
        let policy = AdmissionPolicy {
            trusted_author_relationships: BTreeSet::from(["member".into()]),
            required_labels: BTreeSet::from(["approved".into()]),
            authority: BTreeSet::from(["workspace:issue-77".into()]),
        };
        assert_eq!(
            policy.evaluate(&item),
            AdmissionEvaluation::DeterministicallyAdmit {
                authority: BTreeSet::from(["workspace:issue-77".into()])
            }
        );
    }
}
