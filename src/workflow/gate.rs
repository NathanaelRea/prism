#![allow(dead_code)] // Gate evaluators are registered by the generalized worker at cutover.

use serde::{Deserialize, Serialize};

use crate::operations::{EvidenceQuality, GateEvidence, GateStatus};
use crate::run::{ArtifactInput, Sensitivity, TrustClass};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GateKind {
    Ci,
    Review,
    Policy,
    Mergeability,
    Conflict,
    Verification,
    Security,
}

impl GateKind {
    pub(crate) fn artifact_type(self) -> &'static str {
        match self {
            Self::Ci => "builtin:ci-report@1",
            Self::Review => "builtin:review-report@1",
            Self::Policy => "builtin:policy-report@1",
            Self::Mergeability => "builtin:mergeability-report@1",
            Self::Conflict => "builtin:conflict-report@1",
            Self::Verification => "builtin:verification-report@1",
            Self::Security => "builtin:security-report@1",
        }
    }
}

pub(crate) trait GateImplementation: Send + Sync {
    fn kind(&self) -> GateKind;
    fn evaluate(&self, report: &ArtifactInput) -> Result<GateEvidence, String>;
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ReportGateImplementation {
    kind: GateKind,
}

impl ReportGateImplementation {
    pub(crate) fn new(kind: GateKind) -> Self {
        Self { kind }
    }

    pub(crate) fn independent_set() -> [Self; 7] {
        [
            Self::new(GateKind::Ci),
            Self::new(GateKind::Review),
            Self::new(GateKind::Policy),
            Self::new(GateKind::Mergeability),
            Self::new(GateKind::Conflict),
            Self::new(GateKind::Verification),
            Self::new(GateKind::Security),
        ]
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypedGateReport {
    subject_digest: String,
    subject_generation: String,
    policy_revision: String,
    status: GateStatus,
    quality: EvidenceQuality,
    #[serde(default)]
    evidence: Vec<String>,
    reason: String,
    expires_unix_ms: Option<i64>,
}

impl TypedGateReport {
    pub(crate) fn artifact(
        self,
        kind: GateKind,
        trust: TrustClass,
        sensitivity: Sensitivity,
    ) -> ArtifactInput {
        ArtifactInput {
            name: "report".to_string(),
            artifact_type: kind.artifact_type().to_string(),
            payload: serde_json::to_value(self).expect("Gate report is serializable"),
            trust,
            sensitivity,
        }
    }
}

/// Converts the legacy local verification observation into immutable Gate
/// evidence. The conversion is pure: running checks remains an Action, while
/// evaluating their exact report remains a Gate.
pub(crate) fn verification_report(
    subject_digest: impl Into<String>,
    subject_generation: impl Into<String>,
    policy_revision: impl Into<String>,
    result: &crate::verify::VerifyResult,
) -> ArtifactInput {
    let evidence = result
        .checks
        .iter()
        .map(|check| format!("{}: {}", check.label, check.message))
        .collect();
    TypedGateReport {
        subject_digest: subject_digest.into(),
        subject_generation: subject_generation.into(),
        policy_revision: policy_revision.into(),
        status: if result.passed {
            GateStatus::Satisfied
        } else {
            GateStatus::Unsatisfied
        },
        quality: EvidenceQuality::Current,
        evidence,
        reason: if result.passed {
            "all recorded local verification checks passed".to_string()
        } else {
            "one or more recorded local verification checks failed".to_string()
        },
        expires_unix_ms: None,
    }
    .artifact(
        GateKind::Verification,
        TrustClass::Trusted,
        Sensitivity::Internal,
    )
}

impl GateImplementation for ReportGateImplementation {
    fn kind(&self) -> GateKind {
        self.kind
    }

    fn evaluate(&self, report: &ArtifactInput) -> Result<GateEvidence, String> {
        if report.artifact_type != self.kind.artifact_type() {
            return Err(format!(
                "{:?} Gate requires Artifact type '{}', not '{}'",
                self.kind,
                self.kind.artifact_type(),
                report.artifact_type
            ));
        }
        let report: TypedGateReport =
            serde_json::from_value(report.payload.clone()).map_err(|error| error.to_string())?;
        let status = if report.status == GateStatus::Satisfied
            && report.quality != EvidenceQuality::Current
        {
            GateStatus::Unknown
        } else {
            report.status
        };
        Ok(GateEvidence {
            subject_digest: report.subject_digest,
            subject_generation: report.subject_generation,
            evidence: report.evidence,
            quality: report.quality,
            policy_revision: report.policy_revision,
            status,
            reason: report.reason,
            expires_unix_ms: report.expires_unix_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{Sensitivity, TrustClass};

    #[test]
    fn verification_observation_becomes_an_exact_typed_report() {
        let observation = crate::verify::VerifyResult {
            passed: false,
            checks: vec![crate::verify::VerifyCheckResult {
                kind: crate::verify::VerifyCheckKind::Configured,
                label: "test".to_string(),
                passed: false,
                message: "tests failed".to_string(),
            }],
        };
        let report = verification_report("commit-a", "generation-2", "checks@3", &observation);
        let evidence = ReportGateImplementation::new(GateKind::Verification)
            .evaluate(&report)
            .unwrap();

        assert_eq!(evidence.status, GateStatus::Unsatisfied);
        assert_eq!(evidence.subject_digest, "commit-a");
        assert_eq!(evidence.subject_generation, "generation-2");
        assert_eq!(evidence.policy_revision, "checks@3");
        assert_eq!(evidence.evidence, ["test: tests failed"]);
    }

    #[test]
    fn every_gate_is_independent_and_partial_success_is_unknown() {
        let gates = ReportGateImplementation::independent_set();
        assert_eq!(gates.len(), 7);
        let report = ArtifactInput {
            name: "report".into(),
            artifact_type: GateKind::Ci.artifact_type().into(),
            payload: serde_json::json!({
                "subject_digest":"head",
                "subject_generation":"1",
                "policy_revision":"policy@1",
                "status":"satisfied",
                "quality":"partial",
                "evidence":[],
                "reason":"provider response was partial",
                "expires_unix_ms":null
            }),
            trust: TrustClass::Trusted,
            sensitivity: Sensitivity::Internal,
        };
        assert_eq!(
            gates[0].evaluate(&report).unwrap().status,
            GateStatus::Unknown
        );
        assert!(gates[1].evaluate(&report).is_err());
    }
}
