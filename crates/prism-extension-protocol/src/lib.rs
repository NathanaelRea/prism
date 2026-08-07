//! Public, language-neutral wire values for Prism extension protocol version 1.
//!
//! This crate deliberately contains no Prism runtime, persistence, or provider types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_MINOR: u16 = 0;
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const HOST_FEATURES: [&str; 7] = [
    "host.read_artifact",
    "host.trace_process",
    "host.trace_agent",
    "host.run_process",
    "host.run_agent",
    "host.observe_provider",
    "host.brokered_effects",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Hello {
    pub protocol: ProtocolVersion,
    #[serde(default)]
    pub features: Vec<String>,
    pub host: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HelloAck {
    pub protocol: ProtocolVersion,
    #[serde(default)]
    pub features: Vec<String>,
    pub extension_id: String,
    pub extension_revision: String,
    #[serde(default)]
    pub sdk_version: String,
    #[serde(default)]
    pub package_id: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub executable_digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepClass {
    Action,
    Gate,
    Approval,
    Wait,
    Notification,
    WorkflowCall,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PortDescriptor {
    pub name: String,
    pub schema: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImplementationDescriptor {
    pub id: String,
    pub class: StepClass,
    #[serde(default)]
    pub inputs: Vec<PortDescriptor>,
    #[serde(default)]
    pub outputs: Vec<PortDescriptor>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// Describes which guarantees apply to mutations performed by this implementation.
    /// This is disclosure and policy input, not a sandbox boundary.
    #[serde(default)]
    pub effect_boundary: EffectBoundary,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectBoundary {
    /// The implementation is observational and declares no mutations.
    #[default]
    None,
    /// Every declared protected mutation goes through an intent-first host operation.
    Brokered,
    /// The process may mutate directly and receives no fencing or reconciliation guarantee.
    Unbrokered,
}

impl EffectBoundary {
    pub const fn has_broker_guarantees(self) -> bool {
        matches!(self, Self::Brokered)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ArtifactSchemaDescriptor {
    pub id: String,
    pub schema: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputDescriptor {
    pub schema_id: String,
    pub editor: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RendererDescriptor {
    pub schema_id: String,
    pub renderer: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TriggerDescriptor {
    pub id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotificationChannelDescriptor {
    pub id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ExtensionDescriptor {
    #[serde(default)]
    pub implementations: Vec<ImplementationDescriptor>,
    #[serde(default)]
    pub artifact_schemas: Vec<ArtifactSchemaDescriptor>,
    #[serde(default)]
    pub input_support: Vec<InputDescriptor>,
    #[serde(default)]
    pub renderers: Vec<RendererDescriptor>,
    #[serde(default)]
    pub triggers: Vec<TriggerDescriptor>,
    #[serde(default)]
    pub notification_channels: Vec<NotificationChannelDescriptor>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AttemptEnvelope {
    pub attempt_id: String,
    pub generation: u64,
    pub implementation_id: String,
    pub input: Value,
    #[serde(default)]
    pub artifacts: BTreeMap<String, ArtifactReference>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactReference {
    pub id: String,
    pub revision: u64,
    pub schema: String,
    pub digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecuteResult {
    pub attempt_id: String,
    pub generation: u64,
    pub outcome: AttemptOutcome,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Succeeded { outputs: Value },
    Failed { error: String },
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Diagnostic {
    pub stream: DiagnosticStream,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpaqueReference {
    pub id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessRequest {
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    pub working_scope: OpaqueReference,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentRequest {
    pub harness: String,
    pub model: Option<String>,
    pub prompt: String,
    pub working_scope: OpaqueReference,
    pub continuation: Option<OpaqueReference>,
    pub tool_policy: Value,
    pub timeout_ms: u64,
    pub max_output_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderObservationRequest {
    pub subject: OpaqueReference,
    pub operation: String,
}

/// Exact preconditions common to Standard protected mutations. Opaque references are
/// resolved by Prism; extensions never receive credentials or provider adapter objects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectPreconditions {
    pub repository: OpaqueReference,
    pub worktree_session: Option<OpaqueReference>,
    pub expected_head: Option<String>,
    pub target_repository: Option<OpaqueReference>,
    pub policy_revision: Option<String>,
    #[serde(default)]
    pub gate_revisions: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BrokeredEffectRequest {
    pub effect_id: String,
    pub idempotency_key: String,
    pub authority_scope: String,
    pub preconditions: EffectPreconditions,
    pub parameters: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "operation", content = "arguments", rename_all = "snake_case")]
pub enum HostOperation {
    ReadArtifact { artifact: ArtifactReference },
    TraceProcess { pid: u32, identity: Option<String> },
    TraceAgent { session_id: String, metadata: Value },
    RunProcess { request: ProcessRequest },
    RunAgent { request: AgentRequest },
    ObserveProvider { request: ProviderObservationRequest },
    Commit { request: BrokeredEffectRequest },
    Push { request: BrokeredEffectRequest },
    CreateChangeRequest { request: BrokeredEffectRequest },
    ResolveReviewThreads { request: BrokeredEffectRequest },
    SquashMerge { request: BrokeredEffectRequest },
    DeleteWorktree { request: BrokeredEffectRequest },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RenderedSpan {
    pub text: String,
    #[serde(default)]
    pub style: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StructuredRender {
    #[serde(default)]
    pub spans: Vec<RenderedSpan>,
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "boxing a wire field would leak transport allocation into the public protocol API"
)]
pub enum Message {
    Hello {
        hello: Hello,
    },
    HelloAck {
        hello: HelloAck,
    },
    Describe {
        id: String,
    },
    Description {
        id: String,
        descriptor: ExtensionDescriptor,
    },
    Execute {
        id: String,
        attempt: AttemptEnvelope,
    },
    ExecuteResult {
        id: String,
        result: ExecuteResult,
    },
    Cancel {
        id: String,
        attempt_id: String,
        generation: u64,
    },
    Cancelled {
        id: String,
        attempt_id: String,
        generation: u64,
    },
    HostRequest {
        id: String,
        attempt_id: String,
        generation: u64,
        operation: HostOperation,
    },
    HostResponse {
        id: String,
        result: Result<Value, ProtocolError>,
    },
    InvokeTrigger {
        id: String,
        adapter_id: String,
        input: Value,
    },
    TriggerResult {
        id: String,
        result: Result<Value, ProtocolError>,
    },
    SendNotification {
        id: String,
        channel_id: String,
        notification: Value,
    },
    NotificationResult {
        id: String,
        result: Result<Value, ProtocolError>,
    },
    SuggestInput {
        id: String,
        schema_id: String,
        context: Value,
    },
    InputSuggestions {
        id: String,
        result: Result<Vec<Value>, ProtocolError>,
    },
    ValidateInput {
        id: String,
        schema_id: String,
        value: Value,
    },
    InputValidation {
        id: String,
        result: Result<(), ProtocolError>,
    },
    RenderArtifact {
        id: String,
        schema_id: String,
        value: Value,
        width: u16,
    },
    ArtifactRender {
        id: String,
        result: Result<StructuredRender, ProtocolError>,
    },
    Ping {
        id: String,
    },
    Pong {
        id: String,
    },
    Shutdown {
        id: String,
    },
    ShutdownAck {
        id: String,
    },
    Error {
        id: Option<String>,
        error: ProtocolError,
    },
}

impl Message {
    pub fn correlation_id(&self) -> Option<&str> {
        match self {
            Self::Describe { id }
            | Self::Description { id, .. }
            | Self::Execute { id, .. }
            | Self::ExecuteResult { id, .. }
            | Self::Cancel { id, .. }
            | Self::Cancelled { id, .. }
            | Self::HostRequest { id, .. }
            | Self::HostResponse { id, .. }
            | Self::InvokeTrigger { id, .. }
            | Self::TriggerResult { id, .. }
            | Self::SendNotification { id, .. }
            | Self::NotificationResult { id, .. }
            | Self::SuggestInput { id, .. }
            | Self::InputSuggestions { id, .. }
            | Self::ValidateInput { id, .. }
            | Self::InputValidation { id, .. }
            | Self::RenderArtifact { id, .. }
            | Self::ArtifactRender { id, .. }
            | Self::Ping { id }
            | Self::Pong { id }
            | Self::Shutdown { id }
            | Self::ShutdownAck { id } => Some(id),
            Self::Error { id, .. } => id.as_deref(),
            Self::Hello { .. } | Self::HelloAck { .. } => None,
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::HelloAck { .. } => "hello_ack",
            Self::Describe { .. } => "describe",
            Self::Description { .. } => "description",
            Self::Execute { .. } => "execute",
            Self::ExecuteResult { .. } => "execute_result",
            Self::Cancel { .. } => "cancel",
            Self::Cancelled { .. } => "cancelled",
            Self::HostRequest { .. } => "host_request",
            Self::HostResponse { .. } => "host_response",
            Self::InvokeTrigger { .. } => "invoke_trigger",
            Self::TriggerResult { .. } => "trigger_result",
            Self::SendNotification { .. } => "send_notification",
            Self::NotificationResult { .. } => "notification_result",
            Self::SuggestInput { .. } => "suggest_input",
            Self::InputSuggestions { .. } => "input_suggestions",
            Self::ValidateInput { .. } => "validate_input",
            Self::InputValidation { .. } => "input_validation",
            Self::RenderArtifact { .. } => "render_artifact",
            Self::ArtifactRender { .. } => "artifact_render",
            Self::Ping { .. } => "ping",
            Self::Pong { .. } => "pong",
            Self::Shutdown { .. } => "shutdown",
            Self::ShutdownAck { .. } => "shutdown_ack",
            Self::Error { .. } => "error",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

pub fn encode_frame(message: &Message, maximum: usize) -> Result<Vec<u8>, ProtocolError> {
    let mut frame = serde_json::to_vec(message)
        .map_err(|error| ProtocolError::new("encode_json", error.to_string()))?;
    if frame.len().saturating_add(1) > maximum {
        return Err(ProtocolError::new(
            "oversized_frame",
            format!("protocol frame exceeds {maximum} bytes"),
        ));
    }
    frame.push(b'\n');
    Ok(frame)
}

pub fn decode_frame(frame: &[u8], maximum: usize) -> Result<Message, ProtocolError> {
    if frame.len() > maximum {
        return Err(ProtocolError::new(
            "oversized_frame",
            format!("protocol frame exceeds {maximum} bytes"),
        ));
    }
    let payload = frame.strip_suffix(b"\n").unwrap_or(frame);
    let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
    serde_json::from_slice(payload)
        .map_err(|error| ProtocolError::new("malformed_json", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_hello_is_stable() {
        let message = Message::Hello {
            hello: Hello {
                protocol: ProtocolVersion::CURRENT,
                features: vec!["host.read_artifact".into()],
                host: "prism/0.1.4".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&message).unwrap(),
            include_str!("../tests/fixtures/protocol/hello.json").trim()
        );
    }

    #[test]
    fn unknown_optional_fields_are_compatible() {
        let value = r#"{"type":"hello_ack","hello":{"protocol":{"major":1,"minor":0},"features":["future"],"extension_id":"acme.test/ext","extension_revision":"sha256:00","future_field":true}}"#;
        assert!(matches!(
            serde_json::from_str::<Message>(value).unwrap(),
            Message::HelloAck { .. }
        ));
    }

    #[test]
    fn unknown_message_kinds_are_not_silently_accepted() {
        let error =
            serde_json::from_str::<Message>(r#"{"type":"future_required","id":"x"}"#).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn golden_correlated_execution_is_stable() {
        let execute = Message::Execute {
            id: "call-7".into(),
            attempt: AttemptEnvelope {
                attempt_id: "attempt-1".into(),
                generation: 2,
                implementation_id: "acme.test/echo".into(),
                input: serde_json::json!({"value":"hello"}),
                artifacts: BTreeMap::new(),
            },
        };
        let host = Message::HostRequest {
            id: "host-3".into(),
            attempt_id: "attempt-1".into(),
            generation: 2,
            operation: HostOperation::TraceProcess {
                pid: 42,
                identity: Some("start-9".into()),
            },
        };
        let result = Message::ExecuteResult {
            id: "call-7".into(),
            result: ExecuteResult {
                attempt_id: "attempt-1".into(),
                generation: 2,
                outcome: AttemptOutcome::Succeeded {
                    outputs: serde_json::json!({"echo":"hello"}),
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&execute).unwrap(),
            include_str!("../tests/fixtures/protocol/execute.json").trim()
        );
        assert_eq!(
            serde_json::to_string(&host).unwrap(),
            include_str!("../tests/fixtures/protocol/host-request.json").trim()
        );
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            include_str!("../tests/fixtures/protocol/result.json").trim()
        );
    }
}
